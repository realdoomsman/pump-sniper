//! Transaction assembly and submission.
//!
//! ## Why there is no on-chain guard program here
//!
//! The reference wallet deploys its own BPF program whose only job is to abort the
//! transaction when the opportunity is gone, so the tip transfer rolls back. That
//! costs it ~11,140 lamports per miss - 0.57 SOL across 50,718 misses in 15 hours.
//!
//! A Jito bundle achieves the same thing for free: if any transaction in the bundle
//! fails, the whole bundle is dropped and *nothing* reaches the chain, tip included.
//! So the default path here is bundle-first, and the direct spray is the fallback
//! for the leaders where bundles are not available. Deploying a guard program only
//! becomes worthwhile if you want to spray at a rate bundles will not accept.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_sdk::hash::Hash;
use solana_sdk::instruction::Instruction;
use solana_sdk::message::{v0, AddressLookupTableAccount, VersionedMessage};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signer::keypair::Keypair;
use solana_sdk::signer::Signer;
use solana_system_interface::instruction as system_instruction;
use solana_sdk::transaction::VersionedTransaction;
use std::str::FromStr;
use std::sync::LazyLock;

/// A submission venue: where to send, and which accounts its tip must go to.
#[derive(Debug, Clone)]
pub struct Relay {
    pub name: &'static str,
    /// Accounts that will accept this relay's tip.
    pub tip_accounts: &'static [&'static str],
    /// True when the relay drops the whole submission on failure, so a miss is free.
    pub revert_protected: bool,
}

/// Canonical Jito tip accounts. A bundle must tip one of these.
pub const JITO_TIPS: &[&str] = &[
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGbaLLL",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ABiuoxq1XJ8jFYqzHRoBaHZmqZBHJEqW1CUNiKcuNXAk",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];

/// Tip destinations observed being paid by the reference wallet on mainnet.
///
/// Attribution beyond Jito is inferred from address vanity prefixes and has not
/// been verified against the operators' published lists - treat the names as
/// labels, not facts, and confirm before wiring real money to them.
pub const OBSERVED_TIPS: &[(&str, &str)] = &[
    ("TEMPaMeCRFAS9EKF53Jd6KpHxgL47uWLcpFArU1Fanq", "temporal"),
    ("nozFrhfnNGoyqwVuwPAW4aaGqempx4PU6g6D9CJMv7Z", "nozomi"),
    ("noz6uoYCDijhu1V7cutCpwxNiSovEwLdRHPwmgCGDNo", "nozomi"),
    ("nozievPk7HyK1Rqy1MPJwVQ7qQg2QoJGyP71oeDwbsu", "nozomi"),
    ("AstzCZCcETW8ju3xCnJhcSQmHjQKw6SvpBduezgm1VeV", "astralane"),
    ("HEL1USMZKAL2odpNBj2oCjffnFGaYwmbGmyewGv1e2TU", "helius-sender"),
    ("rapidXMVLw5uBieKHDGvF9k4xSSDXyD2FC5wLTAajaJ", "unverified"),
    ("9bnz4RShgq1hAnLnZbP8kbgBg1kEmcJBYQq3gQbmnSta", "unverified"),
    ("2nyhqdwKcJZR2vcqCyrYsaPVdAnFoJjiksCXJ7hfEYgD", "unverified"),
    ("2q5pghRs6arqVjRvT5gfgWfWcHWmw1ZuCzphgd5KfWGJ", "unverified"),
    ("DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL", "unverified"),
    ("HRyRhQ86t3H4aAtgvHVpUJmw64BDrb61gRiKcdKUXs5c", "unverified"),
];

/// Marker account that asks Jito searchers not to front-run this transaction.
/// The reference wallet carries it in its lookup table.
pub static JITO_DONT_FRONT: LazyLock<Pubkey> =
    LazyLock::new(|| Pubkey::from_str("jitodontfront111111111111111111111111111112").unwrap());

pub static JITO_TIP_KEYS: LazyLock<Vec<Pubkey>> =
    LazyLock::new(|| JITO_TIPS.iter().map(|s| Pubkey::from_str(s).unwrap()).collect());

/// Pick a tip account. Spreading across accounts avoids write-lock contention on
/// any single one, which is a real source of lost races.
pub fn pick_tip_account(nonce: u64) -> Pubkey {
    JITO_TIP_KEYS[(nonce as usize) % JITO_TIP_KEYS.len()]
}

