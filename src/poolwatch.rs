//! Watch what happens to a PumpSwap pool in the seconds after it is created.
//!
//! ## Why this exists
//!
//! Everything else in this crate stops at the moment of migration. The curve feed
//! watches bonding-curve accounts, so the instant a coin graduates it disappears
//! from view — and that is exactly the moment the strategy's whole payoff is
//! decided. Two hundred thousand recorded curve updates contain zero post-migration
//! prints.
//!
//! That blind spot is how a phantom number survived for so long. The pool "opening
//! discount" was computed from constants rather than observed, and it described a
//! pool configuration that has never existed on mainnet. No amount of further
//! offline analysis could have caught it; only looking could.
//!
//! ## The question this answers
//!
//! A completion is roughly flat on its own tokens. Any profit has to come from
//! selling into demand that arrives *after* the pool opens. So:
//!
//! > How much quote does a fresh pool actually take in, in its first minute?
//!
//! If the answer is "a few SOL", the play works. If it is "nothing", it does not,
//! and no amount of latency tuning will change that.
//!
//! ## How it measures
//!
//! Deliberately **not** by decoding PumpSwap's `BuyEvent`. That event has been seen
//! exactly once, its layout is unverified, and guessing it would reintroduce the
//! same failure mode this module exists to close: a plausible number from an
//! unchecked assumption.
//!
//! Instead it watches the pool's quote token account, which is an ordinary SPL
//! token account whose `amount` sits at offset 64 and cannot be misread. Every
//! increase is somebody buying; every decrease is somebody selling. The layout risk
//! is zero and the interpretation is unambiguous.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

use crate::pumpswap;

/// `sha256("event:CreatePoolEvent")[..8]`, seen on 82 of 82 migrations.
pub const CREATE_POOL_DISC: [u8; 8] = [0xb1, 0x31, 0x0c, 0xd2, 0xa0, 0x76, 0xa7, 0x74];
/// `sha256("event:InitBoostEvent")[..8]`, seen on 48 of 48.
pub const INIT_BOOST_DISC: [u8; 8] = [0xae, 0x7c, 0x4a, 0xf9, 0x04, 0x51, 0xf6, 0x11];

/// Offset of `amount` in an SPL token account.
const SPL_AMOUNT_OFFSET: usize = 64;

/// Fraction of the migrated quote that `InitBoost` withdraws, in basis points.
///
/// Measured at exactly 20.690000% on 46 of 51 full migrations (the rest differed
/// only by rounding). This is why `CreatePoolEvent`'s `pool_quote_amount` is not
/// the pool's opening reserve: the withdrawal happens later in the same
/// transaction, and using the event value overstates the price by ~1.26x.
const BOOST_SHARE_BPS: u128 = 2069;

/// A pool being born, decoded from `CreatePoolEvent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreatePool {
    pub timestamp: i64,
    pub index: u16,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub quote_decimals: u8,
    pub base_amount_in: u64,
    /// What the curve raised. **Not** the pool's opening reserve when
    /// [`Self::full_migration`] is set — see [`Self::opening_quote`].
    pub quote_amount_in: u64,
    pub pool: Pubkey,
    pub coin_creator: Pubkey,
    /// Byte 333 == 0, which coincided exactly with a classic full-size migration
    /// and with `InitBoost` running later in the same transaction.
    pub full_migration: bool,
}

impl CreatePool {
    pub fn decode(body: &[u8]) -> Result<Self> {
        if body.len() < 334 {
            bail!("CreatePoolEvent is {} bytes, expected >= 334", body.len());
        }
        if body[..8] != CREATE_POOL_DISC {
            bail!("not a CreatePoolEvent");
        }
        let pk = |o: usize| Pubkey::new_from_array(body[o..o + 32].try_into().unwrap());
        let u64at = |o: usize| u64::from_le_bytes(body[o..o + 8].try_into().unwrap());
        Ok(Self {
            timestamp: i64::from_le_bytes(body[8..16].try_into().unwrap()),
            index: u16::from_le_bytes(body[16..18].try_into().unwrap()),
            base_mint: pk(50),
            quote_mint: pk(82),
            quote_decimals: body[115],
            base_amount_in: u64at(116),
            quote_amount_in: u64at(124),
            pool: pk(173),
            coin_creator: pk(301),
            full_migration: body[333] == 0,
        })
    }

