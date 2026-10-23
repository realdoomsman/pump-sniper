//! Replay recorded curve trajectories through the live decision code.
//!
//! The point of a backtest is to be wrong in ways you can name. This one calls
//! [`crate::strategy::evaluate_snapshot`] — the exact function the live loop uses —
//! so a result here is about the real strategy, not a reimplementation of it.
//!
//! ## What it models, and what it cannot
//!
//! * **Fills use the real bonding-curve math**, including the 1.25% fee.
//! * **Our own buy IS carried through the exit.** The recorded trajectory comes
//!   from a world where we did not trade, so the lamports our buy put into the
//!   curve are added back when pricing every sell, and drawn down as we sell out.
//!   Without this, a flat round trip appears to lose ~20% instead of the ~2.5% it
//!   actually costs in fees. What is still missing is *other* traders reacting to
//!   our order, which no replay of recorded data can recover.
//! * **Completion trades cannot be settled from this data at all.** Once a curve
//!   graduates, trading moves to a PumpSwap pool that `collect` never sees. The
//!   tokens are therefore valued at the *measured* pool opening state — 67.406 SOL
//!   against 206,900,000 tokens — which is a mark, not a fill. Reported separately
//!   and never mixed into realised P&L.
//! * **No latency and no competition.** A trigger here always fills. Live, most
//!   will not.

use crate::pumpfun::{self, BondingCurve};
use crate::score::Model;
use crate::strategy::{self, Context, Decision, ExitConfig, Snapshot, StrategyConfig};
use crate::train::Trajectory;
use std::time::Duration;

/// Measured opening state of a freshly migrated PumpSwap pool.
pub const POOL_OPEN_QUOTE_LAMPORTS: u128 = 67_406_000_000;
pub const POOL_OPEN_BASE_TOKENS: u128 = 206_900_000_000_000;

/// Virtual quote liquidity a non-mayhem pool opens with, measured at 17.5845 SOL
/// on 23 of 23 such pools.
pub const BOOST_QUOTE_TYPICAL: u128 = 17_584_500_000;

/// Curve marginal price at graduation, in lamports per token.
pub fn curve_close_price() -> f64 {
    let k = pumpfun::INIT_VIRTUAL_SOL * pumpfun::INIT_VIRTUAL_TOKENS;
    let vs = pumpfun::INIT_VIRTUAL_SOL + pumpfun::graduation_lamports() as u128;
    let vt = k / vs;
    vs as f64 / vt as f64
}

/// How far below the final curve price a pool opens, given its **actual** reserves.
///
/// ## Why this takes explicit reserves
///
/// This function used to be `pool_open_discount(boost)` — one argument, paired
/// internally with the constants below. That shape let you ask for the discount of
/// a pool holding 67.406 SOL of real quote *and* zero boost, and it answered
/// **20.71%** in a tone of complete confidence.
///
/// No such pool has ever existed. Across 267 migrations decoded from mainnet
/// blocks, the two regimes are not two kinds of pool — they are two different
/// graduation rules, and they are perfectly separated:
///
/// | `is_mayhem_mode` | curve raised | pool opens with | discount | n |
/// | --- | --- | --- | --- | --- |
/// | clear | 85.007 SOL (classic) | 67.4059 real + 17.5845 boost | **0.0244%** | 190 |
/// | set | median 0.107 SOL | median 0.116 SOL, boost 0 | 98.53% | 76 |
///
/// Both off-diagonal cells are empty. A mayhem pool is not a discounted
/// graduation; it is a dead coin that graduated under a different rule into a pool
/// holding about a tenth of a SOL. Its "98.53% discount" is unreachable — you
/// cannot buy 2 SOL of a pool that contains 0.116 SOL.
///
/// The old signature could not express that, so it is gone. Pass real numbers.
pub fn pool_open_discount_of(quote: u128, base: u128, boost: u128) -> f64 {
    if base == 0 {
        return 0.0;
    }
    let pool_price = (quote + boost) as f64 / base as f64;
    1.0 - pool_price / curve_close_price()
}