// ---------------------------------------------------------------------------
// Fee ladder
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeeConfig {
    /// Compute unit limit for a single-leg play: one buy, or one sell.
    ///
    /// Priority fee is charged on what you REQUEST, not on what you use, so this is
    /// a real cost - but a tiny one. Under-requesting is not a tiny cost: the
    /// transaction fails with `ExceededMaxComputeUnits` after the fee is paid.
    pub cu_limit: u32,
    /// Compute unit limit for the atomic completion: `BuyExactSolIn` + `MigrateV2`
    /// in one transaction.
    ///
    /// This must be much larger, and the default here used to be far too small to
    /// land at all. A `MigrateV2` on its own was observed consuming **354,648 CU**,
    /// and the wallet that sent it requested 714,286. Our completion does that plus
    /// a buy. At the tier-0 CU price, the difference between requesting 260k and
    /// 600k is about 0.000017 SOL - there is no reason to be tight here.
    #[serde(default = "default_cu_limit_completion")]
    pub cu_limit_completion: u32,
    /// Escalation tiers, micro-lamports per compute unit.
    pub cu_price_tiers: Vec<u64>,
    /// Escalation tiers for the relay tip, in lamports.
    pub tip_tiers: Vec<u64>,
    /// Never let one attempt's fee plus tip exceed this fraction of the order.
    pub max_fee_fraction: f64,
}

/// Safe fallback for configs written before the completion budget was split out.
///
/// Defaulting matters here: the old single `cu_limit` was 260,000, which is below
/// what a bare `MigrateV2` was observed to consume. Silently inheriting that value
/// would leave an existing deployment unable to land the only play it has.
fn default_cu_limit_completion() -> u32 {
    600_000
}

impl Default for FeeConfig {
    fn default() -> Self {
        // Deliberately far below the reference wallet, because the competitor data
        // says its spending does not buy wins.
        //
        // Sampling 79 transactions against the network-wide pump.fun migration
        // index: the transactions that LANDED paid a median 0.000055 SOL fee and a
        // median tip of zero. The ones that FAILED paid a median 0.0002 SOL tip,
        // topping out at 0.5618 SOL of tip and 0.014 SOL of priority fee. Paying
        // more correlated with losing, not winning - the wallets that reliably land
        // migrations do it on latency and position, not on price.
        //
        // So tier 0 is the working default and the ladder exists for the rare
        // genuinely contested block, not as a starting point.
        Self {
            cu_limit: 120_000,
            cu_limit_completion: 600_000,
            cu_price_tiers: vec![50_000, 500_000, 5_000_000, 50_000_000],
            tip_tiers: vec![10_000, 1_000_000, 10_000_000, 100_000_000],
            max_fee_fraction: 0.15,
        }
    }
}

impl FeeConfig {
    /// Priority fee in lamports for a tier, from CU price and the requested limit.
    ///
    /// Takes the limit explicitly because it differs per play - a completion
    /// requests five times what a bare sell does, and pays five times the priority
    /// fee for it.
    pub fn priority_fee(&self, tier: usize, cu_limit: u32) -> u64 {
        let price = self
            .cu_price_tiers
            .get(tier)
            .copied()
            .unwrap_or_else(|| self.cu_price_tiers.last().copied().unwrap_or(0));
        ((price as u128 * cu_limit as u128) / 1_000_000) as u64
    }

    pub fn tip(&self, tier: usize) -> u64 {
        self.tip_tiers
            .get(tier)
            .copied()
            .unwrap_or_else(|| self.tip_tiers.last().copied().unwrap_or(0))
    }

    /// Total cost of one attempt at a tier, for a given compute budget.
    pub fn attempt_cost(&self, tier: usize, cu_limit: u32) -> u64 {
        5_000 + self.priority_fee(tier, cu_limit) + self.tip(tier)
    }

    /// Compute budget for a play. `atomic` means buy + migrate in one transaction.
    pub fn cu_limit_for(&self, atomic: bool) -> u32 {
        if atomic {
            self.cu_limit_completion
        } else {
            self.cu_limit
        }
    }

