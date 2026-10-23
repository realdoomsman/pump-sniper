//! Open positions and their exit schedules.
//!
//! `strategy::plan_exit` decides *how* to get out; this decides *when*, and holds
//! the state between the buy landing and the last tranche clearing. Without it the
//! bot buys and never sells, which is the worst possible bug in a strategy whose
//! entire thesis is a 20-second hold.
//!
//! Time is passed in as milliseconds rather than read from a clock, so the whole
//! schedule is testable without sleeping.

use crate::strategy::Tranche;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;

/// Roughly one Solana slot.
pub const SLOT_MS: u64 = 400;

/// Where a position has to be sold.
///
/// This is not cosmetic. A completion buy **graduates the curve**, and pump.fun
/// then refuses every sell against it — its own error string is "The bonding curve
/// has completed and liquidity migrated". A position opened by a completion can
/// only be exited on the PumpSwap pool the migration created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitVenue {
    /// Still on the bonding curve: sell with pump.fun `SellV2`.
    BondingCurve,
    /// Curve graduated: sell on the PumpSwap pool.
    PumpSwap,
}

#[derive(Debug, Clone)]
pub struct Position {
    /// Where this position must be sold.
    pub venue: ExitVenue,
    pub curve: Pubkey,
    pub mint: Pubkey,
    /// Tokens still held.
    pub remaining: u64,
    pub plan: Vec<Tranche>,
    /// Index of the next tranche to fire.
    pub next_leg: usize,
    /// When that tranche becomes due.
    pub due_ms: u64,
    pub opened_ms: u64,
    /// Gross lamports paid across ALL entry legs.
    ///
    /// A position can be entered twice: the completion buy on the curve, and then
    /// optionally a second buy out of the pool the migration created. `pnl()` stays
    /// correct for free as long as this is the running total.
    pub cost: u64,
    /// One record per entry leg, so a two-leg position can report where its cost
    /// basis actually came from.
    pub entries: Vec<Entry>,
    /// Lamports realised so far.
    pub proceeds: u64,
}

/// One entry leg.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub tokens: u64,
    pub cost: u64,
    pub at_ms: u64,
}

impl Position {
    /// Blended cost basis, lamports per token.
    pub fn avg_cost_per_token(&self) -> f64 {
        let tokens: u64 = self.entries.iter().map(|e| e.tokens).sum();
        if tokens == 0 {
            return 0.0;
        }
        self.cost as f64 / tokens as f64
    }

    pub fn is_done(&self) -> bool {
        self.next_leg >= self.plan.len() || self.remaining == 0
    }

    /// Signed P&L so far, in lamports.
    pub fn pnl(&self) -> i64 {
        self.proceeds as i64 - self.cost as i64
    }

    pub fn age_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.opened_ms)
    }
}

/// One tranche that is ready to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueLeg {
    pub curve: Pubkey,
    pub mint: Pubkey,
    pub venue: ExitVenue,
    pub tokens: u64,
    pub leg: usize,
    pub of: usize,
}

/// What one exit tranche is worth, priced at the venue it will actually execute on.
///
/// Regression guard. Quoting a PumpSwap position against its bonding curve returns
/// **zero** — a graduated curve has no reserves left — so every exit was being
/// reported as a total loss. The venue has to pick the pricing.
pub fn quote_exit_leg(
    venue: ExitVenue,
    curve: &crate::pumpfun::BondingCurve,
    tokens: u64,
) -> u64 {
    match venue {
        ExitVenue::BondingCurve => crate::pumpfun::net_for_gross(curve.sol_out(tokens)),
        // Marked at the measured pool opening price. A true quote needs live pool
        // reserves; this is the honest stand-in, and the same one the backtest uses.
        ExitVenue::PumpSwap => crate::backtest::mark_completion(tokens),
    }
}

#[derive(Default)]
pub struct Book {
    positions: HashMap<Pubkey, Position>,
}

