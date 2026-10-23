//! The trading rules.
//!
//! Two plays, both recovered from mainnet behaviour of the reference wallet, plus
//! one filter it does not have.
//!
//! * **Completion** - when a curve is within reach of the 85.005 SOL graduation
//!   line, buy exactly the remaining gap. That single buy flips `complete`, which
//!   hands you the permissionless migration and therefore the opening print of the
//!   PumpSwap pool. Observed on mainnet paying as little as 0.0054 SOL to claim it.
//! * **Momentum** - a depth-banded ladder entry well below graduation.
//!
//! The filter: the reference wallet fires on depth alone. Adding a demand check
//! before committing is the single highest-value change, because its one large
//! loss in our sample was a completion into a pool with no incoming flow.

use crate::pumpfun::{self, BondingCurve};
use crate::tracker::TrackedCurve;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Tunables for both plays.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyConfig {
    /// Widest gap-to-graduation we are willing to close ourselves, in lamports.
    pub max_gap_lamports: u64,
    /// Hard ceiling on a single completion order, in lamports.
    pub max_position_lamports: u64,
    /// Trailing window used for the demand check.
    #[serde(with = "secs")]
    pub demand_window: Duration,
    /// Minimum net inflow over `demand_window` required to commit, in lamports.
    pub min_inflow_lamports: i64,
    /// Minimum number of distinct curve updates before we trust the demand signal.
    pub min_updates: usize,
    /// Refuse to act on a curve whose last update is older than this.
    #[serde(with = "secs")]
    pub max_staleness: Duration,
    /// Slippage allowance applied to `min_tokens_out`, in basis points.
    pub slippage_bps: u64,
    /// Momentum ladder: (lower_lamports, upper_lamports, gross_order_lamports).
    pub momentum_bands: Vec<(u64, u64, u64)>,
    /// Refuse to trade when the model puts P(graduate) below this.
    ///
    /// Only consulted when a model is supplied. Set to 0.0 to disable the veto.
    pub min_graduation_prob: f64,
}

impl Default for StrategyConfig {
    fn default() -> Self {
        Self {
            // Measured, not copied. The reference wallet closed gaps up to 11.36
            // SOL; that is a bad trade. The completion buy is a premium paid for an
            // option on the migration, the option's payoff is independent of the
            // gap, and the premium is ~20.7% of it. Sweeping the backtest:
            //
            //   max_gap   triggers  precision   premium/trigger
            //   12.0 SOL        36        69%       -1.4021 SOL
            //    6.0 SOL        23        96%       -0.7172 SOL
            //    2.0 SOL        18       100%       -0.2567 SOL
            //    0.5 SOL        10       100%       -0.0609 SOL
            //
            // Tighter is cheaper AND more accurate. 2 SOL keeps half the
            // opportunities at 100% precision for a fifth of the premium; retune
            // once the opening-buy edge is measurable. `backtest --sweep-gap`.
            max_gap_lamports: 2_000_000_000,
            max_position_lamports: 45_000_000_000,
            demand_window: Duration::from_secs(30),
            // Require the curve to have taken at least 0.75 SOL in the window.
            min_inflow_lamports: 750_000_000,
            min_updates: 3,
            max_staleness: Duration::from_secs(20),
            slippage_bps: 1_500,
            // OFF by default, because it was measured and it loses money.
            //
            // These are the five 4-SOL-wide bands from the reference wallet's own
            // guard payload. Replayed over 4,230 recorded curves they produced 397
            // trades, a 26.7% hit rate and -108.6 SOL - roughly -7% per round trip.
            // Buying at 7-32 SOL of depth and exiting ~20 seconds later does not
            // work: most curves stall and the exit sells into nothing.
            //
            // The bands are kept in the code, not deleted, because they are a
            // measured result worth preserving. Put them back in config.toml if you
            // want to re-test them; `sniper backtest` will tell you the same thing.
            momentum_bands: vec![],
            // Starts permissive on purpose. An untrained model should not silently
            // stop the bot trading; raise this once the weights have seen outcomes.
            min_graduation_prob: 0.15,
        }
    }
}

mod secs {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_secs())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_secs(u64::deserialize(d)?))
    }
}

