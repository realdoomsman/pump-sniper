//! pump.fun bonding-curve state, math and instruction encoding.
//!
//! Every constant and discriminator in this file was recovered by decoding live
//! mainnet transactions, not from an IDL. See `docs/RESEARCH.md`.

use anyhow::{bail, Result};
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use std::sync::LazyLock;

macro_rules! key {
    ($name:ident, $s:expr) => {
        pub static $name: LazyLock<Pubkey> = LazyLock::new(|| Pubkey::from_str($s).unwrap());
    };
}

key!(PROGRAM, "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");
key!(GLOBAL, "4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf");
key!(EVENT_AUTHORITY, "Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1");
key!(FEE_PROGRAM, "pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");
key!(FEE_CONFIG, "8Wf5TiAheLUqBrKXeYg2JtAFFMWtKdG2BSFgqUcPVwTt");
key!(GLOBAL_VOLUME_ACC, "Hq2wp8uJ9jCPsYgNHex8RtqdvMPfVGoYwjvF1ATiwn2Y");
key!(SYSTEM_PROGRAM, "11111111111111111111111111111111");
key!(TOKEN_PROGRAM, "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
key!(TOKEN_2022_PROGRAM, "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
key!(ATA_PROGRAM, "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
key!(WSOL_MINT, "So11111111111111111111111111111111111111112");

/// Anchor discriminators observed on mainnet.
pub mod disc {
    /// `BuyExactSolIn(sol_in: u64, min_tokens_out: u64)` - 18 accounts.
    pub const BUY_EXACT_SOL_IN: [u8; 8] = [0x38, 0xfc, 0x74, 0x08, 0x9e, 0xdf, 0xcd, 0x5f];
    /// `SellV2(token_amount: u64, min_sol_out: u64)` - 26 accounts.
    pub const SELL_V2: [u8; 8] = [0x5d, 0xf6, 0x82, 0x3c, 0xe7, 0xe9, 0x40, 0xb2];
    /// `Migrate()` - 27 accounts, no args.
    pub const MIGRATE: [u8; 8] = [0x9b, 0xea, 0xe7, 0x92, 0xec, 0x9e, 0xa2, 0x1e];
    /// `MigrateV2()` - 29 accounts, no args.
    pub const MIGRATE_V2: [u8; 8] = [0xbb, 0xcb, 0x12, 0x1f, 0xce, 0xed, 0xfe, 0x29];
}

// ---------------------------------------------------------------------------
// Curve constants
// ---------------------------------------------------------------------------

/// Virtual SOL reserves a fresh curve starts with (30 SOL).
pub const INIT_VIRTUAL_SOL: u128 = 30_000_000_000;
/// Virtual token reserves a fresh curve starts with (1.073e9 tokens, 6dp).
pub const INIT_VIRTUAL_TOKENS: u128 = 1_073_000_000_000_000;
/// Real tokens available to buyers before graduation (793.1e6 tokens, 6dp).
pub const INIT_REAL_TOKENS: u128 = 793_100_000_000_000;

/// Total fee charged on top of the amount that reaches the curve, in basis points.
///
/// Measured: a 3.440000000 SOL order delivered 3.397531 SOL into the curve, with
/// 0.016138 x2 protocol and 0.010193 creator alongside. 3.397531 * 1.0125 = 3.44.
pub const FEE_BPS: u128 = 125;

/// `real_sol_reserves` at which pump.fun flips `complete = true`.
///
/// Derived, not guessed: the curve is a constant product, so when the last real
/// token is sold `virtual_sol = k / (virtual_tokens - real_tokens)`.
pub fn graduation_lamports() -> u64 {
    let k = INIT_VIRTUAL_SOL * INIT_VIRTUAL_TOKENS;
    let vt_final = INIT_VIRTUAL_TOKENS - INIT_REAL_TOKENS;
    let vs_final = k / vt_final;
    (vs_final - INIT_VIRTUAL_SOL) as u64
}

// ---------------------------------------------------------------------------
// Account state
// ---------------------------------------------------------------------------

/// On-chain sizes of the `BondingCurve` account.
///
/// Measured, not assumed: of 119 curves sampled from mainnet, 88 were 151 bytes
/// and 31 were 115 bytes — pump.fun has two live account versions. Guessing a
/// single size here silently matches nothing, which is exactly what a `dataSize`
/// subscription filter does when it is wrong: it fails quietly and forever.
pub const CURVE_ACCOUNT_SIZES: [usize; 2] = [115, 151];

/// Decoded `BondingCurve` account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BondingCurve {
    pub virtual_token_reserves: u64,
    pub virtual_sol_reserves: u64,
    pub real_token_reserves: u64,
    pub real_sol_reserves: u64,
    pub token_total_supply: u64,
    pub complete: bool,
    pub creator: Option<Pubkey>,
}

impl BondingCurve {
    pub const MIN_LEN: usize = 49;

    pub fn decode(data: &[u8]) -> Result<Self> {
        if data.len() < Self::MIN_LEN {
            bail!("bonding curve account too short: {} bytes", data.len());
        }
        let u = |o: usize| u64::from_le_bytes(data[o..o + 8].try_into().unwrap());
        let creator = if data.len() >= 81 {
            Some(Pubkey::new_from_array(data[49..81].try_into().unwrap()))
        } else {
            None
        };
        Ok(Self {
            virtual_token_reserves: u(8),
            virtual_sol_reserves: u(16),
            real_token_reserves: u(24),
            real_sol_reserves: u(32),
            token_total_supply: u(40),
            complete: data[48] != 0,
            creator,
        })
    }

    /// True for a curve that exists but has never been bought.
    pub fn is_pristine(&self) -> bool {
        self.real_sol_reserves == 0 && !self.complete
    }

    /// Lamports of *net* curve inflow still required to graduate. `None` once done.
    pub fn gap_to_graduation(&self) -> Option<u64> {
        if self.complete {
            return None;
        }
        graduation_lamports().checked_sub(self.real_sol_reserves)
    }

    /// Fraction of the way to graduation, 0.0 - 1.0.
    pub fn progress(&self) -> f64 {
        self.real_sol_reserves as f64 / graduation_lamports() as f64
    }

    /// Tokens received for `sol_in` lamports reaching the curve (fees already removed).
    pub fn tokens_out(&self, sol_in: u64) -> u64 {
        if sol_in == 0 {
            return 0;
        }
        let vs = self.virtual_sol_reserves as u128;
        let vt = self.virtual_token_reserves as u128;
        let k = vs * vt;
        let new_vs = vs + sol_in as u128;
        let new_vt = k / new_vs;
        let out = vt.saturating_sub(new_vt);
        out.min(self.real_token_reserves as u128) as u64
    }

    /// Lamports received for selling `tokens` back into the curve, before fees.
    pub fn sol_out(&self, tokens: u64) -> u64 {
        if tokens == 0 {
            return 0;
        }
        let vs = self.virtual_sol_reserves as u128;
        let vt = self.virtual_token_reserves as u128;
        let k = vs * vt;
        let new_vt = vt + tokens as u128;
        let new_vs = k / new_vt;
        vs.saturating_sub(new_vs) as u64
    }

    /// Apply a buy locally so the tracker can stay ahead of the next account update.
    pub fn apply_buy(&mut self, sol_in: u64) -> u64 {
        let out = self.tokens_out(sol_in);
        self.virtual_sol_reserves = self.virtual_sol_reserves.saturating_add(sol_in);
        self.virtual_token_reserves = self.virtual_token_reserves.saturating_sub(out);
        self.real_token_reserves = self.real_token_reserves.saturating_sub(out);
        self.real_sol_reserves = self.real_sol_reserves.saturating_add(sol_in);
        if self.real_sol_reserves >= graduation_lamports() {
            self.complete = true;
        }
        out
    }
}

/// Gross order size (what you pay) for a target net amount reaching the curve.
pub fn gross_for_net(net: u64) -> u64 {
    let n = net as u128;
    ((n * (10_000 + FEE_BPS)).div_ceil(10_000)) as u64
}

/// Net amount reaching the curve for a gross order.
pub fn net_for_gross(gross: u64) -> u64 {
    ((gross as u128 * 10_000) / (10_000 + FEE_BPS)) as u64
}

// ---------------------------------------------------------------------------
// PDAs
// ---------------------------------------------------------------------------

pub fn bonding_curve(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"bonding-curve", mint.as_ref()], &PROGRAM).0
}