impl Book {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.positions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }

    pub fn get(&self, curve: &Pubkey) -> Option<&Position> {
        self.positions.get(curve)
    }

    /// Record a filled buy and schedule its exit.
    ///
    /// Returns false if a position on this curve is already open — re-opening
    /// would lose the existing schedule and strand the tokens.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        &mut self,
        curve: Pubkey,
        mint: Pubkey,
        venue: ExitVenue,
        tokens: u64,
        cost: u64,
        plan: Vec<Tranche>,
        now_ms: u64,
    ) -> bool {
        if self.positions.contains_key(&curve) || tokens == 0 || plan.is_empty() {
            return false;
        }
        let due_ms = now_ms + plan[0].delay_slots * SLOT_MS;
        self.positions.insert(
            curve,
            Position {
                venue,
                curve,
                mint,
                remaining: tokens,
                plan,
                next_leg: 0,
                due_ms,
                opened_ms: now_ms,
                cost,
                entries: vec![Entry {
                    tokens,
                    cost,
                    at_ms: now_ms,
                }],
                proceeds: 0,
            },
        );
        true
    }

    /// Fold a SECOND entry into an already-open position and re-plan its exit.
    ///
    /// This exists because [`Self::open`] refuses a curve it already holds, and the
    /// caller reads that `false` as "could not schedule an exit, release the
    /// reservation" - which on a live position would release the *first* leg's
    /// reservation while still holding its tokens.
    ///
    /// Only the tail of the plan is re-planned. Tranches already fired stay fired;
    /// re-planning the whole schedule would try to sell tokens that are gone.
    ///
    /// Returns false if there is no open position, if it has already finished, or
    /// if `tokens` is zero - in every case the caller must NOT treat the buy as
    /// booked.
    pub fn add_leg(
        &mut self,
        curve: &Pubkey,
        tokens: u64,
        cost: u64,
        ecfg: &crate::strategy::ExitConfig,
        now_ms: u64,
    ) -> bool {
        let Some(p) = self.positions.get_mut(curve) else {
            return false;
        };
        if p.is_done() || tokens == 0 {
            return false;
        }
        p.remaining = p.remaining.saturating_add(tokens);
        p.cost = p.cost.saturating_add(cost);
        p.entries.push(Entry {
            tokens,
            cost,
            at_ms: now_ms,
        });
        p.plan.truncate(p.next_leg);
        p.plan.extend(crate::strategy::plan_exit(p.remaining, ecfg));
        if p.next_leg < p.plan.len() {
            p.due_ms = now_ms + p.plan[p.next_leg].delay_slots * SLOT_MS;
        }
        true
    }

    /// Tranches whose scheduled time has arrived.
    ///
    /// Does not advance anything — a leg stays due until [`Self::on_sold`]
    /// confirms it, so a failed submission is retried rather than silently
    /// skipped and the position left half-open.
    pub fn due(&self, now_ms: u64) -> Vec<DueLeg> {
        self.positions
            .values()
            .filter(|p| !p.is_done() && now_ms >= p.due_ms)
            .map(|p| {
                let planned = p.plan[p.next_leg].tokens;
                DueLeg {
                    curve: p.curve,
                    mint: p.mint,
                    venue: p.venue,
                    // Never try to sell more than is actually held.
                    tokens: planned.min(p.remaining),
                    leg: p.next_leg + 1,
                    of: p.plan.len(),
                }
            })
            .filter(|d| d.tokens > 0)
            .collect()
    }

    /// Confirm a tranche sold; schedules the next one.
    pub fn on_sold(&mut self, curve: &Pubkey, tokens: u64, proceeds: u64, now_ms: u64) {
        let Some(p) = self.positions.get_mut(curve) else {
            return;
        };
        p.remaining = p.remaining.saturating_sub(tokens);
        p.proceeds = p.proceeds.saturating_add(proceeds);
        p.next_leg += 1;
        if p.next_leg < p.plan.len() {
            p.due_ms = now_ms + p.plan[p.next_leg].delay_slots * SLOT_MS;
        }
    }

    /// Positions that have finished their plan, removed from the book.
    pub fn take_finished(&mut self) -> Vec<Position> {
        let done: Vec<Pubkey> = self
            .positions
            .values()
            .filter(|p| p.is_done())
            .map(|p| p.curve)
            .collect();
        done.iter()
            .filter_map(|k| self.positions.remove(k))
            .collect()
    }

    /// Force-close a position, e.g. on shutdown or a kill switch.
    pub fn abandon(&mut self, curve: &Pubkey) -> Option<Position> {
        self.positions.remove(curve)
    }

    /// Positions held longer than `max_ms` and still not finished. A hold that
    /// long means the exit schedule is not clearing and needs attention.
    pub fn stuck(&self, now_ms: u64, max_ms: u64) -> Vec<Pubkey> {
        self.positions
            .values()
            .filter(|p| !p.is_done() && p.age_ms(now_ms) > max_ms)
            .map(|p| p.curve)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::{plan_exit, ExitConfig};

    fn plan(tokens: u64) -> Vec<Tranche> {
        plan_exit(tokens, &ExitConfig::default())
    }

    fn book_with(tokens: u64, now: u64) -> (Book, Pubkey) {
        let mut b = Book::new();
        let curve = Pubkey::new_unique();
        assert!(b.open(curve, Pubkey::new_unique(), ExitVenue::BondingCurve, tokens, 4_000_000_000, plan(tokens), now));
        (b, curve)
    }

    #[test]
    fn opening_schedules_the_first_leg() {
        let (b, curve) = book_with(1_000_000, 10_000);
        let p = b.get(&curve).unwrap();
        assert_eq!(p.next_leg, 0);
        assert_eq!(p.remaining, 1_000_000);
        // Default first delay is 3 slots.
        assert_eq!(p.due_ms, 10_000 + 3 * SLOT_MS);
    }

    #[test]
    fn nothing_is_due_before_its_time() {
        let (b, _) = book_with(1_000_000, 10_000);
        assert!(b.due(10_000).is_empty());
        assert!(b.due(10_000 + 3 * SLOT_MS - 1).is_empty());
        assert_eq!(b.due(10_000 + 3 * SLOT_MS).len(), 1);
    }

    #[test]
    fn a_due_leg_stays_due_until_confirmed() {
        // A failed submission must be retried, not silently skipped.
        let (mut b, curve) = book_with(1_000_000, 0);
        let t = 3 * SLOT_MS;
        assert_eq!(b.due(t).len(), 1);
        assert_eq!(b.due(t).len(), 1, "still due - nothing confirmed it");
        b.on_sold(&curve, 100, 1, t);
        assert!(b.due(t).is_empty(), "next leg is not due yet");
    }

    #[test]
    fn the_whole_plan_drains_the_position() {
        let tokens = 1_000_000u64;
        let (mut b, curve) = book_with(tokens, 0);
        let mut now = 0u64;
        let mut sold_total = 0u64;
        for _ in 0..20 {
            now += 60 * SLOT_MS;
            let due = b.due(now);
            if due.is_empty() {
                break;
            }
            for d in due {
                sold_total += d.tokens;
                b.on_sold(&d.curve, d.tokens, d.tokens, now);
            }
        }
        assert_eq!(sold_total, tokens, "every token must be sold");
        assert!(b.get(&curve).unwrap().is_done());
        assert_eq!(b.take_finished().len(), 1);
        assert!(b.is_empty());
    }

    #[test]
    fn never_sells_more_than_is_held() {
        let mut b = Book::new();
        let curve = Pubkey::new_unique();
        // A plan that wants more than we hold.
        let fat = vec![Tranche { tokens: 10_000, delay_slots: 0 }];
        b.open(curve, Pubkey::new_unique(), ExitVenue::BondingCurve, 500, 1, fat, 0);
        let due = b.due(0);
        assert_eq!(due[0].tokens, 500);
    }

    #[test]
    fn reopening_the_same_curve_is_refused() {
        let (mut b, curve) = book_with(1_000_000, 0);
        assert!(
            !b.open(curve, Pubkey::new_unique(), ExitVenue::BondingCurve, 999, 1, plan(999), 0),
            "must not clobber a live schedule"
        );
        assert_eq!(b.get(&curve).unwrap().remaining, 1_000_000);
    }

    #[test]
    fn empty_or_zero_positions_are_refused() {
        let mut b = Book::new();
        assert!(!b.open(Pubkey::new_unique(), Pubkey::new_unique(), ExitVenue::BondingCurve, 0, 1, plan(1), 0));
        assert!(!b.open(Pubkey::new_unique(), Pubkey::new_unique(), ExitVenue::BondingCurve, 10, 1, vec![], 0));
        assert!(b.is_empty());
    }

    #[test]
    fn pnl_tracks_proceeds_against_cost() {
        let (mut b, curve) = book_with(1_000_000, 0);
        assert_eq!(b.get(&curve).unwrap().pnl(), -4_000_000_000);
        b.on_sold(&curve, 500_000, 5_000_000_000, 0);
        assert_eq!(b.get(&curve).unwrap().pnl(), 1_000_000_000);
    }

    /// A curve as it actually looks after migration.
    ///
    /// Not a curve with `complete = true` bolted on: pump.fun **zeroes every
    /// reserve** when it migrates. Verified against the collected feed, where
    /// completed curves read `virt_sol=0, virt_tok=0, real_sol=0, real_tok=0`.
    /// That is precisely why quoting one returns 0.
    fn graduated_curve() -> crate::pumpfun::BondingCurve {
        crate::pumpfun::BondingCurve {
            virtual_token_reserves: 0,
            virtual_sol_reserves: 0,
            real_token_reserves: 0,
            real_sol_reserves: 0,
            token_total_supply: 0,
            complete: true,
            creator: None,
        }
    }

    #[test]
    fn a_pumpswap_leg_is_never_priced_against_the_dead_curve() {
        // The bug: a graduated curve quotes ~0, so every completion exit looked
        // like a 100% loss in paper mode.
        let c = graduated_curve();
        assert!(c.complete);
        let tokens = 1_000_000_000_000u64;
        let on_curve = quote_exit_leg(ExitVenue::BondingCurve, &c, tokens);
        let on_amm = quote_exit_leg(ExitVenue::PumpSwap, &c, tokens);
        assert_eq!(on_curve, 0, "a drained curve quotes zero - that was the bug");
        assert!(
            on_amm > 0,
            "a PumpSwap leg must be worth something, got {on_amm}"
        );
    }

    #[test]
    fn a_live_curve_leg_still_prices_on_the_curve() {
        let mut c = crate::pumpfun::BondingCurve {
            virtual_token_reserves: crate::pumpfun::INIT_VIRTUAL_TOKENS as u64,
            virtual_sol_reserves: crate::pumpfun::INIT_VIRTUAL_SOL as u64,
            real_token_reserves: crate::pumpfun::INIT_REAL_TOKENS as u64,
            real_sol_reserves: 0,
            token_total_supply: 1_000_000_000_000_000,
            complete: false,
            creator: None,
        };
        c.apply_buy(20_000_000_000);
        assert!(quote_exit_leg(ExitVenue::BondingCurve, &c, 1_000_000_000) > 0);
    }

    #[test]
    fn a_completion_position_is_marked_for_pumpswap() {
        // Regression: completions used to be routed to pump.fun SellV2 against a
        // curve they had just graduated, so every exit tranche failed forever.
        let mut b = Book::new();
        let curve = Pubkey::new_unique();
        b.open(curve, Pubkey::new_unique(), ExitVenue::PumpSwap, 1_000, 1, plan(1_000), 0);
        assert_eq!(b.get(&curve).unwrap().venue, ExitVenue::PumpSwap);
        assert_eq!(b.due(3 * SLOT_MS)[0].venue, ExitVenue::PumpSwap);
    }

    #[test]
    fn stuck_positions_are_reported() {
        let (b, curve) = book_with(1_000_000, 0);
        assert!(b.stuck(1_000, 60_000).is_empty());
        assert_eq!(b.stuck(120_000, 60_000), vec![curve]);
    }

    #[test]
    fn abandon_removes_and_returns_the_position() {
        let (mut b, curve) = book_with(1_000_000, 0);
        let p = b.abandon(&curve).expect("was open");
        assert_eq!(p.remaining, 1_000_000);
        assert!(b.is_empty());
        assert!(b.abandon(&curve).is_none());
    }

    #[test]
    fn add_leg_grows_the_position_and_keeps_the_plan_consistent() {
        let mut b = Book::new();
        let c = Pubkey::new_unique();
        let ecfg = crate::strategy::ExitConfig::default();
        let plan = crate::strategy::plan_exit(1_000_000, &ecfg);
        assert!(b.open(c, Pubkey::new_unique(), ExitVenue::PumpSwap, 1_000_000, 500, plan, 0));

        assert!(b.add_leg(&c, 400_000, 300, &ecfg, 10));
        let p = b.get(&c).unwrap();
        assert_eq!(p.remaining, 1_400_000);
        assert_eq!(p.cost, 800, "cost is the running total across legs");
        assert_eq!(p.entries.len(), 2);
        // The invariant that matters: what is still scheduled equals what is held.
        let scheduled: u64 = p.plan[p.next_leg..].iter().map(|t| t.tokens).sum();
        assert_eq!(scheduled, p.remaining);
    }

    #[test]
    fn add_leg_holds_the_invariant_at_every_point_in_the_schedule() {
        let ecfg = crate::strategy::ExitConfig::default();
        for fired in 0..4usize {
            let mut b = Book::new();
            let c = Pubkey::new_unique();
            let plan = crate::strategy::plan_exit(1_000_000, &ecfg);
            b.open(c, Pubkey::new_unique(), ExitVenue::PumpSwap, 1_000_000, 500, plan, 0);
            // Fire `fired` tranches.
            for _ in 0..fired {
                let due: Vec<DueLeg> = b.due(u64::MAX);
                let Some(leg) = due.first() else { break };
                b.on_sold(&leg.curve, leg.tokens, 1, 1);
            }
            let held_before = b.get(&c).map(|p| p.remaining).unwrap_or(0);
            if held_before == 0 {
                continue;
            }
            assert!(b.add_leg(&c, 250_000, 100, &ecfg, 5));
            let p = b.get(&c).unwrap();
            let scheduled: u64 = p.plan[p.next_leg..].iter().map(|t| t.tokens).sum();
            assert_eq!(
                scheduled, p.remaining,
                "after firing {fired} tranches the tail must still cover what is held"
            );
        }
    }

    #[test]
    fn add_leg_refuses_when_there_is_nothing_to_add_to() {
        let mut b = Book::new();
        let ecfg = crate::strategy::ExitConfig::default();
        assert!(
            !b.add_leg(&Pubkey::new_unique(), 1, 1, &ecfg, 0),
            "no position: the caller must not book this buy"
        );
        let c = Pubkey::new_unique();
        let plan = crate::strategy::plan_exit(1_000, &ecfg);
        b.open(c, Pubkey::new_unique(), ExitVenue::PumpSwap, 1_000, 1, plan, 0);
        assert!(!b.add_leg(&c, 0, 0, &ecfg, 0), "zero tokens is not a leg");
    }

    #[test]
    fn a_two_leg_position_reports_its_blended_basis() {
        let mut b = Book::new();
        let c = Pubkey::new_unique();
        let ecfg = crate::strategy::ExitConfig::default();
        let plan = crate::strategy::plan_exit(100, &ecfg);
        b.open(c, Pubkey::new_unique(), ExitVenue::PumpSwap, 100, 1_000, plan, 0);
        b.add_leg(&c, 100, 3_000, &ecfg, 1);
        // 4,000 lamports for 200 tokens.
        assert!((b.get(&c).unwrap().avg_cost_per_token() - 20.0).abs() < 1e-9);
    }
}
