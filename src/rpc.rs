//! Thin async JSON-RPC client. Only the calls this bot actually needs.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use std::time::Duration;

#[derive(Clone)]
pub struct RpcClient {
    http: reqwest::Client,
    url: String,
}

/// One account as returned by the RPC, already base64-decoded.
#[derive(Debug, Clone)]
pub struct Account {
    pub pubkey: Pubkey,
    pub lamports: u64,
    pub owner: Pubkey,
    pub data: Vec<u8>,
}

#[derive(Deserialize)]
struct RpcEnvelope {
    result: Option<Value>,
    error: Option<Value>,
}

impl RpcClient {
    pub fn new(url: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
            url: url.into(),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
        let resp: RpcEnvelope = self
            .http
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("{method} request failed"))?
            .json()
            .await
            .with_context(|| format!("{method} returned non-JSON"))?;
        if let Some(e) = resp.error {
            bail!("{method} error: {e}");
        }
        resp.result.ok_or_else(|| anyhow!("{method}: empty result"))
    }

    pub async fn latest_blockhash(&self) -> Result<Hash> {
        let v = self
            .call(
                "getLatestBlockhash",
                json!([{"commitment":"processed"}]),
            )
            .await?;
        let s = v["value"]["blockhash"]
            .as_str()
            .ok_or_else(|| anyhow!("missing blockhash"))?;
        Hash::from_str(s).context("parsing blockhash")
    }

    /// Current slot — a lookup table is derived from the slot it was created in.
    pub async fn slot(&self) -> Result<u64> {
        let v = self.call("getSlot", json!([])).await?;
        v.as_u64().ok_or_else(|| anyhow!("getSlot: not a number"))
    }

    pub async fn balance(&self, who: &Pubkey) -> Result<u64> {
        let v = self.call("getBalance", json!([who.to_string()])).await?;
        v["value"]
            .as_u64()
            .ok_or_else(|| anyhow!("missing balance value"))
    }

    /// Fetch up to 100 accounts in one round trip. Missing accounts are dropped.
    pub async fn multiple_accounts(&self, keys: &[Pubkey]) -> Result<Vec<Account>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        if keys.len() > 100 {
            bail!("getMultipleAccounts accepts at most 100 keys, got {}", keys.len());
        }
        let strs: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
        let v = self
            .call(
                "getMultipleAccounts",
                json!([strs, {"encoding":"base64","commitment":"processed"}]),
            )
            .await?;
        let arr = v["value"]
            .as_array()
            .ok_or_else(|| anyhow!("getMultipleAccounts: value not an array"))?;
        let mut out = Vec::new();
        for (k, item) in keys.iter().zip(arr) {
            if item.is_null() {
                continue;
            }
            out.push(decode_account(*k, item)?);
        }
        Ok(out)
    }

    /// Submit a base64 transaction. `skip_preflight` is what you want in a race.
    pub async fn send_transaction(&self, b64: &str, skip_preflight: bool) -> Result<String> {
        let v = self
            .call(
                "sendTransaction",
                json!([b64, {
                    "encoding":"base64",
                    "skipPreflight": skip_preflight,
                    "maxRetries": 0
                }]),
            )
            .await?;
        v.as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("sendTransaction: no signature returned"))
    }
}

/// What the cluster knows about a submitted transaction.
///
/// The distinction that matters is `Unknown` vs `Landed`. `sendTransaction`
/// returns the node's echo of the signature, which proves only that the bytes
/// parsed and were forwarded. For a bot that skips preflight and sets
/// `maxRetries: 0`, most submissions never land at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Landed {
    /// Not seen on chain yet. May still land, may never.
    Unknown,
    /// Landed and succeeded.
    Ok,
    /// Landed and reverted.
    Failed,
}