/// The only regime a *completion* can produce: a classic 85.007 SOL graduation.
///
/// Completing a curve means driving it to the classic graduation line, so this is
/// the discount our own trades actually meet. It is ~0.02% — the 17.59 SOL that
/// leaves for the boost vault is credited straight back as virtual liquidity, so
/// the pool opens at essentially the price the last curve buyer paid.
///
/// **There is no opening discount for a completion to capture.**
pub fn ordinary_pool_open_discount() -> f64 {
    pool_open_discount_of(
        POOL_OPEN_QUOTE_LAMPORTS,
        POOL_OPEN_BASE_TOKENS,
        BOOST_QUOTE_TYPICAL,
    )
}

/// Roughly one Solana slot.
const SLOT_MS: u64 = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Completion,
    Momentum,
}

#[derive(Debug, Clone)]
pub struct SimTrade {
    pub curve: String,
    pub kind: Kind,
    pub entry_ts: u64,
    /// Gross lamports paid, fees included.
    pub gross_in: u64,
    pub tokens: u64,
    /// Lamports realised by selling back into the curve. Completions do not have
    /// this: they graduate, and the exit venue is not in the data.
    pub realised: Option<u64>,
    /// Mark-to-open value for completions.
    pub marked: Option<u64>,
    pub curve_graduated: bool,
}

impl SimTrade {
    /// Signed P&L in lamports, using whichever valuation applies.
    pub fn pnl(&self) -> i64 {
        let out = self.realised.or(self.marked).unwrap_or(0);
        out as i64 - self.gross_in as i64
    }
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub curves_replayed: usize,
    pub triggers: usize,
    pub completions: usize,
    pub momentum: usize,
    /// Completions whose curve did in fact graduate.
    pub completions_that_graduated: usize,
    pub momentum_wins: usize,
    pub momentum_losses: usize,
    /// Realised only — momentum round trips.
    pub realised_pnl: i64,
    /// Marked only — completions, valued at the pool open.
    pub marked_pnl: i64,
    pub fees_paid: u64,
}

/// Rebuild the rolling demand window from recorded samples.
fn snapshot_at(t: &Trajectory, i: usize, window: Duration) -> Snapshot {
    let s = &t.samples[i];
    let cutoff = s.ts.saturating_sub(window.as_millis() as u64);
    let first = t.samples[..=i].iter().find(|x| x.ts >= cutoff);
    let (inflow, velocity) = match first {
        Some(f) if f.ts < s.ts => {
            let dt = (s.ts - f.ts) as f64 / 1000.0;
            let d = s.real_sol as i64 - f.real_sol as i64;
            (Some(d), if dt > 0.0 { Some(d as f64 / dt) } else { None })
        }
        _ => (None, None),
    };
    // Reconstruct the curve from the recorded reserves. Only `real_sol` and
    // `complete` are recorded, so virtual reserves are re-derived from the
    // invariant - which is exact, since the curve is a constant product.
    let vs = pumpfun::INIT_VIRTUAL_SOL as u64 + s.real_sol;
    let k = pumpfun::INIT_VIRTUAL_SOL * pumpfun::INIT_VIRTUAL_TOKENS;
    let vt = (k / vs as u128) as u64;
    let sold = pumpfun::INIT_VIRTUAL_TOKENS as u64 - vt;
    let state = BondingCurve {
        virtual_sol_reserves: vs,
        virtual_token_reserves: vt,
        real_token_reserves: (pumpfun::INIT_REAL_TOKENS as u64).saturating_sub(sold),
        real_sol_reserves: s.real_sol,
        token_total_supply: 1_000_000_000_000_000,
        complete: s.complete,
        creator: None,
    };
    Snapshot {
        state,
        updates: i + 1,
        age: Duration::from_millis(s.ts.saturating_sub(t.first_ts())),
        // Replay is gap-free by construction; staleness is the step since the
        // previous sample.
        staleness: Duration::from_millis(if i == 0 {
            0
        } else {
            s.ts.saturating_sub(t.samples[i - 1].ts)
        }),
        inflow,
        velocity,
    }
}