    /// Highest tier whose cost stays within `max_fee_fraction` of the order.
    ///
    /// Depends on the compute budget, because the priority fee is charged on the
    /// limit you request: a completion asks for five times the CUs of a bare sell,
    /// so at the same tier it costs five times as much and may not be affordable.
    pub fn max_affordable_tier(&self, order_lamports: u64, cu_limit: u32) -> Option<usize> {
        let budget = (order_lamports as f64 * self.max_fee_fraction) as u64;
        (0..self.cu_price_tiers.len().max(self.tip_tiers.len()))
            .filter(|t| self.attempt_cost(*t, cu_limit) <= budget)
            .next_back()
    }

    pub fn tiers(&self) -> usize {
        self.cu_price_tiers.len().max(self.tip_tiers.len())
    }
}

// ---------------------------------------------------------------------------
// Transaction assembly
// ---------------------------------------------------------------------------

/// Everything needed to put one attempt on the wire.
pub struct AttemptSpec<'a> {
    pub payer: &'a Keypair,
    pub blockhash: Hash,
    /// The venue instructions (buy, migrate, sell...).
    pub body: Vec<Instruction>,
    pub fee: &'a FeeConfig,
    pub tier: usize,
    /// Compute budget for THIS attempt. Build it with [`FeeConfig::cu_limit_for`].
    pub cu_limit: u32,
    /// Set for bundle submission; `None` skips the tip transfer entirely.
    pub tip_account: Option<Pubkey>,
    pub luts: Vec<AddressLookupTableAccount>,
}

/// Assemble and sign one versioned transaction.
///
/// Instruction order matters: compute budget first so the fee is set before any
/// work, then the tip, then the body.
pub fn build_attempt(spec: AttemptSpec<'_>) -> Result<VersionedTransaction> {
    let mut ixs = Vec::with_capacity(spec.body.len() + 3);
    ixs.push(ComputeBudgetInstruction::set_compute_unit_limit(
        spec.cu_limit,
    ));
    let price = spec
        .fee
        .cu_price_tiers
        .get(spec.tier)
        .copied()
        .unwrap_or_else(|| spec.fee.cu_price_tiers.last().copied().unwrap_or(0));
    ixs.push(ComputeBudgetInstruction::set_compute_unit_price(price));

    if let Some(tip) = spec.tip_account {
        let amount = spec.fee.tip(spec.tier);
        if amount > 0 {
            ixs.push(system_instruction::transfer(
                &spec.payer.pubkey(),
                &tip,
                amount,
            ));
        }
    }
    ixs.extend(spec.body);

    let msg = v0::Message::try_compile(&spec.payer.pubkey(), &ixs, &spec.luts, spec.blockhash)
        .context("compiling v0 message")?;
    VersionedTransaction::try_new(VersionedMessage::V0(msg), &[spec.payer])
        .context("signing transaction")
}

/// Parse a lookup table fetched from chain into the form `try_compile` wants.
///
/// A completion attempt touches ~30 accounts across pump.fun and PumpSwap; without
/// a lookup table the transaction does not fit in a packet.
pub fn parse_lookup_table(key: Pubkey, data: &[u8]) -> Result<AddressLookupTableAccount> {
    let table = solana_address_lookup_table_interface::state::AddressLookupTable::deserialize(data)
        .context("deserializing address lookup table")?;
    Ok(AddressLookupTableAccount {
        key,
        addresses: table.addresses.to_vec(),
    })
}