    /// Quote the pool will actually hold once the transaction finishes.
    ///
    /// Prefer the `InitBoostEvent` figure when you have it; this is the fallback.
    pub fn opening_quote(&self) -> u64 {
        if self.full_migration {
            ((self.quote_amount_in as u128 * (10_000 - BOOST_SHARE_BPS)) / 10_000) as u64
        } else {
            self.quote_amount_in
        }
    }
}

/// The boost withdrawal, decoded from `InitBoostEvent`.
///
/// This single 136-byte record carries the mint, the dying curve, the new pool and
/// the true funded quote, with no `getAccountInfo` round trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitBoost {
    pub timestamp: i64,
    pub base_mint: Pubkey,
    pub bonding_curve: Pubkey,
    pub pool: Pubkey,
    /// Quote moved to the boost vault. Equals `boost_quote` in the pool struct.
    pub boost_share: u64,
    /// Quote left in the pool's token account. The real opening reserve.
    pub remainder: u64,
}

impl InitBoost {
    pub fn decode(body: &[u8]) -> Result<Self> {
        if body.len() < 136 {
            bail!("InitBoostEvent is {} bytes, expected >= 136", body.len());
        }
        if body[..8] != INIT_BOOST_DISC {
            bail!("not an InitBoostEvent");
        }
        let pk = |o: usize| Pubkey::new_from_array(body[o..o + 32].try_into().unwrap());
        let u64at = |o: usize| u64::from_le_bytes(body[o..o + 8].try_into().unwrap());
        // Field at 120 was zero in 48/48; it may be the high half of a u128 at 112.
        // Decoding as two u64 and asserting the high half is zero keeps us honest
        // either way.
        if u64at(120) != 0 {
            bail!("InitBoostEvent field@120 was non-zero; layout may have changed");
        }
        Ok(Self {
            timestamp: i64::from_le_bytes(body[8..16].try_into().unwrap()),
            base_mint: pk(16),
            bonding_curve: pk(48),
            pool: pk(80),
            boost_share: u64at(112),
            remainder: u64at(128),
        })
    }
}

/// Anything from a PumpSwap log frame that we can decode with confidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolEvent {
    Created(CreatePool),
    Boosted(InitBoost),
}

/// Strip Anchor's self-CPI marker if present, leaving discriminator + fields.
///
/// `Program data:` log lines carry the event discriminator at byte 0 and no
/// marker; inner-instruction blobs carry the marker first. One tolerant strip
/// handles both. Skipping a fixed 16 bytes mis-parses every log event by 8.
fn strip(payload: &[u8]) -> &[u8] {
    if payload.len() >= 16 && payload[..8] == crate::events::CPI_EVENT_PREFIX {
        &payload[8..]
    } else {
        payload
    }
}

/// Pull pool lifecycle events out of one transaction's logs.
///
/// Unknown discriminators fall through silently and on purpose: the set is not
/// closed, and an error here would take the feed down every time PumpSwap ships a
/// new event.
pub fn pool_events_from_logs(logs: &[String]) -> Vec<PoolEvent> {
    let mut out = Vec::new();
    for l in logs {
        let Some(b64) = l.strip_prefix("Program data: ") else {
            continue;
        };
        let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(b64) else {
            continue;
        };
        let body = strip(&raw);
        if body.len() < 8 {
            continue;
        }
        if body[..8] == CREATE_POOL_DISC {
            if let Ok(e) = CreatePool::decode(body) {
                out.push(PoolEvent::Created(e));
            }
        } else if body[..8] == INIT_BOOST_DISC {
            if let Ok(e) = InitBoost::decode(body) {
                out.push(PoolEvent::Boosted(e));
            }
        }
    }
    out
}