pub fn creator_vault(creator: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"creator-vault", creator.as_ref()], &PROGRAM).0
}

pub fn user_volume_accumulator(user: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"user_volume_accumulator", user.as_ref()], &PROGRAM).0
}

/// Associated token account, derived for whichever token program the mint uses.
pub fn ata(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ATA_PROGRAM,
    )
    .0
}

// ---------------------------------------------------------------------------
// Instructions
// ---------------------------------------------------------------------------

/// Accounts required by `BuyExactSolIn`, in the exact on-chain order.
#[derive(Debug, Clone)]
pub struct BuyAccounts {
    pub fee_recipient: Pubkey,
    pub mint: Pubkey,
    pub bonding_curve: Pubkey,
    pub associated_bonding_curve: Pubkey,
    pub associated_user: Pubkey,
    pub user: Pubkey,
    pub token_program: Pubkey,
    pub creator_vault: Pubkey,
    pub user_volume_accumulator: Pubkey,
    pub fee_config: Pubkey,
    pub trailing: Vec<Pubkey>,
}

/// `BuyExactSolIn(sol_in, min_tokens_out)`.
///
/// `sol_in` is the **gross** amount debited, fees included.
pub fn buy_exact_sol_in(a: &BuyAccounts, sol_in: u64, min_tokens_out: u64) -> Instruction {
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&disc::BUY_EXACT_SOL_IN);
    data.extend_from_slice(&sol_in.to_le_bytes());
    data.extend_from_slice(&min_tokens_out.to_le_bytes());

    let mut accounts = vec![
        AccountMeta::new_readonly(*GLOBAL, false),
        AccountMeta::new(a.fee_recipient, false),
        AccountMeta::new_readonly(a.mint, false),
        AccountMeta::new(a.bonding_curve, false),
        AccountMeta::new(a.associated_bonding_curve, false),
        AccountMeta::new(a.associated_user, false),
        AccountMeta::new(a.user, true),
        AccountMeta::new_readonly(*SYSTEM_PROGRAM, false),
        AccountMeta::new_readonly(a.token_program, false),
        AccountMeta::new(a.creator_vault, false),
        AccountMeta::new_readonly(*EVENT_AUTHORITY, false),
        AccountMeta::new_readonly(*PROGRAM, false),
        AccountMeta::new(*GLOBAL_VOLUME_ACC, false),
        AccountMeta::new(a.user_volume_accumulator, false),
        AccountMeta::new_readonly(a.fee_config, false),
        AccountMeta::new_readonly(*FEE_PROGRAM, false),
    ];
    accounts.extend(a.trailing.iter().map(|k| AccountMeta::new_readonly(*k, false)));

    Instruction {
        program_id: *PROGRAM,
        accounts,
        data,
    }
}