impl RpcClient {
    /// One `getSignatureStatuses` lookup.
    ///
    /// `searchTransactionHistory` stays false: we only ever ask about signatures we
    /// submitted seconds ago, and the history search is expensive enough that
    /// providers rate-limit it.
    pub async fn signature_status(&self, sig: &str) -> Result<Landed> {
        let v = self
            .call(
                "getSignatureStatuses",
                json!([[sig], {"searchTransactionHistory": false}]),
            )
            .await?;
        let item = &v["value"][0];
        if item.is_null() {
            return Ok(Landed::Unknown);
        }
        if !item["err"].is_null() {
            return Ok(Landed::Failed);
        }
        Ok(Landed::Ok)
    }

    /// Poll until the transaction lands, reverts, or we run out of patience.
    ///
    /// Returns `Unknown` on timeout — which is a real answer, not an error. A
    /// completion that has not landed within a few slots has lost the race, and the
    /// correct response is to release the reservation, not to keep waiting.
    pub async fn confirm(&self, sig: &str, timeout: Duration) -> Result<Landed> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match self.signature_status(sig).await {
                Ok(Landed::Unknown) => {}
                Ok(other) => return Ok(other),
                // A transient RPC error must not be read as "it failed".
                Err(e) if std::time::Instant::now() >= deadline => return Err(e),
                Err(_) => {}
            }
            if std::time::Instant::now() >= deadline {
                return Ok(Landed::Unknown);
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    }
}

/// What a confirmed transaction actually moved, from the owner's point of view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Realised {
    /// Net native lamport change for the owner, with the transaction fee added
    /// back so this measures the trade rather than the cost of sending it.
    pub sol_delta: i64,
    /// Net change in the owner's wrapped-SOL token balance.
    pub wsol_delta: i64,
    /// Fee actually paid.
    pub fee: u64,
}

impl Realised {
    /// Total quote value received. A curve sell pays in native SOL; a PumpSwap
    /// sell pays into the WSOL account. Summing covers both without the caller
    /// needing to know which venue ran.
    pub fn proceeds(&self) -> u64 {
        (self.sol_delta.max(0) as u64).saturating_add(self.wsol_delta.max(0) as u64)
    }
}

impl RpcClient {
    /// Read what a landed transaction actually did to the owner's balances.
    ///
    /// This exists because the exit path used to book a *quote* as though it were a
    /// fill, in live mode as well as paper. Every reported P&L number was therefore
    /// a restatement of our own price model, and no amount of slippage, partial
    /// fill, or fee could ever show up in it.
    pub async fn realised(&self, sig: &str, owner: &Pubkey, wsol_mint: &Pubkey) -> Result<Realised> {
        let v = self
            .call(
                "getTransaction",
                json!([sig, {
                    "encoding": "jsonParsed",
                    "maxSupportedTransactionVersion": 0,
                    "commitment": "confirmed"
                }]),
            )
            .await?;
        if v.is_null() {
            bail!("transaction {sig} not found");
        }
        let meta = &v["meta"];
        let fee = meta["fee"].as_u64().unwrap_or(0);

        // Native lamports: find the owner in the (possibly LUT-expanded) key list.
        let owner_s = owner.to_string();
        let keys = v["transaction"]["message"]["accountKeys"]
            .as_array()
            .ok_or_else(|| anyhow!("no accountKeys"))?;
        let idx = keys.iter().position(|k| {
            k.as_str() == Some(owner_s.as_str()) || k["pubkey"].as_str() == Some(owner_s.as_str())
        });
        let mut sol_delta = 0i64;
        if let Some(i) = idx {
            let pre = meta["preBalances"][i].as_u64().unwrap_or(0) as i128;
            let post = meta["postBalances"][i].as_u64().unwrap_or(0) as i128;
            // Add the fee back: it is the cost of sending, not part of the trade.
            sol_delta = (post - pre + fee as i128).clamp(i64::MIN as i128, i64::MAX as i128) as i64;
        }

        // Wrapped SOL, summed over any token account the owner holds for that mint.
        let wsol_s = wsol_mint.to_string();
        let sum = |field: &str| -> i128 {
            meta[field]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter(|b| {
                            b["owner"].as_str() == Some(owner_s.as_str())
                                && b["mint"].as_str() == Some(wsol_s.as_str())
                        })
                        .filter_map(|b| b["uiTokenAmount"]["amount"].as_str())
                        .filter_map(|s| s.parse::<i128>().ok())
                        .sum()
                })
                .unwrap_or(0)
        };
        let wsol_delta = (sum("postTokenBalances") - sum("preTokenBalances"))
            .clamp(i64::MIN as i128, i64::MAX as i128) as i64;

        Ok(Realised {
            sol_delta,
            wsol_delta,
            fee,
        })
    }
}