/// One pool, watched from birth for a fixed window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub pool: String,
    pub base_mint: String,
    pub created_ts: i64,
    /// What the curve raised.
    pub quote_raised: u64,
    /// Virtual quote credited back as `boost_quote`.
    pub boost_quote: u64,
    /// Real quote in the pool's token account at open.
    pub opening_quote: u64,
    pub full_migration: bool,
    /// `(ms since first sample, quote reserve)`.
    pub samples: Vec<(u64, u64)>,
    /// Sum of every increase — this is other people's buying, and it is the
    /// number the whole strategy depends on.
    pub gross_inflow: u64,
    /// Sum of every decrease.
    pub gross_outflow: u64,
    /// Last minus first.
    pub net_flow: i64,
}

impl Observation {
    fn summarise(&mut self) {
        let (mut up, mut down) = (0u64, 0u64);
        for w in self.samples.windows(2) {
            let (a, b) = (w[0].1, w[1].1);
            if b > a {
                up += b - a;
            } else {
                down += a - b;
            }
        }
        self.gross_inflow = up;
        self.gross_outflow = down;
        self.net_flow = match (self.samples.first(), self.samples.last()) {
            (Some(f), Some(l)) => l.1 as i64 - f.1 as i64,
            _ => 0,
        };
    }
}

/// Read an SPL token account's `amount`.
fn spl_amount(data: &[u8]) -> Option<u64> {
    if data.len() < SPL_AMOUNT_OFFSET + 8 {
        return None;
    }
    Some(u64::from_le_bytes(
        data[SPL_AMOUNT_OFFSET..SPL_AMOUNT_OFFSET + 8]
            .try_into()
            .ok()?,
    ))
}

struct Watch {
    obs: Observation,
    started: Instant,
    sub_id: Option<u64>,
}

/// `logsSubscribe` on the PumpSwap program: where pool creations announce
/// themselves.
pub fn pumpswap_logs_subscribe_request(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "logsSubscribe",
        "params": [
            { "mentions": [ pumpswap::PROGRAM.to_string() ] },
            { "commitment": "confirmed" }
        ]
    })
}

/// `accountSubscribe` on one pool's quote token account.
pub fn quote_account_subscribe_request(id: u64, account: &Pubkey) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "accountSubscribe",
        "params": [
            account.to_string(),
            { "encoding": "base64", "commitment": "confirmed" }
        ]
    })
}

