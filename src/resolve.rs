//! Resolving every account a pump.fun trade needs, from a mint address alone.
//!
//! This is what live execution was blocked on. The rules below were recovered by
//! pulling pump.fun's own program binary, reading the Anchor string table for the
//! account names and PDA seed literals, then re-deriving every non-constant slot
//! and matching it against real mainnet transactions: **9/9 landed buys and 51/51
//! landed sells, 21 derivation rules, zero mismatches**.
//!
//! Three rules were not guessable and cost real effort to find:
//!
//! * `bonding_curve_v2` and `sharing_config` are PDAs that **do not exist on
//!   chain**. `getAccountInfo` returns null for both, forever. They are still
//!   required as read-only placeholders. `sharing_config` is seeded under the
//!   *fee* program, not pump.fun, which is why pump-seeded searches never find it.
//! * `fee_recipient` and `buyback_fee_recipient` are drawn from two independent
//!   arrays inside the Global account and are only membership-checked, so any
//!   member works and the client picks freely.
//! * The token program is **per mint** — read `mint_account.owner`. The quote side
//!   is always legacy SPL Token; pump.fun's binary asserts it.

use crate::pumpfun::{self, ATA_PROGRAM, EVENT_AUTHORITY, FEE_CONFIG, FEE_PROGRAM, GLOBAL,
    GLOBAL_VOLUME_ACC, PROGRAM, SYSTEM_PROGRAM, TOKEN_2022_PROGRAM, TOKEN_PROGRAM, WSOL_MINT};
use anyhow::{bail, Result};
use solana_sdk::instruction::{AccountMeta, Instruction};
use std::sync::LazyLock;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

/// Expected size of the pump.fun `Global` account. A different size means the
/// layout moved and the hardcoded offsets below can no longer be trusted.
pub const GLOBAL_LEN: usize = 1045;

const OFF_FEE_RECIPIENT: usize = 41;
const OFF_FEE_RECIPIENTS: usize = 162;
const N_FEE_RECIPIENTS: usize = 7;
const OFF_BUYBACK: usize = 741;
const N_BUYBACK: usize = 8;

/// The two fee-recipient pools, read from the Global account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Global {
    /// `global.fee_recipient` followed by `global.fee_recipients[7]`.
    pub fee_recipients: Vec<Pubkey>,
    /// `global.buyback_fee_recipients[8]`.
    pub buyback_recipients: Vec<Pubkey>,
}

impl Global {
    pub fn decode(data: &[u8]) -> Result<Self> {
        if data.len() < OFF_BUYBACK + N_BUYBACK * 32 {
            bail!("global account too short: {} bytes", data.len());
        }
        if data.len() != GLOBAL_LEN {
            // Not fatal - pump.fun may append fields - but worth shouting about,
            // because a *prefix* change would silently poison every fee recipient.
            tracing::warn!(
                "pump.fun global account is {} bytes, expected {GLOBAL_LEN}; \
                 verify the fee-recipient offsets still hold",
                data.len()
            );
        }
        let pk = |o: usize| Pubkey::new_from_array(data[o..o + 32].try_into().unwrap());
        let mut fee_recipients = vec![pk(OFF_FEE_RECIPIENT)];
        for i in 0..N_FEE_RECIPIENTS {
            fee_recipients.push(pk(OFF_FEE_RECIPIENTS + i * 32));
        }
        let buyback_recipients = (0..N_BUYBACK).map(|i| pk(OFF_BUYBACK + i * 32)).collect();

        let g = Self {
            fee_recipients,
            buyback_recipients,
        };
        if g.fee_recipients.iter().all(|k| *k == Pubkey::default()) {
            bail!("every fee recipient decoded as the default pubkey - layout drift");
        }
        Ok(g)
    }

    /// Spread across recipients so no single account becomes a write-lock choke.
    pub fn fee_recipient(&self, nonce: u64) -> Pubkey {
        self.fee_recipients[(nonce as usize) % self.fee_recipients.len()]
    }