/// Serialize a signed transaction to base64 for JSON-RPC / bundle submission.
pub fn encode_tx(tx: &VersionedTransaction) -> Result<String> {
    use base64::Engine;
    let bytes = bincode::serialize(tx).context("serializing transaction")?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jito_tip_list_is_the_canonical_eight() {
        assert_eq!(JITO_TIPS.len(), 8);
        assert_eq!(JITO_TIP_KEYS.len(), 8);
    }

    #[test]
    fn every_hardcoded_tip_address_parses() {
        for s in JITO_TIPS {
            Pubkey::from_str(s).unwrap_or_else(|_| panic!("bad jito tip {s}"));
        }
        for (s, _) in OBSERVED_TIPS {
            Pubkey::from_str(s).unwrap_or_else(|_| panic!("bad observed tip {s}"));
        }
        let _ = *JITO_DONT_FRONT;
    }

    #[test]
    fn tip_selection_spreads_across_accounts() {
        let picks: std::collections::HashSet<_> = (0..8u64).map(pick_tip_account).collect();
        assert_eq!(picks.len(), 8, "should cycle every tip account");
    }

    #[test]
    fn the_completion_budget_covers_an_observed_migration() {
        let f = FeeConfig::default();
        // A landed MigrateV2 was observed consuming 354,648 CU on its own, and the
        // wallet that sent it requested 714,286. Our completion carries a buy as
        // well. The default was 260,000 for most of this project's life, which
        // means the completion - the entire strategy - could never have landed.
        assert!(
            f.cu_limit_completion >= 450_000,
            "completion budget {} is below an observed migration",
            f.cu_limit_completion
        );
        assert!(f.cu_limit_for(true) > f.cu_limit_for(false));
        // And the safety is nearly free: pennies at the working tier.
        let extra = f.priority_fee(0, f.cu_limit_completion) - f.priority_fee(0, f.cu_limit);
        assert!(extra < 100_000, "headroom costs {extra} lamports");
    }

    #[test]
    fn priority_fee_matches_the_cu_price_formula() {
        let f = FeeConfig {
            cu_limit: 1_000_000,
            cu_price_tiers: vec![1_000_000],
            ..Default::default()
        };
        // 1e6 micro-lamports/CU * 1e6 CU = 1e12 micro-lamports = 1e6 lamports.
        assert_eq!(f.priority_fee(0, f.cu_limit), 1_000_000);
    }

    #[test]
    fn tiers_escalate_monotonically() {
        let f = FeeConfig::default();
        for t in 1..f.tiers() {
            assert!(
                f.attempt_cost(t, f.cu_limit) > f.attempt_cost(t - 1, f.cu_limit),
                "tier {t} must cost more than {}",
                t - 1
            );
        }
    }

    #[test]
    fn out_of_range_tier_saturates_at_the_top() {
        let f = FeeConfig::default();
        assert_eq!(f.tip(999), *f.tip_tiers.last().unwrap());
        assert_eq!(
            f.priority_fee(999, f.cu_limit),
            f.priority_fee(f.tiers() - 1, f.cu_limit)
        );
    }

    #[test]
    fn default_tier_zero_is_cheaper_than_the_observed_losers() {
        // Losing migration attempts paid a median 0.0002 SOL tip. Our opening bid
        // must sit under that or we are buying the losing side of the trade.
        let f = FeeConfig::default();
        assert!(
            f.attempt_cost(0, f.cu_limit) < 200_000,
            "tier 0 costs {} lamports, above the observed losing median",
            f.attempt_cost(0, f.cu_limit)
        );
    }

    #[test]
    fn fee_budget_caps_the_tier() {
        let f = FeeConfig::default();
        // A tiny order cannot afford any tier.
        assert_eq!(f.max_affordable_tier(1_000, f.cu_limit), None);
        // A large order can afford the top tier.
        assert_eq!(
            f.max_affordable_tier(100_000_000_000, f.cu_limit),
            Some(f.tiers() - 1)
        );
    }

    #[test]
    fn build_attempt_signs_and_encodes() {
        let payer = Keypair::new();
        let fee = FeeConfig::default();
        let ix = system_instruction::transfer(&payer.pubkey(), &Pubkey::new_unique(), 1);
        let tx = build_attempt(AttemptSpec {
            cu_limit: 200_000,
            payer: &payer,
            blockhash: Hash::default(),
            body: vec![ix],
            fee: &fee,
            tier: 0,
            tip_account: Some(pick_tip_account(0)),
            luts: vec![],
        })
        .expect("build");
        assert_eq!(tx.signatures.len(), 1);
        // compute limit + compute price + tip + body
        assert_eq!(tx.message.instructions().len(), 4);
        assert!(!encode_tx(&tx).unwrap().is_empty());
    }

    #[test]
    fn tipless_build_omits_the_transfer() {
        let payer = Keypair::new();
        let fee = FeeConfig::default();
        let ix = system_instruction::transfer(&payer.pubkey(), &Pubkey::new_unique(), 1);
        let tx = build_attempt(AttemptSpec {
            cu_limit: 200_000,
            payer: &payer,
            blockhash: Hash::default(),
            body: vec![ix],
            fee: &fee,
            tier: 0,
            tip_account: None,
            luts: vec![],
        })
        .expect("build");
        assert_eq!(tx.message.instructions().len(), 3);
    }
}