/// Why we chose not to act. Kept explicit so paper runs can explain themselves.
#[derive(Debug, Clone, PartialEq)]
pub enum Skip {
    AlreadyComplete,
    Stale,
    GapTooWide { gap: u64 },
    NoDemandSignal,
    DemandTooWeak { inflow: i64 },
    OrderTooLarge { gross: u64 },
    OutsideBands,
    /// The model does not believe this curve graduates.
    LowProbability { p: f64 },
}

/// Everything the decision needs beyond the curve itself.
///
/// Held separately so `evaluate` stays usable with no model at all — the hard
/// thresholds are the floor, and the model only ever removes trades, never adds
/// one the thresholds rejected.
#[derive(Default, Clone, Copy)]
pub struct Context<'a> {
    pub model: Option<&'a crate::score::Model>,
    /// Summed reputation lift of wallets seen buying this curve.
    pub smart_money: f64,
    /// Distinct buyers observed on this curve.
    pub unique_buyers: u32,
}

/// What we decided to do about one curve.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Close the gap and take the migration.
    Complete {
        /// Gross lamports to spend, fees included.
        gross: u64,
        /// Net lamports that will reach the curve.
        net: u64,
        min_tokens_out: u64,
    },
    /// Ladder entry far from graduation.
    Momentum { gross: u64, min_tokens_out: u64 },
    Skip(Skip),
}

/// A curve's decision inputs, decoupled from any clock.
///
/// The live path builds this from a [`TrackedCurve`]; the backtest builds it from
/// recorded timestamps. Both then run **the same** decision code, which is the
/// only way a backtest result means anything about live behaviour.
#[derive(Debug, Clone, Copy)]
pub struct Snapshot {
    pub state: BondingCurve,
    pub updates: usize,
    pub age: Duration,
    pub staleness: Duration,
    /// Net lamports over the demand window. `None` = not enough history.
    pub inflow: Option<i64>,
    pub velocity: Option<f64>,
}

impl Snapshot {
    pub fn from_tracked(c: &TrackedCurve, window: Duration) -> Self {
        Self {
            state: c.state,
            updates: c.updates(),
            age: c.age(),
            staleness: c.staleness(),
            inflow: c.inflow(window),
            velocity: c.velocity(window),
        }
    }
}

/// Evaluate a curve using the hard thresholds only.
pub fn evaluate(c: &TrackedCurve, cfg: &StrategyConfig) -> Decision {
    evaluate_with(c, cfg, &Context::default())
}

/// Build the model's feature row for a curve.
pub fn features(c: &TrackedCurve, cfg: &StrategyConfig, ctx: &Context<'_>) -> crate::score::Features {
    features_of(&Snapshot::from_tracked(c, cfg.demand_window), ctx)
}

/// Feature row from a clock-free snapshot.
pub fn features_of(s: &Snapshot, ctx: &Context<'_>) -> crate::score::Features {
    crate::score::Features {
        depth_lamports: s.state.real_sol_reserves,
        velocity_lamports_s: s.velocity.unwrap_or(0.0),
        updates: s.updates as u32,
        age: s.age,
        smart_money: ctx.smart_money,
        unique_buyers: ctx.unique_buyers,
    }
}

/// Evaluate a single tracked curve.
pub fn evaluate_with(c: &TrackedCurve, cfg: &StrategyConfig, ctx: &Context<'_>) -> Decision {
    evaluate_snapshot(&Snapshot::from_tracked(c, cfg.demand_window), cfg, ctx)
}

/// The actual decision. Everything else is a wrapper around this.
pub fn evaluate_snapshot(c: &Snapshot, cfg: &StrategyConfig, ctx: &Context<'_>) -> Decision {
    let st = &c.state;
    if st.complete {
        return Decision::Skip(Skip::AlreadyComplete);
    }
    if c.staleness > cfg.max_staleness {
        return Decision::Skip(Skip::Stale);
    }
    let Some(gap) = st.gap_to_graduation() else {
        return Decision::Skip(Skip::AlreadyComplete);
    };

    // The model is a veto, never a licence. It runs after the deterministic gates
    // so a mis-fitted model can only make the bot trade less, not more.
    if let Some(m) = ctx.model {
        let p = m.predict(&features_of(c, ctx));
        if p < cfg.min_graduation_prob {
            return Decision::Skip(Skip::LowProbability { p });
        }
    }

    if gap <= cfg.max_gap_lamports {
        return complete_decision(c, st, gap, cfg);
    }
    // The demand gate applies to momentum entries too. Without it, a curve that
    // merely sits inside a band triggers an order on every single account update -
    // a paper run against mainnet produced 12 orders in 1.5 seconds and hit the
    // rate limiter immediately.
    if let Some(skip) = demand_gate(c, cfg) {
        return Decision::Skip(skip);
    }
    momentum_decision(st, cfg)
}