/// `MigrateV2()` - takes no arguments; the account list carries everything.
pub fn migrate_v2(accounts: Vec<AccountMeta>) -> Instruction {
    Instruction {
        program_id: *PROGRAM,
        accounts,
        data: disc::MIGRATE_V2.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> BondingCurve {
        BondingCurve {
            virtual_token_reserves: INIT_VIRTUAL_TOKENS as u64,
            virtual_sol_reserves: INIT_VIRTUAL_SOL as u64,
            real_token_reserves: INIT_REAL_TOKENS as u64,
            real_sol_reserves: 0,
            token_total_supply: 1_000_000_000_000_000,
            complete: false,
            creator: None,
        }
    }

    #[test]
    fn graduation_matches_observed_mainnet_value() {
        // Seven completion buys observed on mainnet all landed the curve on
        // 85.0071-85.0073 SOL of account balance, which is this plus rent.
        let g = graduation_lamports();
        assert_eq!(g, 85_005_359_056);
    }

    #[test]
    fn fresh_curve_gap_is_full_graduation() {
        assert_eq!(fresh().gap_to_graduation(), Some(graduation_lamports()));
    }

    #[test]
    fn fee_round_trip_matches_observed_order() {
        // Observed: gross 3.440000000 SOL -> 3.397531 SOL net into the curve.
        let net = net_for_gross(3_440_000_000);
        assert!((net as i64 - 3_397_530_864).abs() < 1_000, "net was {net}");
        assert!(gross_for_net(net) >= 3_439_000_000);
    }

    #[test]
    fn buying_the_whole_gap_completes_the_curve() {
        let mut c = fresh();
        let gap = c.gap_to_graduation().unwrap();
        c.apply_buy(gap);
        assert!(c.complete);
        assert_eq!(c.gap_to_graduation(), None);
    }

    #[test]
    fn tokens_out_is_capped_by_real_reserves() {
        let c = fresh();
        let huge = c.tokens_out(1_000_000_000_000);
        assert!(huge <= c.real_token_reserves);
    }

    #[test]
    fn pristine_detection() {
        assert!(fresh().is_pristine());
        let mut c = fresh();
        c.apply_buy(1_000_000_000);
        assert!(!c.is_pristine());
    }
}
