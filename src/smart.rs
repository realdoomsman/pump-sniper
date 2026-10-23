//! Wallet reputation, learned from outcomes.
//!
//! "Follow the smart money" is only worth anything if smart money is measurably
//! distinguishable from luck. A wallet that bought one curve that happened to
//! graduate tells you nothing; the raw hit rate would score it 100%.
//!
//! So a wallet's score is its **lift over the base rate**, shrunk toward zero by a
//! Beta prior. A wallet performing exactly at the base rate scores 0 and
//! contributes nothing. A wallet with few observations also scores near 0, no
//! matter how good its record looks, because the prior dominates until it has
//! earned otherwise.

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use std::collections::{HashMap, HashSet};

/// Strength of the shrinkage prior, in pseudo-observations.
///
/// At 20, a wallet needs roughly that many settled curves before its own record
/// outweighs the assumption that it is average.
const PRIOR_STRENGTH: f64 = 20.0;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WalletStats {
    /// Curves this wallet bought into that have since settled.
    pub settled: u32,
    /// How many of those graduated.
    pub graduated: u32,
}

/// Reputation store. Cheap to read on the hot path, updated as outcomes land.
#[derive(Default)]
pub struct SmartMoney {
    inner: RwLock<Inner>,
}

#[derive(Default, Serialize, Deserialize)]
struct Inner {
    stats: HashMap<String, WalletStats>,
    /// Buyers seen per curve, pending an outcome.
    #[serde(skip)]
    pending: HashMap<String, HashSet<String>>,
    total_settled: u32,
    total_graduated: u32,
}

impl SmartMoney {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `wallet` bought into `curve`. Idempotent per (curve, wallet).
    pub fn observe_buy(&self, curve: &Pubkey, wallet: &Pubkey) {
        let mut g = self.inner.write();
        g.pending
            .entry(curve.to_string())
            .or_default()
            .insert(wallet.to_string());
    }

    /// Resolve a curve. Every wallet seen buying it gets one settled observation.
    ///
    /// Call this only for genuinely settled curves. Marking a curve that is still
    /// climbing as a failure poisons every wallet that was early and right.
    pub fn settle(&self, curve: &Pubkey, graduated: bool) -> usize {
        let mut g = self.inner.write();
        let Some(buyers) = g.pending.remove(&curve.to_string()) else {
            return 0;
        };
        for w in &buyers {
            let e = g.stats.entry(w.clone()).or_default();
            e.settled += 1;
            if graduated {
                e.graduated += 1;
            }
        }
        g.total_settled += 1;
        if graduated {
            g.total_graduated += 1;
        }
        buyers.len()
    }

    /// Fraction of settled curves that graduated. `None` until it means something.
    pub fn base_rate(&self) -> Option<f64> {
        let g = self.inner.read();
        (g.total_settled >= 30).then(|| f64::from(g.total_graduated) / f64::from(g.total_settled))
    }

    /// A wallet's shrunk lift over the base rate.
    ///
    /// Positive means it graduates curves more often than average. Returns 0.0 for
    /// unknown wallets and for wallets performing at the base rate — so an unknown
    /// buyer never moves a trading decision on its own.
    pub fn score(&self, wallet: &Pubkey) -> f64 {
        let g = self.inner.read();
        let Some(base) = (g.total_settled >= 30)
            .then(|| f64::from(g.total_graduated) / f64::from(g.total_settled))
        else {
            return 0.0;
        };
        let Some(s) = g.stats.get(&wallet.to_string()) else {
            return 0.0;
        };
        // Beta posterior mean with the prior centred on the base rate.
        let a = base * PRIOR_STRENGTH + f64::from(s.graduated);
        let b = (1.0 - base) * PRIOR_STRENGTH + f64::from(s.settled - s.graduated);
        let posterior = a / (a + b);
        posterior - base
    }

    /// Combined signal for a curve: the summed lift of everyone seen buying it.
    ///
    /// Summed rather than averaged, because several good wallets arriving is a
    /// stronger signal than one, and a single unknown buyer should not dilute it.
    pub fn curve_score(&self, buyers: &[Pubkey]) -> f64 {
        buyers.iter().map(|w| self.score(w)).sum()
    }

    /// Wallets ranked by lift, for inspection. Only those with real evidence.
    pub fn top(&self, limit: usize, min_settled: u32) -> Vec<(String, WalletStats, f64)> {
        let g = self.inner.read();
        let Some(base) = (g.total_settled >= 30)
            .then(|| f64::from(g.total_graduated) / f64::from(g.total_settled))
        else {
            return Vec::new();
        };
        let mut v: Vec<_> = g
            .stats
            .iter()
            .filter(|(_, s)| s.settled >= min_settled)
            .map(|(w, s)| {
                let a = base * PRIOR_STRENGTH + f64::from(s.graduated);
                let b = (1.0 - base) * PRIOR_STRENGTH + f64::from(s.settled - s.graduated);
                (w.clone(), *s, a / (a + b) - base)
            })
            .collect();
        v.sort_by(|x, y| y.2.partial_cmp(&x.2).unwrap_or(std::cmp::Ordering::Equal));
        v.truncate(limit);
        v
    }

    /// Forget curves that never settled, so the pending map stays bounded.
    pub fn forget_pending(&self, curves: &[Pubkey]) -> usize {
        let mut g = self.inner.write();
        let mut n = 0;
        for c in curves {
            if g.pending.remove(&c.to_string()).is_some() {
                n += 1;
            }
        }
        n
    }

    pub fn pending_curves(&self) -> usize {
        self.inner.read().pending.len()
    }