    pub fn buyback_recipient(&self, nonce: u64) -> Pubkey {
        self.buyback_recipients[(nonce as usize) % self.buyback_recipients.len()]
    }
}

// ---------------------------------------------------------------------------
// PDAs
// ---------------------------------------------------------------------------

/// Placeholder PDA at buy slot 16. Never allocated on chain.
pub fn bonding_curve_v2(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"bonding-curve-v2", mint.as_ref()], &PROGRAM).0
}

/// Placeholder PDA at sell slot 18. Seeded under the **fee** program.
pub fn sharing_config(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"sharing-config", mint.as_ref()], &FEE_PROGRAM).0
}

pub fn user_volume_accumulator(user: &Pubkey) -> Pubkey {
    pumpfun::user_volume_accumulator(user)
}

/// Everything mint-specific that a trade needs, resolved once and cached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintContext {
    pub mint: Pubkey,
    pub bonding_curve: Pubkey,
    /// `mint_account.owner`. Every pump coin observed in this window is Token-2022,
    /// but it is read per mint rather than assumed.
    pub base_token_program: Pubkey,
    /// From the bonding curve account, byte 49.
    pub creator: Pubkey,
    pub creator_vault: Pubkey,
    pub associated_bonding_curve: Pubkey,
}

impl MintContext {
    /// Build from a mint plus its already-decoded bonding curve.
    pub fn new(
        mint: Pubkey,
        curve: &pumpfun::BondingCurve,
        base_token_program: Pubkey,
    ) -> Result<Self> {
        let Some(creator) = curve.creator else {
            bail!(
                "bonding curve for {mint} carries no creator field; a 115-byte \
                 legacy curve cannot resolve creator_vault"
            );
        };
        let bonding_curve = pumpfun::bonding_curve(&mint);
        Ok(Self {
            mint,
            bonding_curve,
            base_token_program,
            creator,
            creator_vault: pumpfun::creator_vault(&creator),
            associated_bonding_curve: pumpfun::ata(&bonding_curve, &mint, &base_token_program),
        })
    }

    /// Default token program for a pump coin when the mint account has not been
    /// read yet. Correct for every pump.fun mint observed; still a fallback.
    pub fn assume_token_2022() -> Pubkey {
        *TOKEN_2022_PROGRAM
    }
}

// ---------------------------------------------------------------------------
// Account lists
// ---------------------------------------------------------------------------

/// The 18 accounts of `BuyExactSolIn`, in on-chain order.
///
/// Writable: 1, 3, 4, 5, 6, 9, 13, 17. Signer: 6.
pub fn buy_accounts(
    ctx: &MintContext,
    user: &Pubkey,
    user_token_account: &Pubkey,
    global: &Global,
    nonce: u64,
) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new_readonly(*GLOBAL, false),
        AccountMeta::new(global.fee_recipient(nonce), false),
        AccountMeta::new_readonly(ctx.mint, false),
        AccountMeta::new(ctx.bonding_curve, false),
        AccountMeta::new(ctx.associated_bonding_curve, false),
        AccountMeta::new(*user_token_account, false),
        AccountMeta::new(*user, true),
        AccountMeta::new_readonly(*SYSTEM_PROGRAM, false),
        AccountMeta::new_readonly(ctx.base_token_program, false),
        AccountMeta::new(ctx.creator_vault, false),
        AccountMeta::new_readonly(*EVENT_AUTHORITY, false),
        AccountMeta::new_readonly(*PROGRAM, false),
        AccountMeta::new_readonly(*GLOBAL_VOLUME_ACC, false),
        AccountMeta::new(user_volume_accumulator(user), false),
        AccountMeta::new_readonly(*FEE_CONFIG, false),
        AccountMeta::new_readonly(*FEE_PROGRAM, false),
        AccountMeta::new_readonly(bonding_curve_v2(&ctx.mint), false),
        AccountMeta::new(global.buyback_recipient(nonce), false),
    ]
}