/// Value tokens at the pool's opening price, given its boost.
pub fn mark_at_pool_open_with_boost(tokens: u64, boost_quote: u128) -> u64 {
    ((tokens as u128 * (POOL_OPEN_QUOTE_LAMPORTS + boost_quote)) / POOL_OPEN_BASE_TOKENS) as u64
}

/// Value a completion's tokens at the pool it will actually open.
///
/// A completion always produces an ordinary pool, so it always carries the typical
/// boost. Marking at boost = 0 (which this function used to do) values the tokens
/// against a pool that cannot exist and understates them by ~20.7%.
pub fn mark_completion(tokens: u64) -> u64 {
    mark_at_pool_open_with_boost(tokens, BOOST_QUOTE_TYPICAL)
}

/// One row of a gap sweep.
#[derive(Debug, Clone, Copy)]
pub struct GapPoint {
    pub max_gap: u64,
    pub triggers: usize,
    pub graduated: usize,
    pub marked_pnl: i64,
}

impl GapPoint {
    /// Premium paid per completion, in lamports. This is the option price.
    pub fn cost_per_trigger(&self) -> f64 {
        if self.triggers == 0 {
            0.0
        } else {
            self.marked_pnl as f64 / self.triggers as f64
        }
    }
    /// Fraction of triggered curves that really did graduate.
    pub fn precision(&self) -> f64 {
        if self.triggers == 0 {
            0.0
        } else {
            self.graduated as f64 / self.triggers as f64
        }
    }
}

/// Sweep `max_gap_lamports` and report the cost/precision tradeoff.
///
/// The completion buy is a **premium paid for an option on the migration**. The
/// option's payoff — the edge on buying the new pool 20.7% below the final curve
/// price — does not depend on how large a gap you closed. The premium does, almost
/// exactly linearly. So the premium should be as small as the opportunity count
/// allows, and this is the function that shows where that is.
pub fn sweep_gap(
    trajs: &[Trajectory],
    scfg: &StrategyConfig,
    ecfg: &ExitConfig,
    gaps: &[u64],
    per_attempt_fee: u64,
) -> Vec<GapPoint> {
    gaps.iter()
        .map(|g| {
            let cfg = StrategyConfig {
                max_gap_lamports: *g,
                // Momentum would pollute the comparison.
                momentum_bands: Vec::new(),
                ..scfg.clone()
            };
            let (_, rep) = run(trajs, &cfg, ecfg, None, per_attempt_fee);
            GapPoint {
                max_gap: *g,
                triggers: rep.completions,
                graduated: rep.completions_that_graduated,
                marked_pnl: rep.marked_pnl,
            }
        })
        .collect()
}

