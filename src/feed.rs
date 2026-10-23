//! Live curve feed over WebSocket `programSubscribe`.
//!
//! We subscribe to pump.fun's program accounts filtered to the `BondingCurve`
//! layout, so every buy and sell anywhere on the platform arrives as an account
//! update. That is the whole watchlist: there is no candidate selection to do,
//! because the strategy decides on state, not on identity.

use crate::pumpfun::{self, BondingCurve};
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// A decoded curve update.
#[derive(Debug, Clone)]
pub struct CurveUpdate {
    pub curve_key: Pubkey,
    pub state: BondingCurve,
    pub lamports: u64,
}

/// Build a `programSubscribe` request for one bonding-curve account size.
///
/// Filters within a single subscription are ANDed, so a curve feed needs one
/// subscription per live account size — see [`pumpfun::CURVE_ACCOUNT_SIZES`].
pub fn subscribe_request(id: u64, data_size: usize) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "programSubscribe",
        "params": [
            pumpfun::PROGRAM.to_string(),
            {
                "encoding": "base64",
                "commitment": "processed",
                "filters": [ { "dataSize": data_size } ]
            }
        ]
    })
}

/// Subscribe to every pump.fun log, which is where trade events arrive.
///
/// The account feed says a curve moved; only this says who moved it. Smart-money
/// scoring is impossible without it.
pub fn logs_subscribe_request(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "logsSubscribe",
        "params": [
            { "mentions": [ pumpfun::PROGRAM.to_string() ] },
            { "commitment": "processed" }
        ]
    })
}

/// Surface a JSON-RPC error frame as an error.
///
/// Without this a rejected subscription is indistinguishable from a quiet market:
/// the node replies once with `{"error": ...}`, every later frame never arrives,
/// and the feed reports nothing forever. That is exactly how the trade-event feed
/// silently produced zero wallets on its first run.
pub fn check_rpc_error(text: &str) -> Result<()> {
    let v: Value = match serde_json::from_str::<Value>(text) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    if let Some(e) = v.get("error") {
        bail!("subscription rejected by the node: {e}");
    }
    Ok(())
}

/// Extract trade events from one `logsNotification` frame.
///
/// Failed transactions are skipped: a reverted buy did not happen, and crediting
/// its sender with a purchase would corrupt the reputation store.
pub fn parse_logs_notification(text: &str) -> Result<Vec<crate::events::TradeEvent>> {
    check_rpc_error(text)?;
    let v: Value = serde_json::from_str(text).context("frame was not JSON")?;
    if v.get("method").and_then(Value::as_str) != Some("logsNotification") {
        return Ok(Vec::new());
    }
    let val = &v["params"]["result"]["value"];
    if !val["err"].is_null() {
        return Ok(Vec::new());
    }
    let Some(arr) = val["logs"].as_array() else {
        return Ok(Vec::new());
    };
    let logs: Vec<String> = arr
        .iter()
        .filter_map(|l| l.as_str().map(str::to_string))
        .collect();
    Ok(crate::events::from_logs(&logs))
}

