//! Configuration, and the gate that stands between paper and real money.

use crate::exec::FeeConfig;
use crate::risk::RiskConfig;
use crate::strategy::{ExitConfig, StrategyConfig};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// How orders are executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Decide and log, never sign. The default, and the only mode that needs no key.
    Paper,
    /// Sign and submit for real.
    Live,
}

impl Default for Mode {
    fn default() -> Self {
        Self::Paper
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcConfig {
    /// HTTP JSON-RPC endpoint.
    pub http: String,
    /// WebSocket endpoint for account subscriptions.
    pub ws: String,
    /// Jito block-engine base URL, for bundle submission.
    pub jito_block_engine: Option<String>,
    /// Address lookup table covering the constant accounts.
    ///
    /// **Required for the atomic completion play.** Without one the buy+migrate
    /// transaction is 1,484 bytes against a 1,232-byte limit and cannot be sent.
    /// Create one with `sniper create-alt`.
    pub lookup_table: Option<String>,
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            http: "https://api.mainnet-beta.solana.com".into(),
            ws: "wss://api.mainnet-beta.solana.com".into(),
            jito_block_engine: None,
            lookup_table: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub mode: Mode,
    pub rpc: RpcConfig,
    /// Path to the signing keypair. Required only in live mode.
    pub keypair_path: Option<String>,
    pub strategy: StrategyConfig,
    pub exit: ExitConfig,
    pub fees: FeeConfig,
    pub risk: RiskConfig,
    /// Explicit, deliberate opt-in. Live mode refuses to start without it.
    pub i_understand_this_spends_real_money: bool,
    /// Directory for the learned state: the model and the reputation store.
    pub data_dir: Option<String>,
    /// Allow live completion trades even though the PumpSwap exit is unbuilt.
    ///
    /// A completion graduates the curve, and pump.fun refuses sells on a graduated
    /// curve, so such a position currently cannot be closed by the bot. Off by
    /// default: taking a trade with no exit is worse than not trading.
    pub allow_unexitable_completions: bool,
}

impl Config {
    fn dir(&self) -> std::path::PathBuf {
        std::path::PathBuf::from(self.data_dir.clone().unwrap_or_else(|| "data".into()))
    }
    pub fn model_path(&self) -> std::path::PathBuf {
        self.dir().join("model.json")
    }
    pub fn smart_path(&self) -> std::path::PathBuf {
        self.dir().join("smart.json")
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config at {}", path.display()))?;
        let cfg: Config = toml::from_str(&raw).context("parsing config TOML")?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Reject configurations that would lose money by accident.
    pub fn validate(&self) -> Result<()> {
        if self.rpc.http.is_empty() || self.rpc.ws.is_empty() {
            bail!("rpc.http and rpc.ws must both be set");
        }
        if self.strategy.max_gap_lamports == 0 {
            bail!("strategy.max_gap_lamports is 0 - nothing would ever trigger");
        }
        if self.strategy.max_position_lamports > self.risk.max_order_lamports {
            bail!(
                "strategy.max_position_lamports ({}) exceeds risk.max_order_lamports ({}); \
                 the risk gate would refuse every order the strategy produces",
                self.strategy.max_position_lamports,
                self.risk.max_order_lamports
            );
        }
        if self.fees.cu_price_tiers.is_empty() || self.fees.tip_tiers.is_empty() {
            bail!("fees must define at least one cu_price tier and one tip tier");
        }
        // A landed MigrateV2 alone was observed at 354,648 CU, and the completion
        // carries a buy on top. Below this the play cannot land at all, and the
        // failure looks like ordinary bad luck rather than a misconfiguration.
        if self.fees.cu_limit_completion < 450_000 {
            bail!(
                "fees.cu_limit_completion is {}, below the ~400k a buy+migrate needs; \
                 the completion could never land",
                self.fees.cu_limit_completion
            );
        }
        if !(0.0..=1.0).contains(&self.fees.max_fee_fraction) {
            bail!("fees.max_fee_fraction must be between 0.0 and 1.0");
        }
        if self.exit.tranches == 0 {
            bail!("exit.tranches must be at least 1");
        }
        if self.exit.front_load <= 0.0 {
            bail!("exit.front_load must be positive");
        }

        if self.mode == Mode::Live {
            if !self.i_understand_this_spends_real_money {
                bail!(
                    "live mode requires i_understand_this_spends_real_money = true in the config"
                );
            }
            if self.keypair_path.is_none() {
                bail!("live mode requires keypair_path");
            }
            if self.rpc.http.contains("api.mainnet-beta.solana.com") {
                bail!(
                    "live mode on the public RPC will lose every race and get rate-limited; \
                     point rpc.http at a paid endpoint"
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Config {
        Config {
            mode: Mode::Paper,
            rpc: RpcConfig::default(),
            keypair_path: None,
            strategy: StrategyConfig::default(),
            exit: ExitConfig::default(),
            fees: FeeConfig::default(),
            risk: RiskConfig::default(),
            i_understand_this_spends_real_money: false,
            data_dir: None,
            allow_unexitable_completions: false,
        }
    }

    #[test]
    fn defaults_are_valid_and_paper() {
        let c = base();
        assert_eq!(c.mode, Mode::Paper);
        c.validate().unwrap();
    }

    #[test]
    fn live_without_acknowledgement_is_refused() {
        let mut c = base();
        c.mode = Mode::Live;
        c.keypair_path = Some("k.json".into());
        c.rpc.http = "https://paid.example".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn live_without_keypair_is_refused() {
        let mut c = base();
        c.mode = Mode::Live;
        c.i_understand_this_spends_real_money = true;
        c.rpc.http = "https://paid.example".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn live_on_public_rpc_is_refused() {
        let mut c = base();
        c.mode = Mode::Live;
        c.i_understand_this_spends_real_money = true;
        c.keypair_path = Some("k.json".into());
        assert!(c.validate().is_err());
    }

    #[test]
    fn fully_configured_live_passes() {
        let mut c = base();
        c.mode = Mode::Live;
        c.i_understand_this_spends_real_money = true;
        c.keypair_path = Some("k.json".into());
        c.rpc.http = "https://mainnet.helius-rpc.com/?api-key=x".into();
        c.validate().unwrap();
    }

    #[test]
    fn strategy_larger_than_risk_limit_is_refused() {
        let mut c = base();
        c.strategy.max_position_lamports = c.risk.max_order_lamports + 1;
        assert!(c.validate().is_err());
    }

    #[test]
    fn config_round_trips_through_toml() {
        let c = base();
        let s = toml::to_string(&c).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        back.validate().unwrap();
        assert_eq!(back.mode, c.mode);
        assert_eq!(
            back.strategy.max_gap_lamports,
            c.strategy.max_gap_lamports
        );
    }
}