/// Replay every trajectory. One trade per curve, matching the live dedup rule.
pub fn run(
    trajs: &[Trajectory],
    scfg: &StrategyConfig,
    ecfg: &ExitConfig,
    model: Option<&Model>,
    per_attempt_fee: u64,
) -> (Vec<SimTrade>, Report) {
    let mut rep = Report {
        curves_replayed: trajs.len(),
        ..Default::default()
    };
    let ctx = Context {
        model,
        smart_money: 0.0,
        unique_buyers: 0,
    };
    let mut trades = Vec::new();

    for t in trajs {
        if t.samples.len() < 2 || t.only_seen_after_graduation() {
            continue;
        }
        for i in 0..t.samples.len() {
            let snap = snapshot_at(t, i, scfg.demand_window);
            let decision = strategy::evaluate_snapshot(&snap, scfg, &ctx);
            let (kind, gross) = match decision {
                Decision::Complete { gross, .. } => (Kind::Completion, gross),
                Decision::Momentum { gross, .. } => (Kind::Momentum, gross),
                Decision::Skip(_) => continue,
            };

            rep.triggers += 1;
            rep.fees_paid += per_attempt_fee;
            let net = pumpfun::net_for_gross(gross);
            let tokens = snap.state.tokens_out(net);

            let mut trade = SimTrade {
                curve: t.curve.clone(),
                kind,
                entry_ts: t.samples[i].ts,
                gross_in: gross.saturating_add(per_attempt_fee),
                tokens,
                realised: None,
                marked: None,
                curve_graduated: t.graduated(),
            };

            match kind {
                Kind::Completion => {
                    rep.completions += 1;
                    if trade.curve_graduated {
                        rep.completions_that_graduated += 1;
                    }
                    let m = mark_completion(tokens);
                    trade.marked = Some(m);
                    rep.marked_pnl += trade.pnl();
                }
                Kind::Momentum => {
                    rep.momentum += 1;
                    let realised = simulate_exit(t, i, tokens, net, ecfg);
                    trade.realised = Some(realised);
                    let pnl = trade.pnl();
                    rep.realised_pnl += pnl;
                    if pnl > 0 {
                        rep.momentum_wins += 1;
                    } else {
                        rep.momentum_losses += 1;
                    }
                }
            }
            trades.push(trade);
            // One trade per curve, exactly like the live loop.
            break;
        }
    }
    (trades, rep)
}