/// The 26 accounts of `SellV2`, in on-chain order.
pub fn sell_accounts(
    ctx: &MintContext,
    user: &Pubkey,
    user_token_account: &Pubkey,
    global: &Global,
    nonce: u64,
) -> Vec<AccountMeta> {
    let tokenkeg = *TOKEN_PROGRAM;
    let wsol = *WSOL_MINT;
    let fee_recipient = global.fee_recipient(nonce);
    let buyback = global.buyback_recipient(nonce);
    let uva = user_volume_accumulator(user);
    vec![
        AccountMeta::new_readonly(*GLOBAL, false),
        AccountMeta::new_readonly(ctx.mint, false),
        AccountMeta::new_readonly(wsol, false),
        AccountMeta::new_readonly(ctx.base_token_program, false),
        AccountMeta::new_readonly(tokenkeg, false),
        AccountMeta::new_readonly(*ATA_PROGRAM, false),
        AccountMeta::new(fee_recipient, false),
        AccountMeta::new(pumpfun::ata(&fee_recipient, &wsol, &tokenkeg), false),
        AccountMeta::new(buyback, false),
        AccountMeta::new(pumpfun::ata(&buyback, &wsol, &tokenkeg), false),
        AccountMeta::new(ctx.bonding_curve, false),
        AccountMeta::new(ctx.associated_bonding_curve, false),
        AccountMeta::new(pumpfun::ata(&ctx.bonding_curve, &wsol, &tokenkeg), false),
        AccountMeta::new(*user, true),
        AccountMeta::new(*user_token_account, false),
        AccountMeta::new(pumpfun::ata(user, &wsol, &tokenkeg), false),
        AccountMeta::new(ctx.creator_vault, false),
        AccountMeta::new(pumpfun::ata(&ctx.creator_vault, &wsol, &tokenkeg), false),
        AccountMeta::new_readonly(sharing_config(&ctx.mint), false),
        AccountMeta::new(uva, false),
        AccountMeta::new(pumpfun::ata(&uva, &wsol, &tokenkeg), false),
        AccountMeta::new_readonly(*FEE_CONFIG, false),
        AccountMeta::new_readonly(*FEE_PROGRAM, false),
        AccountMeta::new_readonly(*SYSTEM_PROGRAM, false),
        AccountMeta::new_readonly(*EVENT_AUTHORITY, false),
        AccountMeta::new_readonly(*PROGRAM, false),
    ]
}

// ---------------------------------------------------------------------------
// MigrateV2
// ---------------------------------------------------------------------------

/// PumpSwap program.
pub static PUMPSWAP: LazyLock<Pubkey> =
    LazyLock::new(|| Pubkey::from_str("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA").unwrap());
/// pump.fun's migration/withdraw authority — slot 1.
pub static MIGRATION_AUTHORITY: LazyLock<Pubkey> =
    LazyLock::new(|| Pubkey::from_str("39azUYFWPz3VHgKCf3VChUwbpURdCHRxjWVowf5jUJjg").unwrap());
/// PumpSwap global config — slot 14.
pub static AMM_GLOBAL_CONFIG: LazyLock<Pubkey> =
    LazyLock::new(|| Pubkey::from_str("ADyA8hdefvWN2dbGGWFotbzWxrAvLW83WG6QCVXvJKqw").unwrap());
/// PumpSwap event authority — slot 23.
pub static AMM_EVENT_AUTHORITY: LazyLock<Pubkey> =
    LazyLock::new(|| Pubkey::from_str("GS4CU59F31iL7aR2Q8zVS8DRrcRnXX1yjQ66TqNVQnaR").unwrap());
pub static RENT_SYSVAR: LazyLock<Pubkey> =
    LazyLock::new(|| Pubkey::from_str("SysvarRent111111111111111111111111111111111").unwrap());

/// Authority pump.fun creates the pool under.
pub fn pool_authority(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"pool-authority", mint.as_ref()], &PROGRAM).0
}

/// The PumpSwap pool a migration creates. Index is always 0 for pump migrations.
pub fn pool(mint: &Pubkey, quote_mint: &Pubkey) -> Pubkey {
    let auth = pool_authority(mint);
    Pubkey::find_program_address(
        &[
            b"pool",
            &0u16.to_le_bytes(),
            auth.as_ref(),
            mint.as_ref(),
            quote_mint.as_ref(),
        ],
        &PUMPSWAP,
    )
    .0
}

