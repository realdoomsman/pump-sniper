//! pump.fun bonding-curve completion sniper.
//!
//! The library half: curve math, live state tracking, strategy, risk and
//! execution primitives. The binary in `main.rs` wires them together.
//!
//! Start with [`pumpfun::graduation_lamports`] and [`strategy::evaluate`] — the
//! rest exists to serve those two.
//!
//! Background and measurements: `docs/RESEARCH.md`.

pub mod backtest;
pub mod config;
pub mod events;
pub mod exec;
pub mod feed;
pub mod live;
pub mod poolwatch;
pub mod positions;
pub mod pumpfun;
pub mod pumpswap;
pub mod resolve;
pub mod risk;
pub mod score;
pub mod smart;
pub mod rpc;
pub mod strategy;
pub mod tracker;
pub mod train;

/// Lamports to SOL, for display only.
pub fn sol(lamports: u64) -> f64 {
    lamports as f64 / 1e9
}

#[cfg(test)]
mod tests {
    #[test]
    fn sol_conversion() {
        assert!((super::sol(1_500_000_000) - 1.5).abs() < f64::EPSILON);
        assert_eq!(super::sol(0), 0.0);
    }
}