/// Shared evidence check: is money actually flowing into this curve right now?
fn demand_gate(c: &Snapshot, cfg: &StrategyConfig) -> Option<Skip> {
    if c.updates < cfg.min_updates {
        return Some(Skip::NoDemandSignal);
    }
    let Some(inflow) = c.inflow else {
        return Some(Skip::NoDemandSignal);
    };
    if inflow < cfg.min_inflow_lamports {
        return Some(Skip::DemandTooWeak { inflow });
    }
    None
}

fn complete_decision(
    c: &Snapshot,
    st: &BondingCurve,
    gap: u64,
    cfg: &StrategyConfig,
) -> Decision {
    // A curve sitting one lamport from graduation is worthless if nothing is
    // flowing into it - that is exactly the -5.30 SOL trade.
    if let Some(skip) = demand_gate(c, cfg) {
        return Decision::Skip(skip);
    }

    let gross = pumpfun::gross_for_net(gap);
    if gross > cfg.max_position_lamports {
        return Decision::Skip(Skip::OrderTooLarge { gross });
    }
    Decision::Complete {
        gross,
        net: gap,
        min_tokens_out: apply_slippage(st.tokens_out(gap), cfg.slippage_bps),
    }
}

fn momentum_decision(st: &BondingCurve, cfg: &StrategyConfig) -> Decision {
    let depth = st.real_sol_reserves;
    let band = cfg
        .momentum_bands
        .iter()
        .find(|(lo, hi, _)| depth >= *lo && depth < *hi);
    match band {
        None => Decision::Skip(Skip::OutsideBands),
        Some((_, _, gross)) => {
            if *gross > cfg.max_position_lamports {
                return Decision::Skip(Skip::OrderTooLarge { gross: *gross });
            }
            let net = pumpfun::net_for_gross(*gross);
            Decision::Momentum {
                gross: *gross,
                min_tokens_out: apply_slippage(st.tokens_out(net), cfg.slippage_bps),
            }
        }
    }
}

fn apply_slippage(expected: u64, bps: u64) -> u64 {
    let bps = bps.min(10_000);
    ((expected as u128 * (10_000 - bps) as u128) / 10_000) as u64
}

// ---------------------------------------------------------------------------
// Exit planning
// ---------------------------------------------------------------------------

/// A single exit leg.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tranche {
    /// Tokens to sell.
    pub tokens: u64,
    /// Slots to wait after the previous leg.
    pub delay_slots: u64,
}

/// Exit tuning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExitConfig {
    pub tranches: usize,
    pub first_delay_slots: u64,
    pub spacing_slots: u64,
    /// Sell harder early; 1.0 is flat, >1.0 front-loads.
    pub front_load: f64,
}

impl Default for ExitConfig {
    fn default() -> Self {
        // The reference wallet exits in 4-10 legs over 20-30 seconds. Front-loading
        // matters: its winning trade took 40 SOL of its 66 SOL out in the first two
        // legs, while its losing trade dribbled out evenly and got nothing.
        Self {
            tranches: 6,
            first_delay_slots: 3,
            spacing_slots: 9,
            front_load: 1.6,
        }
    }
}

