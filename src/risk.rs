//! Hard limits that sit between a decision and a signature.
//!
//! Every check here is a refusal, never an adjustment. If a rule trips, the order
//! does not go out in a smaller size - it does not go out.

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskConfig {
    /// Largest single order, gross lamports.
    pub max_order_lamports: u64,
    /// Largest total exposure across open positions, lamports.
    pub max_exposure_lamports: u64,
    /// Stop trading for the day once realised losses reach this, lamports.
    pub daily_loss_limit_lamports: u64,
    /// Maximum concurrent open positions.
    pub max_open_positions: usize,
    /// Minimum wallet balance to keep in reserve for fees, lamports.
    pub min_reserve_lamports: u64,
    /// Refuse to fire more than this many orders per minute.
    pub max_orders_per_min: usize,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            max_order_lamports: 45_000_000_000,
            max_exposure_lamports: 45_000_000_000,
            daily_loss_limit_lamports: 10_000_000_000,
            max_open_positions: 1,
            min_reserve_lamports: 500_000_000,
            max_orders_per_min: 12,
        }
    }
}

/// Why an order was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    KillSwitch,
    OrderTooLarge { gross: u64, limit: u64 },
    ExposureExceeded { would_be: u64, limit: u64 },
    DailyLossLimit { lost: u64, limit: u64 },
    TooManyPositions { open: usize, limit: usize },
    InsufficientBalance { available: u64, needed: u64 },
    RateLimited { in_last_min: usize, limit: usize },
}

#[derive(Debug)]
struct State {
    killed: bool,
    exposure: u64,
    realised_loss: u64,
    open: usize,
    recent_orders: Vec<Instant>,
}

/// Shared, thread-safe risk gate.
pub struct RiskGate {
    cfg: RiskConfig,
    st: Mutex<State>,
}

impl RiskGate {
    pub fn new(cfg: RiskConfig) -> Self {
        Self {
            cfg,
            st: Mutex::new(State {
                killed: false,
                exposure: 0,
                realised_loss: 0,
                open: 0,
                recent_orders: Vec::new(),
            }),
        }
    }

    pub fn config(&self) -> &RiskConfig {
        &self.cfg
    }

    /// Trip the kill switch. Irreversible for the life of the process.
    pub fn kill(&self) {
        self.st.lock().killed = true;
    }

    pub fn is_killed(&self) -> bool {
        self.st.lock().killed
    }

    /// Lamports currently reserved across all open positions.
    pub fn exposure(&self) -> u64 {
        self.st.lock().exposure
    }

    /// How many positions hold a slot right now.
    pub fn open_positions(&self) -> usize {
        self.st.lock().open
    }

    /// Reserve room for a SECOND entry on a position that is already open.
    ///
    /// Not the same check as [`Self::reserve`], in two ways that matter.
    ///
    /// It skips `TooManyPositions`, because the first leg already consumed the
    /// only slot under `max_open_positions = 1`; running the normal check here
    /// would refuse every follow-on by construction.
    ///
    /// And it tests `position_total`, not `extra`, against `max_order_lamports`.
    /// Checking only the increment would let two 0.15 SOL legs quietly build a
    /// 0.30 SOL position under a 0.2 SOL cap - the exact way a per-order limit
    /// stops meaning anything.
    ///
    /// It does not increment `open`: the position is already counted.
    pub fn add_exposure(
        &self,
        position_total: u64,
        extra: u64,
        balance: u64,
    ) -> Result<(), Refusal> {
        let mut s = self.st.lock();
        if s.killed {
            return Err(Refusal::KillSwitch);
        }
        if position_total > self.cfg.max_order_lamports {
            return Err(Refusal::OrderTooLarge {
                gross: position_total,
                limit: self.cfg.max_order_lamports,
            });
        }
        if s.realised_loss >= self.cfg.daily_loss_limit_lamports {
            return Err(Refusal::DailyLossLimit {
                lost: s.realised_loss,
                limit: self.cfg.daily_loss_limit_lamports,
            });
        }
        let would_be = s.exposure.saturating_add(extra);
        if would_be > self.cfg.max_exposure_lamports {
            return Err(Refusal::ExposureExceeded {
                would_be,
                limit: self.cfg.max_exposure_lamports,
            });
        }
        let needed = extra.saturating_add(self.cfg.min_reserve_lamports);
        if balance < needed {
            return Err(Refusal::InsufficientBalance {
                available: balance,
                needed,
            });
        }
        let cutoff = Instant::now() - Duration::from_secs(60);
        s.recent_orders.retain(|t| *t >= cutoff);
        if s.recent_orders.len() >= self.cfg.max_orders_per_min {
            return Err(Refusal::RateLimited {
                in_last_min: s.recent_orders.len(),
                limit: self.cfg.max_orders_per_min,
            });
        }
        s.exposure = would_be;
        s.recent_orders.push(Instant::now());
        Ok(())
    }