/// Stream fresh pools and record their first `window` of quote-reserve movement.
///
/// Appends one JSON object per pool to `out_path` as each window closes, so a run
/// that is interrupted still leaves everything it finished.
pub async fn run(ws_url: &str, out_path: &str, window: Duration) -> Result<()> {
    let (mut socket, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .with_context(|| format!("connecting to {ws_url}"))?;
    socket
        .send(Message::Text(
            pumpswap_logs_subscribe_request(1).to_string(),
        ))
        .await
        .context("sending logsSubscribe")?;

    // request id -> pool, for matching subscription confirmations.
    let mut pending: HashMap<u64, Pubkey> = HashMap::new();
    // subscription id -> pool, for routing account notifications.
    let mut routes: HashMap<u64, Pubkey> = HashMap::new();
    // quote token account -> pool.
    let mut watching: HashMap<Pubkey, Watch> = HashMap::new();
    let mut next_id = 100u64;
    let mut closed = 0usize;
    let mut report = Instant::now();
    // Counters, because "no pools" has two very different causes: a quiet market,
    // or a subscription the node accepted and never feeds. This project has already
    // lost 45 minutes to the second one looking exactly like the first.
    let (mut frames, mut log_frames, mut seen_pools) = (0u64, 0u64, 0u64);

    while let Some(frame) = socket.next().await {
        let frame = frame.context("websocket read failed")?;
        let Message::Text(text) = frame else { continue };
        crate::feed::check_rpc_error(&text)?;
        frames += 1;
        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Subscription confirmation: {"result": <sub_id>, "id": <req_id>}
        if let (Some(rid), Some(sub)) = (v["id"].as_u64(), v["result"].as_u64()) {
            if let Some(acct) = pending.remove(&rid) {
                routes.insert(sub, acct);
                if let Some(w) = watching.get_mut(&acct) {
                    w.sub_id = Some(sub);
                }
            }
            continue;
        }

        match v["method"].as_str() {
            Some("logsNotification") => {
                let val = &v["params"]["result"]["value"];
                if !val["err"].is_null() {
                    continue;
                }
                log_frames += 1;
                let Some(arr) = val["logs"].as_array() else {
                    continue;
                };
                let logs: Vec<String> = arr
                    .iter()
                    .filter_map(|l| l.as_str().map(str::to_string))
                    .collect();

                // A migration emits CreatePool then InitBoost in the same
                // transaction, so both are in this one frame.
                let evs = pool_events_from_logs(&logs);
                let boost = evs.iter().find_map(|e| match e {
                    PoolEvent::Boosted(b) => Some(*b),
                    _ => None,
                });
                for e in &evs {
                    let PoolEvent::Created(c) = e else { continue };
                    if c.quote_mint != *crate::pumpfun::WSOL_MINT {
                        continue;
                    }
                    let quote_ta = crate::pumpfun::ata(
                        &c.pool,
                        &c.quote_mint,
                        &crate::pumpfun::TOKEN_PROGRAM,
                    );
                    if watching.contains_key(&quote_ta) {
                        continue;
                    }
                    let opening = boost
                        .filter(|b| b.pool == c.pool)
                        .map(|b| b.remainder)
                        .unwrap_or_else(|| c.opening_quote());
                    let boost_quote = boost
                        .filter(|b| b.pool == c.pool)
                        .map(|b| b.boost_share)
                        .unwrap_or(0);
                    seen_pools += 1;
                    tracing::info!(
                        pool = %c.pool,
                        "new pool: raised {:.4} SOL, opens with {:.4}, boost {:.4}",
                        c.quote_amount_in as f64 / 1e9,
                        opening as f64 / 1e9,
                        boost_quote as f64 / 1e9,
                    );
                    let id = next_id;
                    next_id += 1;
                    pending.insert(id, quote_ta);
                    watching.insert(
                        quote_ta,
                        Watch {
                            obs: Observation {
                                pool: c.pool.to_string(),
                                base_mint: c.base_mint.to_string(),
                                created_ts: c.timestamp,
                                quote_raised: c.quote_amount_in,
                                boost_quote,
                                opening_quote: opening,
                                full_migration: c.full_migration,
                                samples: Vec::new(),
                                gross_inflow: 0,
                                gross_outflow: 0,
                                net_flow: 0,
                            },
                            started: Instant::now(),
                            sub_id: None,
                        },
                    );
                    socket
                        .send(Message::Text(
                            quote_account_subscribe_request(id, &quote_ta).to_string(),
                        ))
                        .await
                        .context("sending accountSubscribe")?;
                }
            }
            Some("accountNotification") => {
                let sub = v["params"]["subscription"].as_u64().unwrap_or(0);
                let Some(acct) = routes.get(&sub).copied() else {
                    continue;
                };
                let Some(b64) = v["params"]["result"]["value"]["data"][0].as_str() else {
                    continue;
                };
                let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) else {
                    continue;
                };
                let Some(amount) = spl_amount(&bytes) else {
                    continue;
                };
                if let Some(w) = watching.get_mut(&acct) {
                    let t = w.started.elapsed().as_millis() as u64;
                    w.obs.samples.push((t, amount));
                }
            }
            _ => {}
        }

        // Close out any window that has expired.
        let expired: Vec<Pubkey> = watching
            .iter()
            .filter(|(_, w)| w.started.elapsed() >= window)
            .map(|(k, _)| *k)
            .collect();
        for acct in expired {
            let Some(mut w) = watching.remove(&acct) else {
                continue;
            };
            w.obs.summarise();
            tracing::info!(
                pool = %w.obs.pool,
                "closed: {} samples, in {:.4} SOL, out {:.4} SOL, net {:+.4} SOL",
                w.obs.samples.len(),
                w.obs.gross_inflow as f64 / 1e9,
                w.obs.gross_outflow as f64 / 1e9,
                w.obs.net_flow as f64 / 1e9,
            );
            append_jsonl(out_path, &w.obs)?;
            closed += 1;
            if let Some(sub) = w.sub_id {
                routes.remove(&sub);
                let id = next_id;
                next_id += 1;
                let _ = socket
                    .send(Message::Text(
                        json!({"jsonrpc":"2.0","id":id,"method":"accountUnsubscribe","params":[sub]})
                            .to_string(),
                    ))
                    .await;
            }
        }

        if report.elapsed() >= Duration::from_secs(30) {
            if log_frames == 0 {
                tracing::warn!(
                    "{frames} frames but ZERO logsNotification - the endpoint \
                     accepted the subscription and is not serving it. A public RPC \
                     usually will not carry logsSubscribe with a mentions filter."
                );
            } else {
                tracing::info!(
                    "{log_frames} log frames, {seen_pools} pools seen, \
                     {} watching, {closed} closed -> {out_path}",
                    watching.len()
                );
            }
            report = Instant::now();
        }
    }
    Err(anyhow!("pool log stream ended after {closed} closed windows"))
}