/// Connect and stream trade events into `tx` until the socket dies.
pub async fn run_logs(
    ws_url: &str,
    tx: mpsc::Sender<crate::events::TradeEvent>,
) -> Result<()> {
    let (mut socket, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .with_context(|| format!("connecting to {ws_url}"))?;
    socket
        .send(Message::Text(logs_subscribe_request(1).to_string()))
        .await
        .context("sending logsSubscribe")?;

    // Counters, because "no events" has three very different causes: no frames at
    // all (subscription accepted but dead), frames with no decodable events
    // (layout drift), or frames that are all failed transactions.
    let (mut frames, mut events, mut decoded) = (0u64, 0u64, 0u64);
    let mut last_report = std::time::Instant::now();

    while let Some(frame) = socket.next().await {
        let frame = frame.context("websocket read failed")?;
        let Message::Text(text) = frame else { continue };
        // A rejected subscription must kill the feed so the caller reconnects or
        // reports it, never leave it silently listening to nothing.
        check_rpc_error(&text)?;
        frames += 1;
        match parse_logs_notification(&text) {
            Ok(evs) => {
                events += evs.len() as u64;
                for e in evs {
                    decoded += 1;
                    if tx.send(e).await.is_err() {
                        return Ok(());
                    }
                }
            }
            Err(e) => tracing::debug!("skipping log frame: {e}"),
        }
        if last_report.elapsed() >= std::time::Duration::from_secs(20) {
            tracing::info!("log feed: {frames} frames, {events} events, {decoded} forwarded");
            last_report = std::time::Instant::now();
        }
    }
    Err(anyhow!("log stream ended after {frames} frames, {events} events"))
}

/// Parse one WebSocket frame into a curve update, if it is one.
///
/// Returns `Ok(None)` for frames that are not account notifications - the
/// subscription confirmation, pongs, and anything else the node sends.
pub fn parse_notification(text: &str) -> Result<Option<CurveUpdate>> {
    check_rpc_error(text)?;
    let v: Value = serde_json::from_str(text).context("frame was not JSON")?;
    if v.get("method").and_then(Value::as_str) != Some("programNotification") {
        return Ok(None);
    }
    let val = &v["params"]["result"]["value"];
    let key = val["pubkey"]
        .as_str()
        .ok_or_else(|| anyhow!("notification missing pubkey"))?;
    let data_b64 = val["account"]["data"][0]
        .as_str()
        .ok_or_else(|| anyhow!("notification missing base64 data"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_b64)
        .context("decoding curve account")?;
    Ok(Some(CurveUpdate {
        curve_key: Pubkey::from_str(key).context("parsing curve pubkey")?,
        state: BondingCurve::decode(&bytes)?,
        lamports: val["account"]["lamports"].as_u64().unwrap_or(0),
    }))
}

/// Connect and stream curve updates into `tx` until the socket dies.
///
/// The caller is responsible for reconnecting; this returns an error rather than
/// retrying so the backoff policy lives in one place.
pub async fn run(ws_url: &str, tx: mpsc::Sender<CurveUpdate>) -> Result<()> {
    let (mut socket, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .with_context(|| format!("connecting to {ws_url}"))?;
    for (i, size) in pumpfun::CURVE_ACCOUNT_SIZES.iter().enumerate() {
        socket
            .send(Message::Text(
                subscribe_request(i as u64 + 1, *size).to_string(),
            ))
            .await
            .with_context(|| format!("subscribing to {size}-byte curves"))?;
    }

    while let Some(frame) = socket.next().await {
        let frame = frame.context("websocket read failed")?;
        let Message::Text(text) = frame else { continue };
        check_rpc_error(&text)?;
        match parse_notification(&text) {
            Ok(Some(update)) => {
                if tx.send(update).await.is_err() {
                    return Ok(()); // consumer gone; shut down cleanly
                }
            }
            Ok(None) => {}
            Err(e) => tracing::debug!("skipping frame: {e}"),
        }
    }
    Err(anyhow!("websocket stream ended"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pumpfun::{INIT_REAL_TOKENS, INIT_VIRTUAL_SOL, INIT_VIRTUAL_TOKENS};

    fn encoded_curve(real_sol: u64, complete: bool) -> String {
        let mut d = vec![0u8; 151];
        d[8..16].copy_from_slice(&(INIT_VIRTUAL_TOKENS as u64).to_le_bytes());
        d[16..24].copy_from_slice(&(INIT_VIRTUAL_SOL as u64 + real_sol).to_le_bytes());
        d[24..32].copy_from_slice(&(INIT_REAL_TOKENS as u64).to_le_bytes());
        d[32..40].copy_from_slice(&real_sol.to_le_bytes());
        d[40..48].copy_from_slice(&1_000_000_000_000_000u64.to_le_bytes());
        d[48] = complete as u8;
        base64::engine::general_purpose::STANDARD.encode(d)
    }

    fn notification(key: &Pubkey, real_sol: u64) -> String {
        json!({
            "jsonrpc":"2.0",
            "method":"programNotification",
            "params":{"result":{"value":{
                "pubkey": key.to_string(),
                "account":{"data":[encoded_curve(real_sol,false),"base64"],"lamports":123456u64}
            }}}
        })
        .to_string()
    }

    #[test]
    fn subscribe_request_targets_pumpfun_curves() {
        let r = subscribe_request(7, 151);
        assert_eq!(r["method"], "programSubscribe");
        assert_eq!(r["params"][0], pumpfun::PROGRAM.to_string());
        assert_eq!(r["params"][1]["filters"][0]["dataSize"], 151);
    }

    #[test]
    fn every_live_account_size_gets_a_subscription() {
        // Regression: a single hardcoded dataSize of 81 matched no account on
        // mainnet and the feed silently delivered nothing for 45 seconds.
        assert!(pumpfun::CURVE_ACCOUNT_SIZES.contains(&151));
        assert!(pumpfun::CURVE_ACCOUNT_SIZES.contains(&115));
        for size in pumpfun::CURVE_ACCOUNT_SIZES {
            assert_eq!(subscribe_request(1, size)["params"][1]["filters"][0]["dataSize"], size);
        }
    }

    const REAL_BUY_B64: &str = "vdt/007mYe5J2xeifXbK0kNll1XTqPoN8jrpkwAmwkM3PgUyWcRhbxqnb4MCAAAAeMDf1l4aAAABC7ZE1aPAWzIDYZftO2ZYw4Gw47aSGHk/Ual4u/WBSSNfKZRqAAAAAAGE2sYaAAAAAJgSTJH+AAAB2LbKEwAAAAAAAAAAAAAAYIzMHfzpYbQ7d5wZFQWm4tO/RdWk20YYrXbILWF1RTVfAAAAAAAAADPWHAYAAAAAUcftLOqIHumUWDRh11Qf7cmlN54d3cmRGthICSnK+XQeAAAAAAAAALIo7gEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAwAAAGJ1eQAAAAAAAAAAAAAAAAAAAAAAiBMAAAAAAAAZaw4DAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABqnb4MCAAAAAYTaxhoAAAAB2LbKEwAAAA==";

    fn log_frame(err: Value) -> String {
        json!({
            "jsonrpc":"2.0","method":"logsNotification",
            "params":{"result":{"value":{
                "err": err,
                "signature":"sig",
                "logs":[
                    "Program log: Instruction: BuyExactSolIn",
                    format!("Program data: {REAL_BUY_B64}")
                ]
            }}}
        })
        .to_string()
    }

    #[test]
    fn a_subscription_error_frame_is_surfaced() {
        // Regression: the node replies once with an error and then goes silent.
        // Swallowing that made a dead feed look like a quiet market.
        let err = json!({
            "jsonrpc":"2.0","id":1,
            "error":{"code":-32601,"message":"Filter by mentions is not supported"}
        }).to_string();
        assert!(check_rpc_error(&err).is_err());
        assert!(parse_logs_notification(&err).is_err());
        assert!(parse_notification(&err).is_err());
    }

    #[test]
    fn ordinary_frames_pass_the_error_check() {
        assert!(check_rpc_error(&json!({"jsonrpc":"2.0","result":1,"id":1}).to_string()).is_ok());
        assert!(check_rpc_error("not json").is_ok());
    }

    #[test]
    fn logs_subscription_mentions_pumpfun() {
        let r = logs_subscribe_request(3);
        assert_eq!(r["method"], "logsSubscribe");
        assert_eq!(r["params"][0]["mentions"][0], pumpfun::PROGRAM.to_string());
    }

    #[test]
    fn a_successful_log_frame_yields_the_trade() {
        let ev = parse_logs_notification(&log_frame(Value::Null)).unwrap();
        assert_eq!(ev.len(), 1);
        assert!(ev[0].is_buy);
    }

    #[test]
    fn failed_transactions_contribute_no_trades() {
        // A reverted buy did not happen; crediting its sender would corrupt the
        // reputation store, and the reference wallet reverts 99% of the time.
        let ev = parse_logs_notification(&log_frame(json!({"InstructionError": [4, {"Custom": 111}]}))).unwrap();
        assert!(ev.is_empty());
    }

    #[test]
    fn non_log_frames_yield_nothing() {
        let ok = json!({"jsonrpc":"2.0","result":9,"id":1}).to_string();
        assert!(parse_logs_notification(&ok).unwrap().is_empty());
    }

    #[test]
    fn parses_a_real_shaped_notification() {
        let key = Pubkey::new_unique();
        let u = parse_notification(&notification(&key, 42_000_000_000))
            .unwrap()
            .expect("should parse");
        assert_eq!(u.curve_key, key);
        assert_eq!(u.state.real_sol_reserves, 42_000_000_000);
        assert_eq!(u.lamports, 123_456);
    }

    #[test]
    fn ignores_subscription_confirmations() {
        let ok = json!({"jsonrpc":"2.0","result":123,"id":1}).to_string();
        assert!(parse_notification(&ok).unwrap().is_none());
    }

    #[test]
    fn rejects_non_json_frames() {
        assert!(parse_notification("not json at all").is_err());
    }

    #[test]
    fn rejects_notification_with_truncated_account() {
        let short = base64::engine::general_purpose::STANDARD.encode([0u8; 10]);
        let bad = json!({
            "jsonrpc":"2.0","method":"programNotification",
            "params":{"result":{"value":{
                "pubkey": Pubkey::new_unique().to_string(),
                "account":{"data":[short,"base64"],"lamports":1u64}
            }}}
        })
        .to_string();
        assert!(parse_notification(&bad).is_err());
    }
}