    /// Give back exposure taken by [`Self::add_exposure`] when the leg never went
    /// out. Does not touch `open` - the first leg still holds that slot.
    pub fn release_exposure(&self, extra: u64) {
        let mut s = self.st.lock();
        s.exposure = s.exposure.saturating_sub(extra);
    }

    /// Check an order without reserving anything.
    pub fn check(&self, gross: u64, balance: u64) -> Result<(), Refusal> {
        let mut s = self.st.lock();
        Self::check_locked(&self.cfg, &mut s, gross, balance)
    }

    fn check_locked(
        cfg: &RiskConfig,
        s: &mut State,
        gross: u64,
        balance: u64,
    ) -> Result<(), Refusal> {
        if s.killed {
            return Err(Refusal::KillSwitch);
        }
        if gross > cfg.max_order_lamports {
            return Err(Refusal::OrderTooLarge {
                gross,
                limit: cfg.max_order_lamports,
            });
        }
        if s.realised_loss >= cfg.daily_loss_limit_lamports {
            return Err(Refusal::DailyLossLimit {
                lost: s.realised_loss,
                limit: cfg.daily_loss_limit_lamports,
            });
        }
        if s.open >= cfg.max_open_positions {
            return Err(Refusal::TooManyPositions {
                open: s.open,
                limit: cfg.max_open_positions,
            });
        }
        let would_be = s.exposure.saturating_add(gross);
        if would_be > cfg.max_exposure_lamports {
            return Err(Refusal::ExposureExceeded {
                would_be,
                limit: cfg.max_exposure_lamports,
            });
        }
        let needed = gross.saturating_add(cfg.min_reserve_lamports);
        if balance < needed {
            return Err(Refusal::InsufficientBalance {
                available: balance,
                needed,
            });
        }
        let cutoff = Instant::now() - Duration::from_secs(60);
        s.recent_orders.retain(|t| *t >= cutoff);
        if s.recent_orders.len() >= cfg.max_orders_per_min {
            return Err(Refusal::RateLimited {
                in_last_min: s.recent_orders.len(),
                limit: cfg.max_orders_per_min,
            });
        }
        Ok(())
    }

    /// Check and, if it passes, reserve the exposure. Call this on the hot path.
    pub fn reserve(&self, gross: u64, balance: u64) -> Result<(), Refusal> {
        let mut s = self.st.lock();
        Self::check_locked(&self.cfg, &mut s, gross, balance)?;
        s.exposure = s.exposure.saturating_add(gross);
        s.open += 1;
        s.recent_orders.push(Instant::now());
        Ok(())
    }

    /// Release a position. `pnl` is signed lamports.
    pub fn settle(&self, gross: u64, pnl: i64) {
        let mut s = self.st.lock();
        s.exposure = s.exposure.saturating_sub(gross);
        s.open = s.open.saturating_sub(1);
        if pnl < 0 {
            s.realised_loss = s.realised_loss.saturating_add(pnl.unsigned_abs());
            if s.realised_loss >= self.cfg.daily_loss_limit_lamports {
                s.killed = true;
            }
        }
    }