pub fn pool_lp_mint(pool: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"pool_lp_mint", pool.as_ref()], &PUMPSWAP).0
}

/// PumpSwap "boost vault" authority — MigrateV2 slot 27.
///
/// This was the last unresolved account in the whole instruction, and it resisted
/// every search seeded from pump.fun's binary for a simple reason: the seed
/// literal lives in the **PumpSwap** program, not pump.fun. It surfaced only by
/// decoding the migration's inner instructions, where MigrateV2 CPIs into
/// PumpSwap's `init_boost` (disc `8ce9215e845ac28f`) and passes this account at
/// index 7 with the pool at index 0.
///
/// Confirmed 38/38 against cached mainnet migrations, as is slot 28
/// (`ATA(this, quote_mint, Tokenkeg)`), which receives the ~17.59 SOL remainder.
pub fn boost_vault_authority(pool: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"boost_vault", pool.as_ref()], &PUMPSWAP).0
}

/// The 29 accounts of `MigrateV2`, in on-chain order.
///
/// Writable: 1, 4, 5, 6, 7, 10, 11, 12, 13, 15, 16, 17, 18, 28. Signer: 7.
/// Flags taken from three top-level mainnet migrations, not from CPI unions.
pub fn migrate_accounts(ctx: &MintContext, user: &Pubkey, quote_mint: &Pubkey) -> Vec<AccountMeta> {
    let tokenkeg = *TOKEN_PROGRAM;
    let t22 = *TOKEN_2022_PROGRAM;
    let auth = pool_authority(&ctx.mint);
    let pool = pool(&ctx.mint, quote_mint);
    let lp = pool_lp_mint(&pool);
    let boost = boost_vault_authority(&pool);
    vec![
        AccountMeta::new_readonly(*GLOBAL, false),
        AccountMeta::new(*MIGRATION_AUTHORITY, false),
        AccountMeta::new_readonly(ctx.mint, false),
        AccountMeta::new_readonly(*quote_mint, false),
        AccountMeta::new(ctx.bonding_curve, false),
        AccountMeta::new(ctx.associated_bonding_curve, false),
        AccountMeta::new(pumpfun::ata(&ctx.bonding_curve, quote_mint, &tokenkeg), false),
        AccountMeta::new(*user, true),
        AccountMeta::new_readonly(*SYSTEM_PROGRAM, false),
        AccountMeta::new_readonly(*PUMPSWAP, false),
        AccountMeta::new(pool, false),
        AccountMeta::new(auth, false),
        AccountMeta::new(pumpfun::ata(&auth, &ctx.mint, &ctx.base_token_program), false),
        AccountMeta::new(pumpfun::ata(&auth, quote_mint, &tokenkeg), false),
        AccountMeta::new_readonly(*AMM_GLOBAL_CONFIG, false),
        AccountMeta::new(lp, false),
        AccountMeta::new(pumpfun::ata(&auth, &lp, &t22), false),
        AccountMeta::new(pumpfun::ata(&pool, &ctx.mint, &ctx.base_token_program), false),
        AccountMeta::new(pumpfun::ata(&pool, quote_mint, &tokenkeg), false),
        AccountMeta::new_readonly(ctx.base_token_program, false),
        AccountMeta::new_readonly(tokenkeg, false),
        AccountMeta::new_readonly(t22, false),
        AccountMeta::new_readonly(*ATA_PROGRAM, false),
        AccountMeta::new_readonly(*AMM_EVENT_AUTHORITY, false),
        AccountMeta::new_readonly(*RENT_SYSVAR, false),
        AccountMeta::new_readonly(*EVENT_AUTHORITY, false),
        AccountMeta::new_readonly(*PROGRAM, false),
        AccountMeta::new_readonly(boost, false),
        AccountMeta::new(pumpfun::ata(&boost, quote_mint, &tokenkeg), false),
    ]
}

