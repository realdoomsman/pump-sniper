//! Live registry of bonding curves, plus the demand signal the reference bot lacks.
//!
//! The wallet we reverse-engineered fires purely on curve depth. It has no notion
//! of whether anyone is still *buying*, which is exactly how it paid 15.85 SOL to
//! open a pool that nobody wanted and lost 5.30 SOL in 24 seconds. Every curve
//! here therefore carries a short inflow history so the strategy can ask "is this
//! thing still moving?" before it commits.

use crate::pumpfun::BondingCurve;
use parking_lot::RwLock;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How many reserve samples to retain per curve.
const HISTORY: usize = 32;

#[derive(Debug, Clone, Copy)]
struct Sample {
    at: Instant,
    real_sol: u64,
}

/// One tracked curve.
#[derive(Debug, Clone)]
pub struct TrackedCurve {
    pub mint: Pubkey,
    pub curve_key: Pubkey,
    pub state: BondingCurve,
    pub first_seen: Instant,
    pub last_update: Instant,
    history: Vec<Sample>,
}

impl TrackedCurve {
    fn new(mint: Pubkey, curve_key: Pubkey, state: BondingCurve) -> Self {
        let now = Instant::now();
        Self {
            mint,
            curve_key,
            state,
            first_seen: now,
            last_update: now,
            history: vec![Sample {
                at: now,
                real_sol: state.real_sol_reserves,
            }],
        }
    }

    fn push(&mut self, state: BondingCurve) {
        let now = Instant::now();
        self.state = state;
        self.last_update = now;
        self.history.push(Sample {
            at: now,
            real_sol: state.real_sol_reserves,
        });
        if self.history.len() > HISTORY {
            let excess = self.history.len() - HISTORY;
            self.history.drain(0..excess);
        }
    }

    /// Oldest and newest samples inside the trailing `window`, if there are two.
    fn span(&self, window: Duration) -> Option<(Sample, Sample)> {
        let cutoff = Instant::now().checked_sub(window)?;
        let mut recent = self.history.iter().filter(|s| s.at >= cutoff);
        let first = *recent.next()?;
        let last = *self.history.last()?;
        if last.at <= first.at {
            return None;
        }
        Some((first, last))
    }

    /// Net lamports that flowed into the curve over the trailing `window`.
    ///
    /// Returns `None` when there is not enough history to judge, which callers
    /// must treat as "unknown", never as "zero".
    pub fn inflow(&self, window: Duration) -> Option<i64> {
        let (first, last) = self.span(window)?;
        Some(last.real_sol as i64 - first.real_sol as i64)
    }

    /// Lamports per second of net inflow over the trailing `window`.
    pub fn velocity(&self, window: Duration) -> Option<f64> {
        let (first, last) = self.span(window)?;
        let secs = last.at.duration_since(first.at).as_secs_f64();
        if secs <= f64::EPSILON {
            return None;
        }
        Some((last.real_sol as f64 - first.real_sol as f64) / secs)
    }

    /// How many distinct updates we have seen. A curve that only ever produced one
    /// update is a curve nobody is trading.
    pub fn updates(&self) -> usize {
        self.history.len()
    }

    pub fn age(&self) -> Duration {
        self.first_seen.elapsed()
    }

    pub fn staleness(&self) -> Duration {
        self.last_update.elapsed()
    }
}

/// Thread-safe map of every curve we are watching.
#[derive(Default)]
pub struct Tracker {
    inner: RwLock<HashMap<Pubkey, TrackedCurve>>,
}