    pub fn snapshot(&self) -> (u64, u64, usize, bool) {
        let s = self.st.lock();
        (s.exposure, s.realised_loss, s.open, s.killed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate() -> RiskGate {
        RiskGate::new(RiskConfig::default())
    }

    #[test]
    fn ordinary_order_passes() {
        assert!(gate().check(1_000_000_000, 50_000_000_000).is_ok());
    }

    #[test]
    fn oversized_order_is_refused() {
        let g = gate();
        assert!(matches!(
            g.check(999_000_000_000, 999_999_000_000_000),
            Err(Refusal::OrderTooLarge { .. })
        ));
    }

    #[test]
    fn reserve_must_be_left_for_fees() {
        let g = gate();
        // Balance exactly equals the order: no room for the fee reserve.
        assert!(matches!(
            g.check(1_000_000_000, 1_000_000_000),
            Err(Refusal::InsufficientBalance { .. })
        ));
    }

    #[test]
    fn second_position_is_refused_by_default() {
        let g = gate();
        g.reserve(1_000_000_000, 50_000_000_000).unwrap();
        assert!(matches!(
            g.check(1_000_000_000, 50_000_000_000),
            Err(Refusal::TooManyPositions { .. })
        ));
    }

    #[test]
    fn settling_frees_the_slot() {
        let g = gate();
        g.reserve(1_000_000_000, 50_000_000_000).unwrap();
        g.settle(1_000_000_000, 500_000_000);
        assert!(g.check(1_000_000_000, 50_000_000_000).is_ok());
        assert_eq!(g.snapshot().0, 0);
    }

    #[test]
    fn daily_loss_limit_trips_the_kill_switch() {
        let g = RiskGate::new(RiskConfig {
            daily_loss_limit_lamports: 1_000_000_000,
            ..Default::default()
        });
        g.reserve(2_000_000_000, 50_000_000_000).unwrap();
        g.settle(2_000_000_000, -1_500_000_000);
        assert!(g.is_killed());
        assert!(matches!(
            g.check(1_000, 50_000_000_000),
            Err(Refusal::KillSwitch)
        ));
    }

    #[test]
    fn kill_switch_blocks_everything() {
        let g = gate();
        g.kill();
        assert!(matches!(
            g.check(1, 50_000_000_000),
            Err(Refusal::KillSwitch)
        ));
    }

    #[test]
    fn rate_limit_applies_within_the_minute() {
        let g = RiskGate::new(RiskConfig {
            max_orders_per_min: 2,
            max_open_positions: 100,
            ..Default::default()
        });
        g.reserve(1_000_000, 50_000_000_000).unwrap();
        g.reserve(1_000_000, 50_000_000_000).unwrap();
        assert!(matches!(
            g.check(1_000_000, 50_000_000_000),
            Err(Refusal::RateLimited { .. })
        ));
    }

    #[test]
    fn add_exposure_caps_the_whole_position_not_just_the_increment() {
        // The silent-overspend case. Two legs inside the per-order cap must not be
        // able to build a position above it.
        let cfg = RiskConfig {
            max_order_lamports: 200_000_000,
            max_exposure_lamports: 200_000_000,
            ..RiskConfig::default()
        };
        let g = RiskGate::new(cfg);
        let bal = 10_000_000_000;
        assert!(g.reserve(150_000_000, bal).is_ok());
        // 150m held + 100m more = 250m, over the 200m per-order cap.
        assert!(matches!(
            g.add_exposure(250_000_000, 100_000_000, bal),
            Err(Refusal::OrderTooLarge { .. })
        ));
        // A second leg that keeps the position inside the cap is fine.
        assert!(g.add_exposure(200_000_000, 50_000_000, bal).is_ok());
    }

    #[test]
    fn add_exposure_ignores_the_open_position_limit() {
        // max_open_positions = 1 means the first leg already took the only slot.
        // Running the ordinary check here would refuse every follow-on forever.
        let g = RiskGate::new(RiskConfig {
            max_open_positions: 1,
            ..RiskConfig::default()
        });
        let bal = 100_000_000_000;
        assert!(g.reserve(1_000_000_000, bal).is_ok());
        assert!(matches!(
            g.check(1_000_000_000, bal),
            Err(Refusal::TooManyPositions { .. })
        ));
        assert!(
            g.add_exposure(2_000_000_000, 1_000_000_000, bal).is_ok(),
            "a second leg on an existing position must not be blocked by the slot limit"
        );
    }

    #[test]
    fn a_two_leg_position_settles_back_to_zero() {
        let g = RiskGate::new(RiskConfig::default());
        let bal = 100_000_000_000;
        g.reserve(1_000_000_000, bal).unwrap();
        g.add_exposure(3_000_000_000, 2_000_000_000, bal).unwrap();
        g.settle(3_000_000_000, 0);
        assert_eq!(g.exposure(), 0, "exposure must return to zero");
        assert_eq!(g.open_positions(), 0, "and the slot must be given back");
    }

    #[test]
    fn release_exposure_gives_back_only_the_failed_leg() {
        let g = RiskGate::new(RiskConfig::default());
        let bal = 100_000_000_000;
        g.reserve(1_000_000_000, bal).unwrap();
        g.add_exposure(3_000_000_000, 2_000_000_000, bal).unwrap();
        // The second leg never went out.
        g.release_exposure(2_000_000_000);
        assert_eq!(g.exposure(), 1_000_000_000, "leg one still holds its reservation");
        assert_eq!(g.open_positions(), 1, "and still holds the slot");
    }

    #[test]
    fn add_exposure_still_honours_the_kill_switch_and_the_daily_loss_limit() {
        let g = RiskGate::new(RiskConfig::default());
        let bal = 100_000_000_000;
        g.reserve(1_000_000_000, bal).unwrap();
        g.kill();
        assert!(matches!(
            g.add_exposure(2_000_000_000, 1_000_000_000, bal),
            Err(Refusal::KillSwitch)
        ));
    }
}