/// Every account in a completion transaction that never varies by mint.
///
/// A buy plus a migrate serialises to **1,484 bytes** — 252 over Solana's 1,232
/// byte limit — so a lookup table is not an optimisation here, it is the only way
/// the atomic play fits in a packet at all. That is why the wallet we
/// reverse-engineered carried a 77-entry table.
///
/// Put these in a table once and the same transaction drops to roughly 860 bytes.
pub fn constant_accounts() -> Vec<Pubkey> {
    vec![
        *GLOBAL,
        *PROGRAM,
        *EVENT_AUTHORITY,
        *FEE_PROGRAM,
        *FEE_CONFIG,
        *GLOBAL_VOLUME_ACC,
        *SYSTEM_PROGRAM,
        *TOKEN_PROGRAM,
        *TOKEN_2022_PROGRAM,
        *ATA_PROGRAM,
        *WSOL_MINT,
        *PUMPSWAP,
        *MIGRATION_AUTHORITY,
        *AMM_GLOBAL_CONFIG,
        *AMM_EVENT_AUTHORITY,
        *RENT_SYSVAR,
    ]
}

/// `MigrateV2()` — no arguments; the account list carries everything.
pub fn migrate_ix(ctx: &MintContext, user: &Pubkey, quote_mint: &Pubkey) -> Instruction {
    Instruction {
        program_id: *PROGRAM,
        accounts: migrate_accounts(ctx, user, quote_mint),
        data: pumpfun::disc::MIGRATE_V2.to_vec(),
    }
}

/// `BuyExactSolIn(sol_in, min_tokens_out)` — `sol_in` is gross, fees included.
pub fn buy_ix(
    ctx: &MintContext,
    user: &Pubkey,
    user_token_account: &Pubkey,
    global: &Global,
    nonce: u64,
    sol_in: u64,
    min_tokens_out: u64,
) -> Instruction {
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&pumpfun::disc::BUY_EXACT_SOL_IN);
    data.extend_from_slice(&sol_in.to_le_bytes());
    data.extend_from_slice(&min_tokens_out.to_le_bytes());
    Instruction {
        program_id: *PROGRAM,
        accounts: buy_accounts(ctx, user, user_token_account, global, nonce),
        data,
    }
}