impl Tracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or update a curve. Returns the tracked entry after the update.
    pub fn observe(&self, mint: Pubkey, curve_key: Pubkey, state: BondingCurve) -> TrackedCurve {
        let mut g = self.inner.write();
        let entry = g
            .entry(curve_key)
            .or_insert_with(|| TrackedCurve::new(mint, curve_key, state));
        // `or_insert_with` already seeded the first sample; only push on real updates.
        if entry.state != state {
            entry.push(state);
        }
        entry.clone()
    }

    pub fn get(&self, curve_key: &Pubkey) -> Option<TrackedCurve> {
        self.inner.read().get(curve_key).cloned()
    }

    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Curves sorted by how close they are to graduating, nearest first.
    pub fn nearest_to_graduation(&self, limit: usize) -> Vec<TrackedCurve> {
        let g = self.inner.read();
        let mut v: Vec<_> = g
            .values()
            .filter(|c| !c.state.complete)
            .cloned()
            .collect();
        v.sort_by_key(|c| c.state.gap_to_graduation().unwrap_or(u64::MAX));
        v.truncate(limit);
        v
    }

    /// Drop curves that graduated or went quiet, so the map does not grow forever.
    ///
    /// 95% of the launches the reference wallet bid on never received a single
    /// buy; without eviction the map is almost entirely dead weight.
    pub fn evict(&self, max_idle: Duration) -> usize {
        self.evict_keys(max_idle).len()
    }

    /// Same as [`Self::evict`] but returns which curves were dropped, so the
    /// caller can settle them as outcomes rather than silently forgetting them.
    pub fn evict_keys(&self, max_idle: Duration) -> Vec<Pubkey> {
        let mut g = self.inner.write();
        let dropped: Vec<Pubkey> = g
            .values()
            .filter(|c| c.state.complete || c.staleness() >= max_idle)
            .map(|c| c.curve_key)
            .collect();
        for k in &dropped {
            g.remove(k);
        }
        dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pumpfun::{self, INIT_REAL_TOKENS, INIT_VIRTUAL_SOL, INIT_VIRTUAL_TOKENS};

    fn curve(real_sol: u64) -> BondingCurve {
        let mut c = BondingCurve {
            virtual_token_reserves: INIT_VIRTUAL_TOKENS as u64,
            virtual_sol_reserves: INIT_VIRTUAL_SOL as u64,
            real_token_reserves: INIT_REAL_TOKENS as u64,
            real_sol_reserves: 0,
            token_total_supply: 1_000_000_000_000_000,
            complete: false,
            creator: None,
        };
        if real_sol > 0 {
            c.apply_buy(real_sol);
        }
        c
    }

    #[test]
    fn observe_then_get_round_trips() {
        let t = Tracker::new();
        let mint = Pubkey::new_unique();
        let ck = pumpfun::bonding_curve(&mint);
        t.observe(mint, ck, curve(0));
        assert_eq!(t.len(), 1);
        assert!(t.get(&ck).is_some());
    }

    #[test]
    fn inflow_is_unknown_with_a_single_sample() {
        let t = Tracker::new();
        let mint = Pubkey::new_unique();
        let ck = pumpfun::bonding_curve(&mint);
        let c = t.observe(mint, ck, curve(0));
        assert_eq!(c.inflow(Duration::from_secs(30)), None);
    }

    #[test]
    fn inflow_tracks_real_sol_growth() {
        let t = Tracker::new();
        let mint = Pubkey::new_unique();
        let ck = pumpfun::bonding_curve(&mint);
        t.observe(mint, ck, curve(0));
        let c = t.observe(mint, ck, curve(5_000_000_000));
        let flow = c.inflow(Duration::from_secs(30)).expect("two samples");
        assert!(flow > 4_900_000_000, "inflow was {flow}");
    }

    #[test]
    fn identical_updates_do_not_create_history() {
        let t = Tracker::new();
        let mint = Pubkey::new_unique();
        let ck = pumpfun::bonding_curve(&mint);
        t.observe(mint, ck, curve(0));
        let c = t.observe(mint, ck, curve(0));
        assert_eq!(c.updates(), 1);
    }

    #[test]
    fn nearest_to_graduation_orders_by_gap() {
        let t = Tracker::new();
        for sol in [1u64, 80, 40] {
            let mint = Pubkey::new_unique();
            t.observe(
                mint,
                pumpfun::bonding_curve(&mint),
                curve(sol * 1_000_000_000),
            );
        }
        let near = t.nearest_to_graduation(3);
        assert_eq!(near.len(), 3);
        assert!(near[0].state.real_sol_reserves > near[1].state.real_sol_reserves);
    }

    #[test]
    fn evict_keys_reports_what_it_dropped() {
        let t = Tracker::new();
        let mint = Pubkey::new_unique();
        let ck = pumpfun::bonding_curve(&mint);
        t.observe(mint, ck, curve(0));
        let mut done = curve(0);
        done.complete = true;
        t.observe(mint, ck, done);
        let dropped = t.evict_keys(Duration::from_secs(3600));
        assert_eq!(dropped, vec![ck]);
        assert!(t.is_empty());
    }

    #[test]
    fn evict_removes_completed_curves() {
        let t = Tracker::new();
        let mint = Pubkey::new_unique();
        let ck = pumpfun::bonding_curve(&mint);
        t.observe(mint, ck, curve(0));
        let mut done = curve(0);
        done.complete = true;
        t.observe(mint, ck, done);
        assert_eq!(t.evict(Duration::from_secs(3600)), 1);
        assert!(t.is_empty());
    }
}