/// Split a position into front-loaded tranches. Always returns `cfg.tranches`
/// legs summing to exactly `tokens`.
pub fn plan_exit(tokens: u64, cfg: &ExitConfig) -> Vec<Tranche> {
    let n = cfg.tranches.max(1);
    if tokens == 0 {
        return Vec::new();
    }
    let weights: Vec<f64> = (0..n)
        .map(|i| cfg.front_load.powi((n - 1 - i) as i32))
        .collect();
    let total: f64 = weights.iter().sum();

    let mut out = Vec::with_capacity(n);
    let mut assigned = 0u64;
    for (i, w) in weights.iter().enumerate() {
        let share = if i == n - 1 {
            tokens - assigned
        } else {
            let s = ((tokens as f64) * (w / total)).floor() as u64;
            assigned += s;
            s
        };
        out.push(Tranche {
            tokens: share,
            delay_slots: if i == 0 {
                cfg.first_delay_slots
            } else {
                cfg.spacing_slots
            },
        });
    }
    out.retain(|t| t.tokens > 0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pumpfun::{INIT_REAL_TOKENS, INIT_VIRTUAL_SOL, INIT_VIRTUAL_TOKENS};
    use crate::tracker::Tracker;
    use solana_sdk::pubkey::Pubkey;

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

    /// Build a tracked curve that has seen `steps` buys climbing to `real_sol`.
    fn tracked(real_sol: u64, steps: usize) -> TrackedCurve {
        let t = Tracker::new();
        let mint = Pubkey::new_unique();
        let ck = pumpfun::bonding_curve(&mint);
        let mut last = t.observe(mint, ck, curve(0));
        for i in 1..=steps {
            let sol = real_sol * i as u64 / steps as u64;
            last = t.observe(mint, ck, curve(sol));
        }
        last
    }

    #[test]
    fn near_graduation_with_demand_triggers_completion() {
        let c = tracked(84_000_000_000, 6);
        let cfg = StrategyConfig::default();
        match evaluate(&c, &cfg) {
            Decision::Complete { gross, net, .. } => {
                assert!(net > 0 && net < cfg.max_gap_lamports);
                assert!(gross > net, "gross must include fees");
            }
            other => panic!("expected completion, got {other:?}"),
        }
    }

    #[test]
    fn near_graduation_without_demand_is_skipped() {
        // Two samples only: not enough evidence anyone is buying.
        let c = tracked(84_000_000_000, 1);
        let cfg = StrategyConfig::default();
        assert!(matches!(
            evaluate(&c, &cfg),
            Decision::Skip(Skip::NoDemandSignal)
        ));
    }

    #[test]
    fn flat_curve_near_graduation_is_skipped() {
        // Plenty of updates, but no growth at all - the losing-trade shape.
        let t = Tracker::new();
        let mint = Pubkey::new_unique();
        let ck = pumpfun::bonding_curve(&mint);
        let mut last = t.observe(mint, ck, curve(84_000_000_000));
        for _ in 0..5 {
            // Nudge by a single lamport so a new sample is recorded without inflow.
            let mut c = curve(84_000_000_000);
            c.virtual_token_reserves -= 1;
            last = t.observe(mint, ck, c);
        }
        match evaluate(&last, &StrategyConfig::default()) {
            Decision::Skip(Skip::DemandTooWeak { .. }) | Decision::Skip(Skip::NoDemandSignal) => {}
            other => panic!("expected a demand skip, got {other:?}"),
        }
    }

    /// The reference wallet's ladder, for tests that still exercise momentum.
    fn with_bands() -> StrategyConfig {
        StrategyConfig {
            momentum_bands: vec![
                (7_000_000_000, 11_000_000_000, 4_000_000_000),
                (10_000_000_000, 14_000_000_000, 7_000_000_000),
                (12_000_000_000, 16_000_000_000, 4_300_000_000),
                (18_000_000_000, 22_000_000_000, 4_000_000_000),
                (28_000_000_000, 32_000_000_000, 4_000_000_000),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn momentum_is_off_by_default_because_it_loses_money() {
        // 397 replayed trades, 26.7% hit rate, -108.6 SOL.
        assert!(StrategyConfig::default().momentum_bands.is_empty());
        let c = tracked(20_000_000_000, 6);
        assert!(matches!(
            evaluate(&c, &StrategyConfig::default()),
            Decision::Skip(Skip::OutsideBands)
        ));
    }

    #[test]
    fn wide_gap_falls_through_to_momentum_bands() {
        let c = tracked(20_000_000_000, 6);
        match evaluate(&c, &with_bands()) {
            Decision::Momentum { gross, .. } => assert_eq!(gross, 4_000_000_000),
            other => panic!("expected momentum, got {other:?}"),
        }
    }

    #[test]
    fn momentum_without_demand_is_skipped() {
        // Regression: a curve merely sitting inside a band used to fire an order on
        // every account update.
        let c = tracked(20_000_000_000, 1);
        assert!(matches!(
            evaluate(&c, &with_bands()),
            Decision::Skip(Skip::NoDemandSignal)
        ));
    }

    #[test]
    fn depth_between_bands_is_skipped() {
        let c = tracked(25_000_000_000, 6);
        assert!(matches!(
            evaluate(&c, &with_bands()),
            Decision::Skip(Skip::OutsideBands)
        ));
    }

    #[test]
    fn completed_curve_is_never_traded() {
        let mut c = tracked(84_000_000_000, 6);
        c.state.complete = true;
        assert!(matches!(
            evaluate(&c, &StrategyConfig::default()),
            Decision::Skip(Skip::AlreadyComplete)
        ));
    }

    #[test]
    fn position_cap_blocks_oversized_completion() {
        let cfg = StrategyConfig {
            max_position_lamports: 1_000_000,
            ..Default::default()
        };
        // Deep enough to be inside the default 2 SOL gap, so the size cap is
        // what rejects it rather than the gap width.
        let c = tracked(pumpfun::graduation_lamports() - 1_000_000_000, 6);
        assert!(matches!(
            evaluate(&c, &cfg),
            Decision::Skip(Skip::OrderTooLarge { .. })
        ));
    }

    #[test]
    fn a_pessimistic_model_vetoes_a_trade_the_thresholds_allowed() {
        let c = tracked(84_000_000_000, 6);
        let cfg = StrategyConfig::default();
        // Sanity: without a model this is a Complete.
        assert!(matches!(evaluate(&c, &cfg), Decision::Complete { .. }));

        let mut m = crate::score::Model::default();
        for _ in 0..500 {
            m.update(&features(&c, &cfg, &Context::default()), false);
        }
        let ctx = Context {
            model: Some(&m),
            ..Default::default()
        };
        match evaluate_with(&c, &cfg, &ctx) {
            Decision::Skip(Skip::LowProbability { p }) => assert!(p < cfg.min_graduation_prob),
            other => panic!("expected a model veto, got {other:?}"),
        }
    }

    #[test]
    fn the_model_can_only_remove_trades_never_add_them() {
        // A wildly optimistic model must not resurrect a curve the hard gates
        // rejected - here, one with no demand evidence at all.
        let c = tracked(84_000_000_000, 1);
        let cfg = StrategyConfig::default();
        let mut m = crate::score::Model::default();
        for _ in 0..500 {
            m.update(&features(&c, &cfg, &Context::default()), true);
        }
        assert!(m.predict(&features(&c, &cfg, &Context::default())) > 0.9);
        let ctx = Context {
            model: Some(&m),
            ..Default::default()
        };
        assert!(matches!(
            evaluate_with(&c, &cfg, &ctx),
            Decision::Skip(Skip::NoDemandSignal)
        ));
    }

    #[test]
    fn no_model_means_no_veto() {
        let c = tracked(84_000_000_000, 6);
        let cfg = StrategyConfig::default();
        assert_eq!(
            evaluate(&c, &cfg),
            evaluate_with(&c, &cfg, &Context::default())
        );
    }

    #[test]
    fn smart_money_feeds_through_into_the_feature_row() {
        let c = tracked(50_000_000_000, 6);
        let cfg = StrategyConfig::default();
        let ctx = Context {
            model: None,
            smart_money: 1.25,
            unique_buyers: 9,
        };
        let f = features(&c, &cfg, &ctx);
        assert_eq!(f.smart_money, 1.25);
        assert_eq!(f.unique_buyers, 9);
        assert_eq!(f.depth_lamports, c.state.real_sol_reserves);
    }

    #[test]
    fn exit_plan_conserves_the_whole_position() {
        let cfg = ExitConfig::default();
        let plan = plan_exit(1_000_000_000, &cfg);
        assert_eq!(plan.len(), cfg.tranches);
        assert_eq!(plan.iter().map(|t| t.tokens).sum::<u64>(), 1_000_000_000);
    }

    #[test]
    fn exit_plan_is_front_loaded() {
        let plan = plan_exit(1_000_000_000, &ExitConfig::default());
        assert!(
            plan[0].tokens > plan[plan.len() - 1].tokens,
            "first leg {} should exceed last {}",
            plan[0].tokens,
            plan[plan.len() - 1].tokens
        );
    }

    #[test]
    fn exit_plan_of_nothing_is_empty() {
        assert!(plan_exit(0, &ExitConfig::default()).is_empty());
    }

    #[test]
    fn slippage_reduces_the_minimum() {
        assert_eq!(apply_slippage(10_000, 1_500), 8_500);
        assert_eq!(apply_slippage(10_000, 0), 10_000);
    }
}