fn append_jsonl(path: &str, obs: &Observation) -> Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {path}"))?;
    writeln!(f, "{}", serde_json::to_string(obs)?).context("writing observation")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminators_are_the_anchor_event_hashes() {
        // sha256("event:CreatePoolEvent")[..8] and sha256("event:InitBoostEvent")[..8],
        // both observed on mainnet.
        assert_eq!(
            CREATE_POOL_DISC,
            [0xb1, 0x31, 0x0c, 0xd2, 0xa0, 0x76, 0xa7, 0x74]
        );
        assert_eq!(
            INIT_BOOST_DISC,
            [0xae, 0x7c, 0x4a, 0xf9, 0x04, 0x51, 0xf6, 0x11]
        );
    }

    fn create_pool_bytes(quote_in: u64, full: bool) -> Vec<u8> {
        let mut b = vec![0u8; 334];
        b[..8].copy_from_slice(&CREATE_POOL_DISC);
        b[8..16].copy_from_slice(&1_700_000_000i64.to_le_bytes());
        b[115] = 9;
        b[116..124].copy_from_slice(&206_900_000_000_000u64.to_le_bytes());
        b[124..132].copy_from_slice(&quote_in.to_le_bytes());
        b[333] = if full { 0 } else { 1 };
        b
    }

    #[test]
    fn create_pool_decodes_its_fixed_fields() {
        let b = create_pool_bytes(84_990_359_056, true);
        let c = CreatePool::decode(&b).unwrap();
        assert_eq!(c.timestamp, 1_700_000_000);
        assert_eq!(c.quote_decimals, 9);
        assert_eq!(c.base_amount_in, 206_900_000_000_000);
        assert_eq!(c.quote_amount_in, 84_990_359_056);
        assert!(c.full_migration);
    }

    #[test]
    fn a_full_migration_does_not_open_with_what_the_curve_raised() {
        // The trap this module exists to avoid. InitBoost withdraws 20.69% later in
        // the same transaction, so the event's own figure overstates the opening
        // price by about 1.26x.
        let c = CreatePool::decode(&create_pool_bytes(84_990_359_056, true)).unwrap();
        let opening = c.opening_quote();
        assert!(opening < c.quote_amount_in);
        // Lands on the measured 67.4 SOL, not the 85 the curve raised.
        assert!(
            (67_380_000_000..67_430_000_000).contains(&opening),
            "opening_quote was {opening}"
        );
        let overstatement = c.quote_amount_in as f64 / opening as f64;
        assert!((overstatement - 1.2608).abs() < 0.01, "{overstatement}");
    }

    #[test]
    fn a_partial_migration_opens_with_exactly_what_it_raised() {
        let c = CreatePool::decode(&create_pool_bytes(1_964_473_935, false)).unwrap();
        assert_eq!(c.opening_quote(), 1_964_473_935);
    }

    #[test]
    fn create_pool_rejects_a_short_or_foreign_payload() {
        assert!(CreatePool::decode(&[0u8; 100]).is_err());
        let mut b = create_pool_bytes(1, true);
        b[0] ^= 0xff;
        assert!(CreatePool::decode(&b).is_err());
    }

    fn init_boost_bytes(share: u64, remainder: u64) -> Vec<u8> {
        let mut b = vec![0u8; 136];
        b[..8].copy_from_slice(&INIT_BOOST_DISC);
        b[8..16].copy_from_slice(&1_700_000_000i64.to_le_bytes());
        b[112..120].copy_from_slice(&share.to_le_bytes());
        b[128..136].copy_from_slice(&remainder.to_le_bytes());
        b
    }

    #[test]
    fn init_boost_decodes_and_the_halves_sum_to_the_migration() {
        let (share, rem) = (17_584_505_289u64, 67_405_853_767u64);
        let e = InitBoost::decode(&init_boost_bytes(share, rem)).unwrap();
        assert_eq!(e.boost_share, share);
        assert_eq!(e.remainder, rem);
        // The invariant that held on 48 of 48 observations.
        assert_eq!(e.boost_share + e.remainder, 84_990_359_056);
    }

    #[test]
    fn init_boost_refuses_a_layout_it_does_not_recognise() {
        let mut b = init_boost_bytes(1, 2);
        b[120] = 7; // was zero in 48/48; non-zero means the layout moved
        assert!(InitBoost::decode(&b).is_err());
    }

    #[test]
    fn spl_amount_reads_offset_64() {
        let mut a = vec![0u8; 165];
        a[64..72].copy_from_slice(&123_456_789u64.to_le_bytes());
        assert_eq!(spl_amount(&a), Some(123_456_789));
        assert_eq!(spl_amount(&[0u8; 8]), None);
    }

    #[test]
    fn log_scan_ignores_everything_it_cannot_decode() {
        let good = base64::engine::general_purpose::STANDARD
            .encode(init_boost_bytes(1_000, 2_000));
        let logs = vec![
            "Program log: Instruction: Buy".to_string(),
            "Program data: bm90IGV2ZW4gY2xvc2U=".to_string(),
            format!("Program data: {good}"),
        ];
        let evs = pool_events_from_logs(&logs);
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], PoolEvent::Boosted(_)));
    }

    #[test]
    fn observation_summarises_flow_in_both_directions() {
        let mut o = Observation {
            pool: "p".into(),
            base_mint: "m".into(),
            created_ts: 0,
            quote_raised: 0,
            boost_quote: 0,
            opening_quote: 100,
            samples: vec![(0, 100), (10, 130), (20, 115), (30, 160)],
            full_migration: true,
            gross_inflow: 0,
            gross_outflow: 0,
            net_flow: 0,
        };
        o.summarise();
        assert_eq!(o.gross_inflow, 30 + 45);
        assert_eq!(o.gross_outflow, 15);
        assert_eq!(o.net_flow, 60);
    }

    #[test]
    fn subscribe_requests_are_well_formed() {
        let r = pumpswap_logs_subscribe_request(1);
        assert_eq!(r["method"], "logsSubscribe");
        assert_eq!(
            r["params"][0]["mentions"][0].as_str().unwrap(),
            pumpswap::PROGRAM.to_string()
        );
        let k = Pubkey::new_unique();
        let a = quote_account_subscribe_request(9, &k);
        assert_eq!(a["method"], "accountSubscribe");
        assert_eq!(a["id"], 9);
        assert_eq!(a["params"][0].as_str().unwrap(), k.to_string());
    }
}