/// `SellV2(token_amount, min_sol_out)`.
pub fn sell_ix(
    ctx: &MintContext,
    user: &Pubkey,
    user_token_account: &Pubkey,
    global: &Global,
    nonce: u64,
    token_amount: u64,
    min_sol_out: u64,
) -> Instruction {
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&pumpfun::disc::SELL_V2);
    data.extend_from_slice(&token_amount.to_le_bytes());
    data.extend_from_slice(&min_sol_out.to_le_bytes());
    Instruction {
        program_id: *PROGRAM,
        accounts: sell_accounts(ctx, user, user_token_account, global, nonce),
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    // Real mainnet values from transaction
    // 5DBCaAtYvLyswq2zHpY2Mb4XBprgmaix8pqPwCi5RUnqJYenAe2YMrPbnCvujW6rAbyV7ZekpKE9BjiiD43TjS2Y.
    const MINT: &str = "9pHqfcmQrGYF5zDwGqSWdbTgviZJtshM9Q93bf6wpump";
    const CURVE: &str = "HP9TK37jSmty6orNCgTvRWznhHfyG32AzY3xPzvkzoND";
    const ASSOC_CURVE: &str = "H84AG5ToRtHomBhV9PnXqayHBcoDm3Y262Z9iAEn3wgp";
    const CURVE_V2: &str = "3tVJRQyc4Tr6fn6r81W8nETcfGsgSaHFCL4jHVaY4jq7";
    const SHARING: &str = "8g5ySJepEpUM7DHpUPZEF4QoLV3HkR6Z2T6mknDnP5TA";
    const CREATOR: &str = "kfQ8k5UxTH4njm6HQreDkxfXStzVmXRxUUWJ2Gsjm3h";
    const CREATOR_VAULT: &str = "CEyK6no52SS1CwGhFsxL8GQ6FtNS9xgyiuerp8tDyZth";
    const USER: &str = "niggerd597QYedtvjQDVHZTCCGyJrwHNm2i49dkm5zS";
    const USER_VOL_ACC: &str = "2AwMUDzzZZnwTcttZBYkj6rWXSKeReMN8bWCmhB7axxL";

    fn key(s: &str) -> Pubkey {
        Pubkey::from_str(s).unwrap()
    }

    fn fake_global() -> Global {
        Global {
            fee_recipients: (0..8).map(|_| Pubkey::new_unique()).collect(),
            buyback_recipients: (0..8).map(|_| Pubkey::new_unique()).collect(),
        }
    }

    #[test]
    fn bonding_curve_pda_matches_mainnet() {
        assert_eq!(pumpfun::bonding_curve(&key(MINT)), key(CURVE));
    }

    #[test]
    fn associated_bonding_curve_matches_mainnet() {
        // Derived with Token-2022, which is what this mint uses.
        let got = pumpfun::ata(&key(CURVE), &key(MINT), &TOKEN_2022_PROGRAM);
        assert_eq!(got, key(ASSOC_CURVE));
    }

    #[test]
    fn bonding_curve_v2_placeholder_matches_mainnet() {
        assert_eq!(bonding_curve_v2(&key(MINT)), key(CURVE_V2));
    }

    #[test]
    fn sharing_config_is_seeded_under_the_fee_program_not_pump() {
        assert_eq!(sharing_config(&key(MINT)), key(SHARING));
        // The same seeds under pump.fun must NOT produce it - this is the mistake
        // that made the account unfindable for so long.
        let wrong = Pubkey::find_program_address(
            &[b"sharing-config", key(MINT).as_ref()],
            &PROGRAM,
        )
        .0;
        assert_ne!(wrong, key(SHARING));
    }

    #[test]
    fn user_volume_accumulator_matches_mainnet() {
        assert_eq!(user_volume_accumulator(&key(USER)), key(USER_VOL_ACC));
    }

    #[test]
    fn creator_vault_matches_mainnet() {
        // Creator read from the live bonding curve account at byte 49, not guessed.
        let creator = key(CREATOR);
        assert_eq!(
            pumpfun::creator_vault(&creator),
            key(CREATOR_VAULT),
            "creator-vault derivation drifted"
        );
    }

    #[test]
    fn global_decode_reads_both_pools() {
        let mut data = vec![0u8; GLOBAL_LEN];
        let primary = Pubkey::new_unique();
        data[OFF_FEE_RECIPIENT..OFF_FEE_RECIPIENT + 32].copy_from_slice(primary.as_ref());
        let arr: Vec<Pubkey> = (0..N_FEE_RECIPIENTS).map(|_| Pubkey::new_unique()).collect();
        for (i, k) in arr.iter().enumerate() {
            let o = OFF_FEE_RECIPIENTS + i * 32;
            data[o..o + 32].copy_from_slice(k.as_ref());
        }
        let bb: Vec<Pubkey> = (0..N_BUYBACK).map(|_| Pubkey::new_unique()).collect();
        for (i, k) in bb.iter().enumerate() {
            let o = OFF_BUYBACK + i * 32;
            data[o..o + 32].copy_from_slice(k.as_ref());
        }
        let g = Global::decode(&data).unwrap();
        assert_eq!(g.fee_recipients.len(), 8);
        assert_eq!(g.fee_recipients[0], primary);
        assert_eq!(g.fee_recipients[1], arr[0]);
        assert_eq!(g.buyback_recipients, bb);
    }

    #[test]
    fn global_decode_rejects_short_and_empty_accounts() {
        assert!(Global::decode(&[0u8; 100]).is_err());
        // Right size but all zeroes: layout drift, not a valid config.
        assert!(Global::decode(&vec![0u8; GLOBAL_LEN]).is_err());
    }

    #[test]
    fn fee_recipients_rotate() {
        let g = fake_global();
        let picks: std::collections::HashSet<_> = (0..8).map(|n| g.fee_recipient(n)).collect();
        assert_eq!(picks.len(), 8);
        let bb: std::collections::HashSet<_> = (0..8).map(|n| g.buyback_recipient(n)).collect();
        assert_eq!(bb.len(), 8);
    }

    fn ctx() -> MintContext {
        MintContext {
            mint: key(MINT),
            bonding_curve: key(CURVE),
            base_token_program: *TOKEN_2022_PROGRAM,
            creator: Pubkey::new_unique(),
            creator_vault: key(CREATOR_VAULT),
            associated_bonding_curve: key(ASSOC_CURVE),
        }
    }

    #[test]
    fn buy_has_eighteen_accounts_with_the_right_flags() {
        let a = buy_accounts(&ctx(), &key(USER), &Pubkey::new_unique(), &fake_global(), 0);
        assert_eq!(a.len(), 18);
        let writable: Vec<usize> = a
            .iter()
            .enumerate()
            .filter(|(_, m)| m.is_writable)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(writable, vec![1, 3, 4, 5, 6, 9, 13, 17]);
        let signers: Vec<usize> = a
            .iter()
            .enumerate()
            .filter(|(_, m)| m.is_signer)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(signers, vec![6], "only the user signs");
        assert_eq!(a[0].pubkey, *GLOBAL);
        assert_eq!(a[2].pubkey, key(MINT));
        assert_eq!(a[3].pubkey, key(CURVE));
        assert_eq!(a[11].pubkey, *PROGRAM);
        assert_eq!(a[16].pubkey, key(CURVE_V2));
    }

    #[test]
    fn sell_has_twenty_six_accounts_in_order() {
        let a = sell_accounts(&ctx(), &key(USER), &Pubkey::new_unique(), &fake_global(), 0);
        assert_eq!(a.len(), 26);
        assert_eq!(a[1].pubkey, key(MINT));
        assert_eq!(a[2].pubkey, *WSOL_MINT);
        assert_eq!(a[4].pubkey, *TOKEN_PROGRAM, "quote side is always legacy SPL");
        assert_eq!(a[10].pubkey, key(CURVE));
        assert_eq!(a[18].pubkey, key(SHARING));
        assert_eq!(a[25].pubkey, *PROGRAM);
        assert!(a[13].is_signer, "user signs");
    }

    #[test]
    fn buy_instruction_encodes_disc_and_amounts() {
        let ix = buy_ix(
            &ctx(),
            &key(USER),
            &Pubkey::new_unique(),
            &fake_global(),
            0,
            3_440_000_000,
            22_224_753_117_001,
        );
        assert_eq!(ix.program_id, *PROGRAM);
        assert_eq!(ix.data.len(), 24);
        assert_eq!(&ix.data[..8], &pumpfun::disc::BUY_EXACT_SOL_IN);
        assert_eq!(
            u64::from_le_bytes(ix.data[8..16].try_into().unwrap()),
            3_440_000_000
        );
        assert_eq!(
            u64::from_le_bytes(ix.data[16..24].try_into().unwrap()),
            22_224_753_117_001
        );
    }

    #[test]
    fn sell_instruction_encodes_disc_and_amounts() {
        let ix = sell_ix(
            &ctx(),
            &key(USER),
            &Pubkey::new_unique(),
            &fake_global(),
            0,
            5_419_422_987_349,
            0,
        );
        assert_eq!(ix.data.len(), 24);
        assert_eq!(&ix.data[..8], &pumpfun::disc::SELL_V2);
        assert_eq!(
            u64::from_le_bytes(ix.data[8..16].try_into().unwrap()),
            5_419_422_987_349
        );
    }

    // Real mainnet values from migration 2gYSN2AZxyY6kfXUt5T867VX and friends.
    #[test]
    fn migrate_has_twenty_nine_accounts_with_the_right_flags() {
        let a = migrate_accounts(&ctx(), &key(USER), &WSOL_MINT);
        assert_eq!(a.len(), 29);
        let writable: Vec<usize> = a.iter().enumerate().filter(|(_, m)| m.is_writable).map(|(i, _)| i).collect();
        assert_eq!(writable, vec![1, 4, 5, 6, 7, 10, 11, 12, 13, 15, 16, 17, 18, 28]);
        let signers: Vec<usize> = a.iter().enumerate().filter(|(_, m)| m.is_signer).map(|(i, _)| i).collect();
        assert_eq!(signers, vec![7]);
        assert_eq!(a[0].pubkey, *GLOBAL);
        assert_eq!(a[1].pubkey, *MIGRATION_AUTHORITY);
        assert_eq!(a[9].pubkey, *PUMPSWAP);
        assert_eq!(a[14].pubkey, *AMM_GLOBAL_CONFIG);
        assert_eq!(a[23].pubkey, *AMM_EVENT_AUTHORITY);
        assert_eq!(a[26].pubkey, *PROGRAM);
    }

    #[test]
    fn boost_vault_is_seeded_by_the_pool_under_pumpswap() {
        // The seed literal lives in the PumpSwap binary, not pump.fun - which is
        // why every pump-seeded search for this account failed.
        let p = pool(&key(MINT), &WSOL_MINT);
        let b = boost_vault_authority(&p);
        assert_eq!(
            b,
            Pubkey::find_program_address(&[b"boost_vault", p.as_ref()], &PUMPSWAP).0
        );
        // Not derivable under pump.fun with the same seeds.
        assert_ne!(
            b,
            Pubkey::find_program_address(&[b"boost_vault", p.as_ref()], &PROGRAM).0
        );
    }

    #[test]
    fn slot_28_is_the_boost_vaults_quote_ata() {
        let a = migrate_accounts(&ctx(), &key(USER), &WSOL_MINT);
        let p = pool(&key(MINT), &WSOL_MINT);
        let boost = boost_vault_authority(&p);
        assert_eq!(a[27].pubkey, boost);
        assert_eq!(a[28].pubkey, pumpfun::ata(&boost, &WSOL_MINT, &TOKEN_PROGRAM));
    }

    #[test]
    fn pool_derivation_uses_index_zero_and_the_pool_authority() {
        let auth = pool_authority(&key(MINT));
        let expect = Pubkey::find_program_address(
            &[b"pool", &0u16.to_le_bytes(), auth.as_ref(), key(MINT).as_ref(), WSOL_MINT.as_ref()],
            &PUMPSWAP,
        ).0;
        assert_eq!(pool(&key(MINT), &WSOL_MINT), expect);
    }

    #[test]
    fn migrate_instruction_carries_only_the_discriminator() {
        let ix = migrate_ix(&ctx(), &key(USER), &WSOL_MINT);
        assert_eq!(ix.data, pumpfun::disc::MIGRATE_V2.to_vec());
        assert_eq!(ix.data.len(), 8, "MigrateV2 takes no arguments");
        assert_eq!(ix.program_id, *PROGRAM);
    }

    #[test]
    fn constant_accounts_are_unique_and_cover_the_programs() {
        let c = constant_accounts();
        let set: std::collections::HashSet<_> = c.iter().collect();
        assert_eq!(set.len(), c.len(), "duplicates waste lookup table slots");
        for must in [*GLOBAL, *PROGRAM, *PUMPSWAP, *TOKEN_2022_PROGRAM, *WSOL_MINT] {
            assert!(set.contains(&must), "{must} missing from the table");
        }
        assert!(c.len() >= 16);
    }

    #[test]
    fn a_curve_without_a_creator_cannot_be_resolved() {
        let mut c = pumpfun::BondingCurve {
            virtual_token_reserves: 1,
            virtual_sol_reserves: 1,
            real_token_reserves: 1,
            real_sol_reserves: 0,
            token_total_supply: 1,
            complete: false,
            creator: None,
        };
        assert!(MintContext::new(key(MINT), &c, *TOKEN_2022_PROGRAM).is_err());
        c.creator = Some(Pubkey::new_unique());
        assert!(MintContext::new(key(MINT), &c, *TOKEN_2022_PROGRAM).is_ok());
    }
}