fn decode_account(pubkey: Pubkey, item: &Value) -> Result<Account> {
    let data = item["data"][0]
        .as_str()
        .ok_or_else(|| anyhow!("account data not base64"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .context("decoding account data")?;
    let owner = item["owner"]
        .as_str()
        .ok_or_else(|| anyhow!("account missing owner"))?;
    Ok(Account {
        pubkey,
        lamports: item["lamports"].as_u64().unwrap_or(0),
        owner: Pubkey::from_str(owner).context("parsing owner")?,
        data: bytes,
    })
}

/// Submit a bundle to a Jito block engine. Returns the bundle id.
///
/// The point of this path is that a bundle is atomic: if the buy would fail, the
/// bundle is dropped and the tip is never paid. That is the free-option property
/// the reference wallet needed a custom on-chain program to get.
pub async fn send_bundle(
    http: &reqwest::Client,
    block_engine: &str,
    txs_b64: &[String],
) -> Result<String> {
    if txs_b64.is_empty() || txs_b64.len() > 5 {
        bail!("a bundle must contain 1-5 transactions, got {}", txs_b64.len());
    }
    let url = format!("{}/api/v1/bundles", block_engine.trim_end_matches('/'));
    let body = json!({
        "jsonrpc":"2.0","id":1,"method":"sendBundle",
        "params":[txs_b64, {"encoding":"base64"}]
    });
    let resp: RpcEnvelope = http
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("bundle submission failed")?
        .json()
        .await
        .context("bundle response was not JSON")?;
    if let Some(e) = resp.error {
        bail!("sendBundle error: {e}");
    }
    resp.result
        .and_then(|v| v.as_str().map(str::to_string))
        .ok_or_else(|| anyhow!("sendBundle: no bundle id"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_builds() {
        assert!(RpcClient::new("https://example.invalid").is_ok());
    }

    #[test]
    fn decode_account_reads_base64_and_owner() {
        let owner = Pubkey::new_unique();
        let raw = vec![1u8, 2, 3, 4];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
        let item = json!({
            "data":[b64,"base64"],
            "owner": owner.to_string(),
            "lamports": 4242u64
        });
        let key = Pubkey::new_unique();
        let a = decode_account(key, &item).unwrap();
        assert_eq!(a.data, raw);
        assert_eq!(a.owner, owner);
        assert_eq!(a.lamports, 4242);
        assert_eq!(a.pubkey, key);
    }

    #[test]
    fn decode_account_rejects_missing_data() {
        let item = json!({"owner": Pubkey::new_unique().to_string(), "lamports": 1});
        assert!(decode_account(Pubkey::new_unique(), &item).is_err());
    }

    #[tokio::test]
    async fn multiple_accounts_refuses_oversized_batches() {
        let c = RpcClient::new("https://example.invalid").unwrap();
        let keys: Vec<Pubkey> = (0..101).map(|_| Pubkey::new_unique()).collect();
        assert!(c.multiple_accounts(&keys).await.is_err());
    }

    #[tokio::test]
    async fn multiple_accounts_short_circuits_on_empty() {
        let c = RpcClient::new("https://example.invalid").unwrap();
        assert!(c.multiple_accounts(&[]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn bundle_size_is_enforced() {
        let http = reqwest::Client::new();
        assert!(send_bundle(&http, "https://x.invalid", &[]).await.is_err());
        let six: Vec<String> = (0..6).map(|_| "AA".to_string()).collect();
        assert!(send_bundle(&http, "https://x.invalid", &six).await.is_err());
    }
}