/// Sell the position back into the curve on the planned tranche schedule.
///
/// `our_net` is the lamports our own buy put into the curve. The recorded
/// trajectory comes from a world where we did not trade, so without adding it back
/// every sell is priced into a curve that is missing our own money — which
/// systematically understates proceeds. It is drawn down as we sell out.
fn simulate_exit(
    t: &Trajectory,
    entry: usize,
    tokens: u64,
    our_net: u64,
    ecfg: &ExitConfig,
) -> u64 {
    let plan = strategy::plan_exit(tokens, ecfg);
    let mut proceeds = 0u64;
    let mut elapsed_ms = 0u64;
    let entry_ts = t.samples[entry].ts;
    let mut still_in = our_net;

    for leg in plan {
        elapsed_ms += leg.delay_slots * SLOT_MS;
        let target = entry_ts + elapsed_ms;
        // The first recorded state at or after the scheduled time; if the curve
        // went quiet, the last known state - selling into a dead book is exactly
        // what would happen.
        let s = t
            .samples
            .iter()
            .skip(entry)
            .find(|s| s.ts >= target)
            .or_else(|| t.samples.last());
        let Some(s) = s else { continue };
        if s.complete {
            // Curve graduated mid-exit: the remaining tokens are no longer sellable
            // here. Stop rather than pretend.
            break;
        }
        // Price into the curve as it would be WITH our buy still in it.
        let effective_real = s.real_sol.saturating_add(still_in);
        let vs = pumpfun::INIT_VIRTUAL_SOL as u64 + effective_real;
        let k = pumpfun::INIT_VIRTUAL_SOL * pumpfun::INIT_VIRTUAL_TOKENS;
        let vt = (k / vs as u128) as u64;
        let state = BondingCurve {
            virtual_sol_reserves: vs,
            virtual_token_reserves: vt,
            real_token_reserves: 0,
            real_sol_reserves: effective_real,
            token_total_supply: 0,
            complete: false,
            creator: None,
        };
        let gross_out = state.sol_out(leg.tokens);
        still_in = still_in.saturating_sub(gross_out);
        // Selling pays the same 1.25% as buying.
        proceeds += pumpfun::net_for_gross(gross_out);
    }
    proceeds
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::train::Sample;

    fn traj(name: &str, pts: &[(u64, u64)], complete_last: bool) -> Trajectory {
        let mut samples: Vec<Sample> = pts
            .iter()
            .map(|(ts, sol)| Sample {
                ts: *ts,
                curve: name.into(),
                real_sol: *sol,
                complete: false,
            })
            .collect();
        if complete_last {
            if let Some(l) = samples.last_mut() {
                l.complete = true;
            }
        }
        Trajectory {
            curve: name.into(),
            samples,
        }
    }

    /// Momentum is off by default (it loses money), so tests that exercise the
    /// momentum path have to opt the bands back in explicitly.
    fn cfgs() -> (StrategyConfig, ExitConfig) {
        (
            StrategyConfig {
                momentum_bands: vec![
                    (7_000_000_000, 11_000_000_000, 4_000_000_000),
                    (10_000_000_000, 14_000_000_000, 7_000_000_000),
                    (12_000_000_000, 16_000_000_000, 4_300_000_000),
                    (18_000_000_000, 22_000_000_000, 4_000_000_000),
                    (28_000_000_000, 32_000_000_000, 4_000_000_000),
                ],
                ..Default::default()
            },
            ExitConfig::default(),
        )
    }

    #[test]
    fn default_config_takes_no_momentum_trades() {
        let t = traj(
            "a",
            &[(0, 1_000_000_000), (2_000, 5_000_000_000), (4_000, 8_000_000_000), (6_000, 9_000_000_000)],
            false,
        );
        let (_, rep) = run(&[t], &StrategyConfig::default(), &ExitConfig::default(), None, 5000);
        assert_eq!(rep.momentum, 0, "momentum is disabled by default");
    }

    #[test]
    fn a_completion_meets_the_ordinary_regime_and_it_has_no_discount() {
        // ~0.0244% on 190 of 190 classic graduations. Not 20.71%.
        let d = ordinary_pool_open_discount();
        assert!(
            d.abs() < 0.001,
            "ordinary discount is {d:.6}, expected ~0.000244"
        );
    }

    #[test]
    fn the_mayhem_regime_is_a_pool_too_small_to_trade() {
        // Measured median mayhem pool: 0.116 SOL of real quote, boost 0, against a
        // curve that only ever raised ~0.107 SOL. The discount is enormous and
        // completely unreachable.
        let quote = 116_000_000u128;
        let base = POOL_OPEN_BASE_TOKENS;
        let d = pool_open_discount_of(quote, base, 0);
        assert!(d > 0.9, "mayhem discount is {d:.4}, expected >0.9");

        // The point: you cannot buy into it. Two SOL is ~17x the entire pool, so
        // the constant product hands you a price nothing like the "discount".
        let got = crate::pumpswap::base_out(base as u64, quote as u64, 0, 2_000_000_000);
        let paid_per_token = 2_000_000_000f64 / got as f64;
        let pool_mid = quote as f64 / base as f64;
        assert!(
            paid_per_token > pool_mid * 10.0,
            "expected >10x slippage buying 2 SOL into a 0.116 SOL pool, got {:.2}x",
            paid_per_token / pool_mid
        );
    }

    #[test]
    fn the_mark_scales_with_the_boost() {
        let t = 1_000_000_000_000u64;
        assert!(
            mark_at_pool_open_with_boost(t, BOOST_QUOTE_TYPICAL)
                > mark_at_pool_open_with_boost(t, 0)
        );
        assert_eq!(mark_completion(t), mark_at_pool_open_with_boost(t, BOOST_QUOTE_TYPICAL));
    }

    #[test]
    fn a_completion_is_roughly_flat_not_a_twenty_percent_loss() {
        // The old version of this test asserted a >15% loss, because it marked the
        // tokens against a mayhem pool. A completion never produces one. Marked
        // correctly, a completion costs about the 1.25% fee and nothing more.
        let k = pumpfun::INIT_VIRTUAL_SOL * pumpfun::INIT_VIRTUAL_TOKENS;
        let vs = pumpfun::INIT_VIRTUAL_SOL + pumpfun::graduation_lamports() as u128 - 1_000_000_000;
        let vt = (k / vs) as u64;
        let top = BondingCurve {
            virtual_sol_reserves: vs as u64,
            virtual_token_reserves: vt,
            real_token_reserves: u64::MAX,
            real_sol_reserves: pumpfun::graduation_lamports() - 1_000_000_000,
            token_total_supply: 0,
            complete: false,
            creator: None,
        };
        let paid = 1_000_000_000u64;
        let tokens = top.tokens_out(paid);
        let marked = mark_completion(tokens);
        // Closing a 1 SOL gap buys the curve's last tokens along a RISING price
        // and marks them at the top, so the mark actually comes out slightly
        // POSITIVE - the climb beats the 1.25% fee. Either way the honest claim is
        // "roughly flat", not the ">15% loss" this test used to assert.
        let delta = marked as f64 / paid as f64 - 1.0;
        assert!(
            delta.abs() < 0.03,
            "completion mark moved {delta:+.4}, expected roughly flat"
        );
    }

    #[test]
    fn pool_open_mark_is_the_measured_price() {
        // The whole opening base reserve is worth the whole opening quote reserve
        // plus the boost that backs it.
        assert_eq!(
            mark_at_pool_open_with_boost(POOL_OPEN_BASE_TOKENS as u64, BOOST_QUOTE_TYPICAL),
            (POOL_OPEN_QUOTE_LAMPORTS + BOOST_QUOTE_TYPICAL) as u64
        );
        assert_eq!(mark_completion(0), 0);
    }

    #[test]
    fn snapshot_reconstructs_the_curve_invariant() {
        let t = traj("a", &[(0, 0), (1000, 10_000_000_000)], false);
        let s = snapshot_at(&t, 1, Duration::from_secs(30));
        assert_eq!(s.state.real_sol_reserves, 10_000_000_000);
        assert_eq!(
            s.state.virtual_sol_reserves,
            pumpfun::INIT_VIRTUAL_SOL as u64 + 10_000_000_000
        );
        // Invariant: virtual_sol * virtual_tokens is conserved.
        let k = s.state.virtual_sol_reserves as u128 * s.state.virtual_token_reserves as u128;
        let k0 = pumpfun::INIT_VIRTUAL_SOL * pumpfun::INIT_VIRTUAL_TOKENS;
        let err = (k as f64 - k0 as f64).abs() / k0 as f64;
        assert!(err < 1e-6, "invariant drifted by {err}");
    }

    #[test]
    fn a_flat_curve_never_triggers() {
        let t = traj("a", &[(0, 8_000_000_000); 6], false);
        let (s, e) = cfgs();
        let (trades, rep) = run(&[t], &s, &e, None, 5000);
        assert!(trades.is_empty());
        assert_eq!(rep.triggers, 0);
    }

    #[test]
    fn a_climbing_curve_in_a_band_triggers_momentum_once() {
        // Climbs through the 7-11 SOL band with real inflow.
        let t = traj(
            "a",
            &[
                (0, 1_000_000_000),
                (2_000, 4_000_000_000),
                (4_000, 6_000_000_000),
                (6_000, 8_000_000_000),
                (8_000, 9_000_000_000),
                (10_000, 9_500_000_000),
            ],
            false,
        );
        let (s, e) = cfgs();
        let (trades, rep) = run(&[t], &s, &e, None, 5000);
        assert_eq!(rep.triggers, 1, "one trade per curve");
        assert_eq!(rep.momentum, 1);
        assert_eq!(trades[0].kind, Kind::Momentum);
        assert!(trades[0].tokens > 0);
        assert!(trades[0].realised.is_some());
        assert!(trades[0].marked.is_none());
    }

    #[test]
    fn a_curve_near_the_line_triggers_a_completion_and_is_marked_not_realised() {
        let g = pumpfun::graduation_lamports();
        let t = traj(
            "a",
            &[
                (0, g - 40_000_000_000),
                (2_000, g - 20_000_000_000),
                (4_000, g - 15_000_000_000),
                (6_000, g - 8_000_000_000),
                // Inside the default 2 SOL max_gap.
                (8_000, g - 1_500_000_000),
            ],
            false,
        );
        let (s, e) = cfgs();
        let (trades, rep) = run(&[t], &s, &e, None, 5000);
        assert_eq!(rep.completions, 1);
        assert_eq!(trades[0].kind, Kind::Completion);
        assert!(trades[0].marked.is_some(), "completions are marked");
        assert!(
            trades[0].realised.is_none(),
            "completions must never report realised P&L - the venue is not in the data"
        );
        assert_eq!(rep.realised_pnl, 0);
    }

    #[test]
    fn exit_stops_when_the_curve_graduates_mid_plan() {
        let t = traj(
            "a",
            &[
                (0, 1_000_000_000),
                (2_000, 5_000_000_000),
                (4_000, 8_000_000_000),
                (6_000, 9_000_000_000),
                (8_000, pumpfun::graduation_lamports()),
            ],
            true,
        );
        let (s, e) = cfgs();
        let (trades, _) = run(&[t], &s, &e, None, 5000);
        if let Some(tr) = trades.first() {
            if tr.kind == Kind::Momentum {
                // Must not have sold into a completed curve.
                assert!(tr.realised.unwrap() < tr.gross_in * 10);
            }
        }
    }

    #[test]
    fn fees_are_charged_per_trigger() {
        let t = traj(
            "a",
            &[
                (0, 1_000_000_000),
                (2_000, 4_000_000_000),
                (4_000, 8_000_000_000),
                (6_000, 9_000_000_000),
            ],
            false,
        );
        let (s, e) = cfgs();
        let (trades, rep) = run(&[t], &s, &e, None, 7_777);
        if rep.triggers > 0 {
            assert_eq!(rep.fees_paid, 7_777 * rep.triggers as u64);
            assert!(trades[0].gross_in > 7_777);
        }
    }

    #[test]
    fn curves_only_seen_after_graduation_are_skipped() {
        let mut t = traj("a", &[(0, 0), (1000, 0)], false);
        for s in t.samples.iter_mut() {
            s.complete = true;
        }
        let (s, e) = cfgs();
        let (trades, rep) = run(&[t], &s, &e, None, 5000);
        assert!(trades.is_empty());
        assert_eq!(rep.triggers, 0);
    }

    #[test]
    fn an_immediate_round_trip_loses_only_the_fees() {
        // Buy and sell back into an unchanged curve. The only loss should be the
        // 1.25% each way, not a phantom loss from pricing into a curve that is
        // missing our own money.
        // Flat from the moment of entry onwards, so any profit would be spurious.
        let depth = 8_000_000_000u64;
        let t = traj(
            "a",
            &[
                (0, 1_000_000_000),
                (2_000, 5_000_000_000),
                (4_000, depth),
                (6_000, depth),
                (8_000, depth),
                (60_000, depth),
            ],
            false,
        );
        let (s, e) = cfgs();
        let (trades, rep) = run(&[t], &s, &e, None, 0);
        assert_eq!(rep.momentum, 1);
        let tr = &trades[0];
        let ratio = tr.realised.unwrap() as f64 / tr.gross_in as f64;
        assert!(
            ratio > 0.93,
            "flat round trip returned only {:.1}% - impact correction is wrong",
            ratio * 100.0
        );
        assert!(ratio < 1.0, "cannot profit from a flat curve");
    }

    #[test]
    fn a_tighter_gap_is_cheaper_per_trade_and_more_accurate() {
        // The whole case for a small max_gap, asserted rather than asserted-by-blog.
        // Build curves at a spread of distances from the line, all of which climb.
        let g = pumpfun::graduation_lamports();
        let mut trajs = Vec::new();
        for (i, short) in [10_000_000_000u64, 5_000_000_000, 1_000_000_000, 200_000_000]
            .iter()
            .enumerate()
        {
            trajs.push(traj(
                &format!("c{i}"),
                &[
                    (0, g - short - 20_000_000_000),
                    (2_000, g - short - 10_000_000_000),
                    (4_000, g - short - 5_000_000_000),
                    (6_000, g - short - 1_000_000_000),
                    (8_000, g - short),
                ],
                false,
            ));
        }
        let (s, e) = cfgs();
        let pts = sweep_gap(
            &trajs,
            &s,
            &e,
            &[12_000_000_000, 2_000_000_000, 500_000_000],
            0,
        );
        assert_eq!(pts.len(), 3);
        // Wider gap => more triggers.
        assert!(pts[0].triggers >= pts[2].triggers);
        // The direction of this assertion used to be inverted, because every
        // completion was marked against a mayhem pool that cannot exist. Marked
        // correctly, the completion buy is NOT a premium: it is flat to slightly
        // positive, and a wider gap buys more of the curve's rising tail, so the
        // mark grows rather than shrinking.
        for w in pts.windows(2) {
            if w[0].triggers > 0 && w[1].triggers > 0 {
                assert!(
                    w[0].cost_per_trigger() >= w[1].cost_per_trigger(),
                    "a wider gap should mark higher, not lower: {:?}",
                    (w[0].cost_per_trigger(), w[1].cost_per_trigger())
                );
            }
        }
        // ...and none of them is a meaningful cost.
        for p in &pts {
            assert!(
                p.cost_per_trigger() > -50_000_000.0,
                "gap {} marked {:.0} lamports/trigger - completions are not a premium",
                p.max_gap,
                p.cost_per_trigger()
            );
        }
    }

    #[test]
    fn the_mark_overstates_what_a_completion_could_actually_realise() {
        // THIS is the honest limit of every number in this module. `mark_completion`
        // values tokens at the pool's opening mid. Selling them moves that price,
        // and a completion large enough to be interesting is large relative to the
        // 67.4 SOL pool it opens. The mark is an upper bound, never a fill.
        let tokens = 12_000_000_000_000u64; // ~12 SOL of curve tail
        let marked = mark_completion(tokens);
        let realised = crate::pumpswap::quote_out(
            POOL_OPEN_BASE_TOKENS as u64,
            POOL_OPEN_QUOTE_LAMPORTS as u64,
            BOOST_QUOTE_TYPICAL as u64,
            tokens,
        );
        assert!(
            realised < marked,
            "a real CPMM exit ({realised}) must come in under the mark ({marked})"
        );
        let shortfall = 1.0 - realised as f64 / marked as f64;
        assert!(
            shortfall > 0.02,
            "expected a visible impact shortfall, got {shortfall:.4}"
        );
    }

    #[test]
    fn gap_point_arithmetic() {
        let p = GapPoint { max_gap: 1, triggers: 4, graduated: 3, marked_pnl: -400 };
        assert_eq!(p.cost_per_trigger(), -100.0);
        assert!((p.precision() - 0.75).abs() < 1e-9);
        let empty = GapPoint { max_gap: 1, triggers: 0, graduated: 0, marked_pnl: 0 };
        assert_eq!(empty.cost_per_trigger(), 0.0);
        assert_eq!(empty.precision(), 0.0);
    }

    #[test]
    fn pnl_uses_realised_when_present_and_marked_otherwise() {
        let mut tr = SimTrade {
            curve: "a".into(),
            kind: Kind::Momentum,
            entry_ts: 0,
            gross_in: 1_000,
            tokens: 10,
            realised: Some(1_500),
            marked: Some(9_999),
            curve_graduated: false,
        };
        assert_eq!(tr.pnl(), 500, "realised wins over marked");
        tr.realised = None;
        assert_eq!(tr.pnl(), 8_999);
        tr.marked = None;
        assert_eq!(tr.pnl(), -1_000);
    }
}