    pub fn known_wallets(&self) -> usize {
        self.inner.read().stats.len()
    }

    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let g = self.inner.read();
        std::fs::write(path, serde_json::to_string_pretty(&*g)?)?;
        Ok(())
    }

    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let inner: Inner = serde_json::from_str(&raw)?;
        Ok(Self {
            inner: RwLock::new(inner),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Settle `n` curves at the given hit rate to establish a base rate.
    fn seed_base(sm: &SmartMoney, n: u32, graduate_every: u32) {
        for i in 0..n {
            let c = Pubkey::new_unique();
            sm.observe_buy(&c, &Pubkey::new_unique());
            sm.settle(&c, i % graduate_every == 0);
        }
    }

    #[test]
    fn unknown_wallet_scores_zero() {
        let sm = SmartMoney::new();
        seed_base(&sm, 60, 4);
        assert_eq!(sm.score(&Pubkey::new_unique()), 0.0);
    }

    #[test]
    fn no_score_before_a_base_rate_exists() {
        let sm = SmartMoney::new();
        let w = Pubkey::new_unique();
        let c = Pubkey::new_unique();
        sm.observe_buy(&c, &w);
        sm.settle(&c, true);
        assert_eq!(sm.base_rate(), None);
        assert_eq!(sm.score(&w), 0.0, "must not score on one observation");
    }

    #[test]
    fn one_lucky_hit_barely_moves_the_score() {
        let sm = SmartMoney::new();
        seed_base(&sm, 60, 4);
        let lucky = Pubkey::new_unique();
        let c = Pubkey::new_unique();
        sm.observe_buy(&c, &lucky);
        sm.settle(&c, true);
        let s = sm.score(&lucky);
        assert!(s > 0.0, "should be slightly positive, was {s}");
        assert!(s < 0.05, "one hit must not look like skill, was {s}");
    }

    #[test]
    fn a_consistently_good_wallet_earns_real_lift() {
        let sm = SmartMoney::new();
        seed_base(&sm, 120, 6); // base rate ~1/6
        let good = Pubkey::new_unique();
        for _ in 0..60 {
            let c = Pubkey::new_unique();
            sm.observe_buy(&c, &good);
            sm.settle(&c, true);
        }
        let s = sm.score(&good);
        assert!(s > 0.3, "sustained winner scored only {s}");
    }

    #[test]
    fn a_consistently_bad_wallet_scores_negative() {
        let sm = SmartMoney::new();
        seed_base(&sm, 120, 2); // base rate ~1/2
        let bad = Pubkey::new_unique();
        for _ in 0..60 {
            let c = Pubkey::new_unique();
            sm.observe_buy(&c, &bad);
            sm.settle(&c, false);
        }
        assert!(sm.score(&bad) < -0.2);
    }

    #[test]
    fn a_wallet_at_the_base_rate_scores_about_zero() {
        let sm = SmartMoney::new();
        seed_base(&sm, 200, 4); // base 25%
        let avg = Pubkey::new_unique();
        for i in 0..80 {
            let c = Pubkey::new_unique();
            sm.observe_buy(&c, &avg);
            sm.settle(&c, i % 4 == 0);
        }
        assert!(sm.score(&avg).abs() < 0.05, "scored {}", sm.score(&avg));
    }

    #[test]
    fn settle_credits_every_buyer_once() {
        let sm = SmartMoney::new();
        let c = Pubkey::new_unique();
        let w = Pubkey::new_unique();
        sm.observe_buy(&c, &w);
        sm.observe_buy(&c, &w); // duplicate
        sm.observe_buy(&c, &Pubkey::new_unique());
        assert_eq!(sm.settle(&c, true), 2, "duplicate buyer counted twice");
        assert_eq!(sm.settle(&c, true), 0, "second settle should be a no-op");
    }

    #[test]
    fn curve_score_sums_buyer_lift() {
        let sm = SmartMoney::new();
        seed_base(&sm, 120, 6);
        let good = Pubkey::new_unique();
        for _ in 0..60 {
            let c = Pubkey::new_unique();
            sm.observe_buy(&c, &good);
            sm.settle(&c, true);
        }
        let unknown = Pubkey::new_unique();
        let one = sm.curve_score(&[good]);
        let with_unknown = sm.curve_score(&[good, unknown]);
        assert!((one - with_unknown).abs() < 1e-12, "unknown must not dilute");
        assert!(sm.curve_score(&[good, good]) > one);
    }

    #[test]
    fn pending_can_be_forgotten() {
        let sm = SmartMoney::new();
        let c = Pubkey::new_unique();
        sm.observe_buy(&c, &Pubkey::new_unique());
        assert_eq!(sm.pending_curves(), 1);
        assert_eq!(sm.forget_pending(&[c]), 1);
        assert_eq!(sm.pending_curves(), 0);
    }

    #[test]
    fn top_needs_evidence_and_ranks_by_lift() {
        let sm = SmartMoney::new();
        seed_base(&sm, 120, 6);
        let good = Pubkey::new_unique();
        for _ in 0..40 {
            let c = Pubkey::new_unique();
            sm.observe_buy(&c, &good);
            sm.settle(&c, true);
        }
        assert!(sm.top(10, 1000).is_empty(), "min_settled must filter");
        let t = sm.top(5, 10);
        assert!(!t.is_empty());
        assert_eq!(t[0].0, good.to_string());
    }

    #[test]
    fn reputation_round_trips_through_disk() {
        let sm = SmartMoney::new();
        seed_base(&sm, 60, 4);
        let path = std::env::temp_dir().join("sniper_smart_test.json");
        sm.save(&path).unwrap();
        let back = SmartMoney::load(&path).unwrap();
        assert_eq!(back.known_wallets(), sm.known_wallets());
        assert_eq!(back.base_rate(), sm.base_rate());
        let _ = std::fs::remove_file(&path);
    }
}
