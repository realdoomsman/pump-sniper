//! PumpSwap AMM: the venue a graduated curve actually trades on.
//!
//! This exists because of a bug that would have stranded every completion. A
//! completion buy **graduates the bonding curve**, and pump.fun then refuses every
//! sell against it — its own binary carries the string *"The bonding curve has
//! completed and liquidity migrated"*. So a completion position can only ever be
//! closed here.
//!
//! It is also where the profit is. The pool opens **20.71% below** the price the
//! last curve buyer paid (see [`crate::backtest::ordinary_pool_open_discount`]), so buying
//! the new pool at open is the edge; the completing buy is only the premium paid
//! to earn the right to do it.
//!
//! ## Provenance
//!
//! `Sell` (disc `33e685a4017f83ad`, 24 accounts) was resolved by re-deriving every
//! slot and matching it against **73/73** landed mainnet instances. Two findings
//! that would not have been guessed:
//!
//! * `coin_creator_vault_authority` is seeded `"creator_vault"` with an
//!   **underscore**, under PumpSwap — pump.fun's own equivalent uses
//!   `"creator-vault"` with a hyphen, under pump.fun. Two different accounts.
//! * `pool_v2` is seeded by the **mint**, not the pool, and does not exist on
//!   chain. Like pump.fun's placeholders it must still be passed.
//!
//! The `coin_creator` those accounts derive from sits at pool bytes `211..243`. It
//! usually equals the bonding curve's own `creator`, but **not always**: it is
//! mutable and disagreed on 3 of 40 sampled pools. Read it live from the pool on
//! every trade. Deriving it from the curve is right most of the time, which is
//! exactly what makes it dangerous.

use crate::pumpfun::{self, ATA_PROGRAM, FEE_PROGRAM, SYSTEM_PROGRAM, TOKEN_PROGRAM, WSOL_MINT};
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

key!(PROGRAM, "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");
key!(GLOBAL_CONFIG, "ADyA8hdefvWN2dbGGWFotbzWxrAvLW83WG6QCVXvJKqw");
key!(EVENT_AUTHORITY, "GS4CU59F31iL7aR2Q8zVS8DRrcRnXX1yjQ66TqNVQnaR");
// FEE_CONFIG = PDA(["fee_config", PumpSwap program id], pump fee program).
key!(FEE_CONFIG, "5PHirr8joyTMp9JMm6nW7hNDVyEYdkzDqazxPD7RaTjx");

/// Discriminators observed on mainnet.
pub mod disc {
    /// `Sell(base_amount_in: u64, min_quote_out: u64)` — 24 accounts.
    pub const SELL: [u8; 8] = [0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad];
    /// `Buy(base_out: u64, max_quote_in: u64)` — 26 accounts.
    pub const BUY: [u8; 8] = [0x66, 0x06, 0x3d, 0x12, 0x01, 0xda, 0xeb, 0xea];
    /// `BuyExactQuoteIn(quote_in: u64, min_base_out: u64, u8)` — 26 accounts.
    pub const BUY_EXACT_QUOTE_IN: [u8; 8] = [0xc6, 0x2e, 0x15, 0x52, 0xb4, 0xd9, 0xe8, 0x70];
}

/// Pool account layout, verified against 45 live accounts and 40 `CreatePool`
/// instructions.
pub const POOL_LEN: usize = 301;
const OFF_BASE_MINT: usize = 43;
const OFF_QUOTE_MINT: usize = 75;
const OFF_POOL_BASE_TA: usize = 139;
const OFF_POOL_QUOTE_TA: usize = 171;
/// `coin_creator`. **Mutable** — see [`Pool::coin_creator`].
pub const POOL_COIN_CREATOR_OFFSET: usize = 211;
const OFF_IS_MAYHEM: usize = 243;
const OFF_IS_CASHBACK: usize = 244;
const OFF_BOOST_QUOTE: usize = 245;

/// Decoded PumpSwap `Pool`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pool {
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub pool_base_token_account: Pubkey,
    pub pool_quote_token_account: Pubkey,
    /// The creator whose vault collects the creator fee.
    ///
    /// **Must be read live.** It is mutable: it disagreed with the value passed to
    /// `CreatePool` on 3 of 40 sampled pools. Deriving it from the bonding curve's
    /// `creator` — which is right most of the time — silently produces the wrong
    /// `coin_creator_vault_authority` and the transaction fails on chain.
    pub coin_creator: Pubkey,
    pub is_mayhem_mode: bool,
    /// Cashback pools take a **26-account** Sell, not 24. See [`Pool::sell_supported`].
    pub is_cashback_coin: bool,
    /// Virtual quote liquidity, added to the quote reserve before the constant
    /// product. Ignoring it makes every quote wrong — the CPMM reproduces observed
    /// fills 79/79 with this term and 0/79 without it.
    pub boost_quote: u64,
}

impl Pool {
    pub fn decode(data: &[u8]) -> Result<Self> {
        if data.len() < POOL_LEN {
            bail!("pool account is {} bytes, expected {POOL_LEN}", data.len());
        }
        let pk = |o: usize| Pubkey::new_from_array(data[o..o + 32].try_into().unwrap());
        Ok(Self {
            base_mint: pk(OFF_BASE_MINT),
            quote_mint: pk(OFF_QUOTE_MINT),
            pool_base_token_account: pk(OFF_POOL_BASE_TA),
            pool_quote_token_account: pk(OFF_POOL_QUOTE_TA),
            coin_creator: pk(POOL_COIN_CREATOR_OFFSET),
            is_mayhem_mode: data[OFF_IS_MAYHEM] != 0,
            is_cashback_coin: data[OFF_IS_CASHBACK] != 0,
            boost_quote: u64::from_le_bytes(
                data[OFF_BOOST_QUOTE..OFF_BOOST_QUOTE + 8].try_into().unwrap(),
            ),
        })
    }

    /// Number of accounts this pool's `Sell` takes.
    ///
    /// Not cosmetic: the cashback variant **inserts** two accounts at index 21,
    /// pushing `pool_v2` to 23 and `buyback_fee_recipient` to 24. Sending the
    /// 24-account form to a cashback pool binds the wrong account from index 21
    /// onward. Measured with perfect separation: 73/73 ordinary sells took 24,
    /// 6/6 cashback sells took 26, and no pool ever mixed the two.
    pub fn sell_account_count(&self) -> usize {
        if self.is_cashback_coin {
            26
        } else {
            24
        }
    }

    /// Number of accounts this pool's `Buy` takes - two more than `Sell` in the
    /// ordinary case, because Buy always carries both volume accumulators.
    pub fn buy_account_count(&self) -> usize {
        if self.is_cashback_coin {
            27
        } else {
            26
        }
    }
}

// ---------------------------------------------------------------------------
// PDAs
// ---------------------------------------------------------------------------

/// The authority pump.fun creates migrated pools under.
pub fn pool_authority(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"pool-authority", mint.as_ref()], &pumpfun::PROGRAM).0
}

/// The migrated pool. Index is always 0 for pump.fun migrations.
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
        &PROGRAM,
    )
    .0
}

/// Fee vault authority for the coin creator.
///
/// Note the seed is `creator_vault` with an **underscore**, under PumpSwap.
/// pump.fun's is `creator-vault` with a hyphen, under pump.fun. Mixing them up
/// produces a valid-looking address that the program rejects.
pub fn coin_creator_vault_authority(coin_creator: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"creator_vault", coin_creator.as_ref()], &PROGRAM).0
}

/// Placeholder account. Seeded by the **mint**, not the pool, and never allocated
/// on chain — but still required. Sits at slot 21 on an ordinary pool and slot 23
/// on a cashback pool.
pub fn pool_v2(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"pool-v2", mint.as_ref()], &PROGRAM).0
}

/// PumpSwap's per-user volume accumulator.
///
/// Only appears in the cashback variant of `Sell` — which is the point of it: a
/// volume rebate has to know your volume. Note this is PumpSwap's, seeded under
/// PumpSwap; pump.fun has its own identically-named account under its own program.
pub fn user_volume_accumulator(user: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"user_volume_accumulator", user.as_ref()], &PROGRAM).0
}

/// `coin_creator` from a fetched Pool account.
pub fn coin_creator_from_pool(pool_data: &[u8]) -> Result<Pubkey> {
    Ok(Pool::decode(pool_data)?.coin_creator)
}

// ---------------------------------------------------------------------------
// Fee recipients
// ---------------------------------------------------------------------------

const OFF_PROTOCOL_FEES: usize = 57;
const N_PROTOCOL_FEES: usize = 8;
const OFF_BUYBACK_FEE: usize = 835;

/// Fee recipients read from PumpSwap's GlobalConfig. Membership-checked, so any
/// member is accepted and the client picks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalConfig {
    pub protocol_fee_recipients: Vec<Pubkey>,
    pub buyback_fee_recipient: Pubkey,
}

impl GlobalConfig {
    pub fn decode(data: &[u8]) -> Result<Self> {
        if data.len() < OFF_BUYBACK_FEE + 32 {
            bail!("PumpSwap global config too short: {} bytes", data.len());
        }
        let pk = |o: usize| Pubkey::new_from_array(data[o..o + 32].try_into().unwrap());
        let protocol_fee_recipients =
            (0..N_PROTOCOL_FEES).map(|i| pk(OFF_PROTOCOL_FEES + i * 32)).collect::<Vec<_>>();
        if protocol_fee_recipients.iter().all(|k| *k == Pubkey::default()) {
            bail!("every protocol fee recipient decoded as default - layout drift");
        }
        Ok(Self {
            protocol_fee_recipients,
            buyback_fee_recipient: pk(OFF_BUYBACK_FEE),
        })
    }

    pub fn protocol_fee_recipient(&self, nonce: u64) -> Pubkey {
        self.protocol_fee_recipients[(nonce as usize) % self.protocol_fee_recipients.len()]
    }
}

/// Everything a PumpSwap trade needs, all derivable from the mint plus the
/// coin creator — which is the bonding curve's `creator`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolContext {
    pub mint: Pubkey,
    pub quote_mint: Pubkey,
    pub base_token_program: Pubkey,
    pub pool: Pubkey,
    pub coin_creator: Pubkey,
    /// Selects the 26-account `Sell` form.
    pub is_cashback: bool,
}

impl PoolContext {
    /// Build from a **live** pool account. This is the only correct constructor:
    /// `coin_creator` is mutable and cannot be inferred from the bonding curve.
    pub fn from_pool(mint: Pubkey, base_token_program: Pubkey, pool_data: &[u8]) -> Result<Self> {
        let p = Pool::decode(pool_data)?;
        Ok(Self {
            mint,
            quote_mint: *WSOL_MINT,
            base_token_program,
            pool: pool(&mint, &WSOL_MINT),
            coin_creator: p.coin_creator,
            is_cashback: p.is_cashback_coin,
        })
    }

    /// Test-only constructor. Never use in live paths - see `from_pool`.
    #[cfg(test)]
    pub fn new(mint: Pubkey, coin_creator: Pubkey, base_token_program: Pubkey) -> Self {
        Self {
            mint,
            quote_mint: *WSOL_MINT,
            base_token_program,
            pool: pool(&mint, &WSOL_MINT),
            coin_creator,
            is_cashback: false,
        }
    }
}

/// The 24 accounts of PumpSwap `Sell`, in on-chain order.
pub fn sell_accounts(
    ctx: &PoolContext,
    user: &Pubkey,
    user_base_token_account: &Pubkey,
    cfg: &GlobalConfig,
    nonce: u64,
) -> Vec<AccountMeta> {
    let tkeg = *TOKEN_PROGRAM;
    let wsol = *WSOL_MINT;
    let protocol = cfg.protocol_fee_recipient(nonce);
    let cc_auth = coin_creator_vault_authority(&ctx.coin_creator);
    let mut metas = vec![
        AccountMeta::new(ctx.pool, false),
        AccountMeta::new(*user, true),
        AccountMeta::new_readonly(*GLOBAL_CONFIG, false),
        AccountMeta::new_readonly(ctx.mint, false),
        AccountMeta::new_readonly(wsol, false),
        AccountMeta::new(*user_base_token_account, false),
        AccountMeta::new(pumpfun::ata(user, &wsol, &tkeg), false),
        AccountMeta::new(pumpfun::ata(&ctx.pool, &ctx.mint, &ctx.base_token_program), false),
        AccountMeta::new(pumpfun::ata(&ctx.pool, &wsol, &tkeg), false),
        AccountMeta::new_readonly(protocol, false),
        AccountMeta::new(pumpfun::ata(&protocol, &wsol, &tkeg), false),
        AccountMeta::new_readonly(ctx.base_token_program, false),
        AccountMeta::new_readonly(tkeg, false),
        AccountMeta::new_readonly(*SYSTEM_PROGRAM, false),
        AccountMeta::new_readonly(*ATA_PROGRAM, false),
        AccountMeta::new_readonly(*EVENT_AUTHORITY, false),
        AccountMeta::new_readonly(*PROGRAM, false),
        AccountMeta::new(pumpfun::ata(&cc_auth, &wsol, &tkeg), false),
        AccountMeta::new_readonly(cc_auth, false),
        AccountMeta::new_readonly(*FEE_CONFIG, false),
        AccountMeta::new_readonly(*FEE_PROGRAM, false),
    ];
    // Cashback pools insert the user's volume accumulator and its WSOL ATA before
    // the placeholder, shifting everything after it by two.
    if ctx.is_cashback {
        let uva = user_volume_accumulator(user);
        metas.push(AccountMeta::new(pumpfun::ata(&uva, &wsol, &tkeg), false));
        metas.push(AccountMeta::new(uva, false));
    }
    metas.push(AccountMeta::new_readonly(pool_v2(&ctx.mint), false));
    metas.push(AccountMeta::new_readonly(cfg.buyback_fee_recipient, false));
    metas.push(AccountMeta::new(
        pumpfun::ata(&cfg.buyback_fee_recipient, &wsol, &tkeg),
        false,
    ));
    metas
}

/// `Sell(base_amount_in, min_quote_out)`.
pub fn sell_ix(
    ctx: &PoolContext,
    user: &Pubkey,
    user_base_token_account: &Pubkey,
    cfg: &GlobalConfig,
    nonce: u64,
    base_amount_in: u64,
    min_quote_out: u64,
) -> Instruction {
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&disc::SELL);
    data.extend_from_slice(&base_amount_in.to_le_bytes());
    data.extend_from_slice(&min_quote_out.to_le_bytes());
    Instruction {
        program_id: *PROGRAM,
        accounts: sell_accounts(ctx, user, user_base_token_account, cfg, nonce),
        data,
    }
}

/// `track_volume` byte on `Buy`. Send `0x01` (87 of 114 landed buys) or omit the
/// byte; never `0x00`, whose effect on accumulator rent is unproven.
pub const TRACK_VOLUME_ON: u8 = 0x01;

/// PDA(`["global_volume_accumulator"]`, PumpSwap) — a single account for the whole
/// program, so this is effectively a constant.
///
/// Read-only is correct and is what we send. A minority of clients mark it
/// writable, which also lands, but needlessly takes a write lock on a globally hot
/// account — the last thing you want when you are racing for a slot.
pub fn global_volume_accumulator() -> Pubkey {
    Pubkey::find_program_address(&[b"global_volume_accumulator"], &PROGRAM).0
}

/// The accounts of PumpSwap `Buy`, in on-chain order: **26** for an ordinary pool,
/// **27** for a cashback pool.
///
/// ## The one thing that is not guessable from `Sell`
///
/// Slots `0..=18` are byte-identical to [`sell_accounts`], flags included. Then Buy
/// inserts **two** accounts at 19 and 20 that Sell does not carry at all:
///
/// * `19 global_volume_accumulator` — read-only
/// * `20 user_volume_accumulator` — **writable**
///
/// and everything from Sell's index 19 onward shifts by two. On Sell these
/// accumulators appear only for cashback pools; on Buy they are **mandatory even
/// for ordinary pools**. Deriving this list by analogy from `Sell` binds
/// `fee_config` at slot 19 and the transaction fails on chain.
///
/// Verified independently twice — one pass sampling recent program traffic, one
/// pass through the reference wallet's own buys plus the program's on-chain IDL and
/// ELF string table — agreeing on all 26 slots, names and flags. Non-cashback
/// 19/19 and 20/20; cashback 11/11 and 55/55.
///
/// The cashback variant inserts only **one** account, `ATA(uva, WSOL, Tokenkeg)` at
/// index 23 — unlike cashback `Sell`, which inserts two at 21. It is one rather
/// than two precisely because the `user_volume_accumulator` is already a named
/// account here at slot 20.
pub fn buy_accounts(
    ctx: &PoolContext,
    user: &Pubkey,
    user_base_token_account: &Pubkey,
    cfg: &GlobalConfig,
    nonce: u64,
) -> Vec<AccountMeta> {
    let tkeg = *TOKEN_PROGRAM;
    let wsol = *WSOL_MINT;
    let protocol = cfg.protocol_fee_recipient(nonce);
    let cc_auth = coin_creator_vault_authority(&ctx.coin_creator);
    let uva = user_volume_accumulator(user);
    let mut metas = vec![
        // ---- 0..=18: identical to Sell ----
        AccountMeta::new(ctx.pool, false),
        AccountMeta::new(*user, true),
        AccountMeta::new_readonly(*GLOBAL_CONFIG, false),
        AccountMeta::new_readonly(ctx.mint, false),
        AccountMeta::new_readonly(wsol, false),
        AccountMeta::new(*user_base_token_account, false),
        AccountMeta::new(pumpfun::ata(user, &wsol, &tkeg), false),
        AccountMeta::new(pumpfun::ata(&ctx.pool, &ctx.mint, &ctx.base_token_program), false),
        AccountMeta::new(pumpfun::ata(&ctx.pool, &wsol, &tkeg), false),
        AccountMeta::new_readonly(protocol, false),
        AccountMeta::new(pumpfun::ata(&protocol, &wsol, &tkeg), false),
        AccountMeta::new_readonly(ctx.base_token_program, false),
        AccountMeta::new_readonly(tkeg, false),
        AccountMeta::new_readonly(*SYSTEM_PROGRAM, false),
        AccountMeta::new_readonly(*ATA_PROGRAM, false),
        AccountMeta::new_readonly(*EVENT_AUTHORITY, false),
        AccountMeta::new_readonly(*PROGRAM, false),
        AccountMeta::new(pumpfun::ata(&cc_auth, &wsol, &tkeg), false),
        AccountMeta::new_readonly(cc_auth, false),
        // ---- 19, 20: Buy-only, and the reason this function exists ----
        AccountMeta::new_readonly(global_volume_accumulator(), false),
        AccountMeta::new(uva, false),
        // ---- 21, 22: Sell's 19, 20 ----
        AccountMeta::new_readonly(*FEE_CONFIG, false),
        AccountMeta::new_readonly(*FEE_PROGRAM, false),
    ];
    if ctx.is_cashback {
        metas.push(AccountMeta::new(pumpfun::ata(&uva, &wsol, &tkeg), false));
    }
    metas.push(AccountMeta::new_readonly(pool_v2(&ctx.mint), false));
    metas.push(AccountMeta::new_readonly(cfg.buyback_fee_recipient, false));
    metas.push(AccountMeta::new(
        pumpfun::ata(&cfg.buyback_fee_recipient, &wsol, &tkeg),
        false,
    ));
    metas
}

/// `Buy(base_amount_out, max_quote_amount_in)` — an **exact-out** order.
///
/// Note the shape: you name the tokens you want, and `max_quote_amount_in` is a
/// slippage **cap**, not the amount spent. Observed fills always came in strictly
/// under the cap, and some senders pass `u64::MAX` for it, which only makes sense
/// as a bound. Size with [`quote_in_for_base_out`] and set the cap above it.
///
/// ## Payload length
///
/// A census of **114 landed buys**: 25 bytes with a trailing `0x01` (87), 24 bytes
/// with the byte omitted (22), 26 bytes with `0x0101` (4), and 25 with `0x00` (1).
/// All four land on the current program. We send the modal 25-byte form.
///
/// The trailing byte is `track_volume`, typed in the IDL as `OptionBool` — which is
/// **not** a Rust `Option`; the IDL defines it as `struct { bool }`, exactly one raw
/// byte. Never send `0x00`: only one instance exists in 114 buys, and that wallet's
/// volume accumulator already existed, so whether `0x00` skips the accumulator's
/// 1,844,400-lamport rent is unproven. The second byte in the 26-byte form is
/// unexplained and must not be encoded.
///
/// ## This instruction does NOT create your base token account
///
/// Of 17 sampled buys whose slot-5 account started empty, 16 were created by a
/// separate top-level `AssociatedTokenAccount` instruction and one by the caller's
/// own router CPI immediately before. Prepend
/// [`crate::live::create_ata_idempotent`] for slot 5 or the buy fails.
///
/// It *does* create the `user_volume_accumulator` at slot 20 if missing, charging
/// the user 1,844,400 lamports of rent on a first-ever buy.
pub fn buy_ix(
    ctx: &PoolContext,
    user: &Pubkey,
    user_base_token_account: &Pubkey,
    cfg: &GlobalConfig,
    nonce: u64,
    base_amount_out: u64,
    max_quote_amount_in: u64,
) -> Instruction {
    let mut data = Vec::with_capacity(25);
    data.extend_from_slice(&disc::BUY);
    data.extend_from_slice(&base_amount_out.to_le_bytes());
    data.extend_from_slice(&max_quote_amount_in.to_le_bytes());
    data.push(TRACK_VOLUME_ON);
    Instruction {
        program_id: *PROGRAM,
        accounts: buy_accounts(ctx, user, user_base_token_account, cfg, nonce),
        data,
    }
}

/// Inverse of [`base_out`]: quote needed to receive exactly `base_amount_out`.
///
/// Rounds **up** — an exact-out order must not come up short. Returns `None` if the
/// pool cannot supply that many tokens at any price. Before fees, matching the
/// convention of the other quote helpers here.
pub fn quote_in_for_base_out(
    base_reserves: u64,
    quote_reserves: u64,
    boost_quote: u64,
    base_amount_out: u64,
) -> Option<u64> {
    let q = (quote_reserves as u128).saturating_add(boost_quote as u128);
    let b = base_reserves as u128;
    let out = base_amount_out as u128;
    if out == 0 {
        return Some(0);
    }
    if out >= b || q == 0 {
        return None;
    }
    u64::try_from((out * q).div_ceil(b - out)).ok()
}

/// Constant-product quote: base in, quote out, before fees.
///
/// `boost_quote` is virtual liquidity held in the pool struct, not in the token
/// account, and it **must** be added to the quote reserve. With it the CPMM
/// reproduces observed mainnet fills 79/79; without it, 0/79.
pub fn quote_out(base_reserves: u64, quote_reserves: u64, boost_quote: u64, base_in: u64) -> u64 {
    let quote_reserves = quote_reserves.saturating_add(boost_quote);
    if base_in == 0 || base_reserves == 0 || quote_reserves == 0 {
        return 0;
    }
    let b = base_reserves as u128;
    let q = quote_reserves as u128;
    let k = b * q;
    let new_b = b + base_in as u128;
    let new_q = k / new_b;
    q.saturating_sub(new_q) as u64
}

/// Constant-product quote: quote in, base out, before fees.
///
/// See [`quote_out`] on why `boost_quote` is not optional.
pub fn base_out(base_reserves: u64, quote_reserves: u64, boost_quote: u64, quote_in: u64) -> u64 {
    let quote_reserves = quote_reserves.saturating_add(boost_quote);
    if quote_in == 0 || base_reserves == 0 || quote_reserves == 0 {
        return 0;
    }
    let b = base_reserves as u128;
    let q = quote_reserves as u128;
    let k = b * q;
    let new_q = q + quote_in as u128;
    let new_b = k / new_q;
    b.saturating_sub(new_b) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every value below is from the real mainnet Sell in transaction
    // 5TQRTDdCohkDxkpR81V8VjciWfP2ZHUVefdNL9ML3BkjxnBMHnA72AMre4GqEHo1n8Brcxa6F9VC3iPhfkQCKHEw.
    const MINT: &str = "5yJXeCFGbkQaKeUfNDkjRN2EYgsTAhZHswnoosEZpump";
    const USER: &str = "niggerd597QYedtvjQDVHZTCCGyJrwHNm2i49dkm5zS";
    const POOL: &str = "4vTGNow7KtbKLxDrqcHLkR2WTEbJfuYCyTavGzkyXgnb";
    const POOL_BASE_TA: &str = "7bgGaK6vzEijnrNh1LUauKkYFRX8y66U9gspYB5oYoQJ";
    const POOL_QUOTE_TA: &str = "Cym7JBVQnhYTavA2nHE8i9YazcLyt1icgJx16kjMw8jH";
    const PROTOCOL_FEE: &str = "7VtfL8fvgNfhz17qKRMjzQEXgbdpnHHHQRh54R9jP2RJ";
    const PROTOCOL_FEE_TA: &str = "7GFUN3bWzJMKMRZ34JLsvcqdssDbXnp589SiE33KVwcC";
    const CC_VAULT_AUTH: &str = "2aGqXtKC5TkptNtMJa6YZvrmFgZSbGBWCJwaAWH5i461";
    const CC_VAULT_ATA: &str = "pqYCxRdbFezijcCUNm4cjAJWPzxvEQ4MkLWrxp2qE1u";
    const POOL_V2: &str = "ButcAyw93f1dEMFk9goZWTNppFiBqMtCYcDPJXriTb3k";
    const BUYBACK: &str = "5eHhjP8JaYkz83CWwvGU2uMUXefd3AazWGx4gpcuEEYD";
    const BUYBACK_TA: &str = "CASRL2zkwDnppxEFQ4LgdwgR9pdz5Q8R8nEMKVZ9QoLp";

    fn key(s: &str) -> Pubkey {
        Pubkey::from_str(s).unwrap()
    }

    fn t22() -> Pubkey {
        *pumpfun::TOKEN_2022_PROGRAM
    }

    #[test]
    fn pool_derivation_matches_mainnet() {
        assert_eq!(pool(&key(MINT), &WSOL_MINT), key(POOL));
    }

    #[test]
    fn pool_token_accounts_match_mainnet() {
        assert_eq!(
            pumpfun::ata(&key(POOL), &key(MINT), &t22()),
            key(POOL_BASE_TA)
        );
        assert_eq!(
            pumpfun::ata(&key(POOL), &WSOL_MINT, &TOKEN_PROGRAM),
            key(POOL_QUOTE_TA)
        );
    }

    #[test]
    fn pool_v2_is_seeded_by_the_mint_not_the_pool() {
        assert_eq!(pool_v2(&key(MINT)), key(POOL_V2));
        // Seeding by the pool gives a different, wrong account.
        assert_ne!(
            pool_v2(&key(MINT)),
            Pubkey::find_program_address(&[b"pool-v2", key(POOL).as_ref()], &PROGRAM).0
        );
    }

    #[test]
    fn coin_creator_vault_uses_an_underscore_under_pumpswap() {
        // pump.fun's equivalent is "creator-vault" (hyphen) under pump.fun. Getting
        // this wrong yields a plausible address the program rejects.
        let cc = Pubkey::new_unique();
        let ours = coin_creator_vault_authority(&cc);
        assert_eq!(
            ours,
            Pubkey::find_program_address(&[b"creator_vault", cc.as_ref()], &PROGRAM).0
        );
        assert_ne!(ours, pumpfun::creator_vault(&cc), "that is pump.fun's, not PumpSwap's");
        assert_ne!(
            ours,
            Pubkey::find_program_address(&[b"creator-vault", cc.as_ref()], &PROGRAM).0,
            "hyphen vs underscore must differ"
        );
    }

    #[test]
    fn fee_vault_atas_match_mainnet() {
        assert_eq!(
            pumpfun::ata(&key(CC_VAULT_AUTH), &WSOL_MINT, &TOKEN_PROGRAM),
            key(CC_VAULT_ATA)
        );
        assert_eq!(
            pumpfun::ata(&key(PROTOCOL_FEE), &WSOL_MINT, &TOKEN_PROGRAM),
            key(PROTOCOL_FEE_TA)
        );
        assert_eq!(
            pumpfun::ata(&key(BUYBACK), &WSOL_MINT, &TOKEN_PROGRAM),
            key(BUYBACK_TA)
        );
    }

    fn cfg() -> GlobalConfig {
        let mut r = vec![Pubkey::new_unique(); 8];
        r[1] = key(PROTOCOL_FEE);
        GlobalConfig {
            protocol_fee_recipients: r,
            buyback_fee_recipient: key(BUYBACK),
        }
    }

    #[test]
    fn sell_reproduces_the_real_mainnet_account_list() {
        let ctx = PoolContext {
            mint: key(MINT),
            quote_mint: *WSOL_MINT,
            base_token_program: t22(),
            pool: key(POOL),
            // Cannot reproduce the observed authority without the real creator, so
            // this asserts the slots that do not depend on it, plus the 17/18
            // relationship which holds for any creator.
            coin_creator: Pubkey::new_unique(),
            is_cashback: false,
        };
        let user = key("niggerd597QYedtvjQDVHZTCCGyJrwHNm2i49dkm5zS");
        let a = sell_accounts(&ctx, &user, &Pubkey::new_unique(), &cfg(), 1);
        assert_eq!(a.len(), 24);
        assert_eq!(a[0].pubkey, key(POOL));
        assert_eq!(a[2].pubkey, *GLOBAL_CONFIG);
        assert_eq!(a[3].pubkey, key(MINT));
        assert_eq!(a[4].pubkey, *WSOL_MINT);
        assert_eq!(a[7].pubkey, key(POOL_BASE_TA));
        assert_eq!(a[8].pubkey, key(POOL_QUOTE_TA));
        assert_eq!(a[9].pubkey, key(PROTOCOL_FEE));
        assert_eq!(a[10].pubkey, key(PROTOCOL_FEE_TA));
        assert_eq!(a[11].pubkey, t22());
        assert_eq!(a[12].pubkey, *TOKEN_PROGRAM, "quote side is always legacy");
        assert_eq!(a[15].pubkey, *EVENT_AUTHORITY);
        assert_eq!(a[16].pubkey, *PROGRAM);
        assert_eq!(a[19].pubkey, *FEE_CONFIG);
        assert_eq!(a[20].pubkey, *FEE_PROGRAM);
        assert_eq!(a[21].pubkey, key(POOL_V2));
        assert_eq!(a[22].pubkey, key(BUYBACK));
        assert_eq!(a[23].pubkey, key(BUYBACK_TA));
        // Slot 17 must always be slot 18's WSOL ATA, whatever the creator is.
        assert_eq!(
            a[17].pubkey,
            pumpfun::ata(&a[18].pubkey, &WSOL_MINT, &TOKEN_PROGRAM)
        );
    }

    #[test]
    fn sell_flags_match_the_observed_transaction() {
        let ctx = PoolContext::new(key(MINT), Pubkey::new_unique(), t22());
        let a = sell_accounts(&ctx, &Pubkey::new_unique(), &Pubkey::new_unique(), &cfg(), 0);
        let writable: Vec<usize> = a
            .iter()
            .enumerate()
            .filter(|(_, m)| m.is_writable)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(writable, vec![0, 1, 5, 6, 7, 8, 10, 17, 23]);
        let signers: Vec<usize> = a
            .iter()
            .enumerate()
            .filter(|(_, m)| m.is_signer)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(signers, vec![1], "only the user signs");
    }

    #[test]
    fn sell_instruction_encodes_disc_and_amounts() {
        let ctx = PoolContext::new(key(MINT), Pubkey::new_unique(), t22());
        let ix = sell_ix(
            &ctx,
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &cfg(),
            0,
            2_646_178_233_897,
            0,
        );
        assert_eq!(ix.program_id, *PROGRAM);
        assert_eq!(ix.data.len(), 24);
        assert_eq!(&ix.data[..8], &disc::SELL);
        assert_eq!(
            u64::from_le_bytes(ix.data[8..16].try_into().unwrap()),
            2_646_178_233_897
        );
    }

    #[test]
    fn global_config_decodes_both_fee_pools() {
        let mut d = vec![0u8; 900];
        let want: Vec<Pubkey> = (0..8).map(|_| Pubkey::new_unique()).collect();
        for (i, k) in want.iter().enumerate() {
            let o = OFF_PROTOCOL_FEES + i * 32;
            d[o..o + 32].copy_from_slice(k.as_ref());
        }
        let bb = Pubkey::new_unique();
        d[OFF_BUYBACK_FEE..OFF_BUYBACK_FEE + 32].copy_from_slice(bb.as_ref());
        let g = GlobalConfig::decode(&d).unwrap();
        assert_eq!(g.protocol_fee_recipients, want);
        assert_eq!(g.buyback_fee_recipient, bb);
        assert_eq!(g.protocol_fee_recipient(1), want[1]);
    }

    #[test]
    fn global_config_rejects_short_or_empty() {
        assert!(GlobalConfig::decode(&[0u8; 100]).is_err());
        assert!(GlobalConfig::decode(&vec![0u8; 900]).is_err());
    }

    #[test]
    fn coin_creator_reads_from_the_pool_account() {
        let mut d = vec![0u8; POOL_LEN];
        let cc = Pubkey::new_unique();
        d[POOL_COIN_CREATOR_OFFSET..POOL_COIN_CREATOR_OFFSET + 32].copy_from_slice(cc.as_ref());
        assert_eq!(coin_creator_from_pool(&d).unwrap(), cc);
        assert!(coin_creator_from_pool(&[0u8; 10]).is_err());
    }

    #[test]
    fn constant_product_quotes_are_symmetric_and_bounded() {
        let (b, q, boost) = (206_900_000_000_000u64, 67_406_000_000u64, 0u64);
        let out = quote_out(b, q, boost, 1_000_000_000_000);
        assert!(out > 0 && out < q);
        let got = base_out(b, q, boost, 1_000_000_000);
        assert!(got > 0 && got < b);
        assert_eq!(quote_out(b, q, boost, 0), 0);
        assert_eq!(base_out(b, q, boost, 0), 0);
        assert_eq!(quote_out(0, 0, 0, 5), 0);
    }

    #[test]
    fn the_boost_term_changes_the_price() {
        // Ignoring boost_quote reproduced 0 of 79 observed mainnet fills; including
        // it reproduced all 79. So it must actually move the number.
        let (b, q) = (206_900_000_000_000u64, 67_406_000_000u64);
        let boost = 13_946_100_000u64; // floor(67.406e9 * 2069 / 10000)
        let without = base_out(b, q, 0, 5_000_000_000);
        let with = base_out(b, q, boost, 5_000_000_000);
        assert!(with < without, "more quote liquidity must buy fewer tokens");
        let diff = (without - with) as f64 / without as f64;
        assert!(diff > 0.10, "boost only shifted the price by {diff:.4}");
    }

    #[test]
    fn a_cashback_pool_builds_the_twenty_six_account_form() {
        let mut d = vec![0u8; POOL_LEN];
        d[OFF_IS_CASHBACK] = 1;
        let p = Pool::decode(&d).unwrap();
        assert!(p.is_cashback_coin);
        assert_eq!(p.sell_account_count(), 26);

        let ctx = PoolContext::from_pool(key(MINT), t22(), &d).expect("supported now");
        assert!(ctx.is_cashback);
        let user = key(USER);
        let a = sell_accounts(&ctx, &user, &Pubkey::new_unique(), &cfg(), 1);
        assert_eq!(a.len(), 26);

        // The two extras are INSERTED at 21, not appended - this is the whole
        // reason a 24-account sell cannot be reused here.
        let uva = user_volume_accumulator(&user);
        assert_eq!(a[21].pubkey, pumpfun::ata(&uva, &WSOL_MINT, &TOKEN_PROGRAM));
        assert_eq!(a[22].pubkey, uva);
        assert_eq!(a[23].pubkey, pool_v2(&key(MINT)), "pool_v2 shifts 21 -> 23");
        assert_eq!(a[24].pubkey, key(BUYBACK), "buyback shifts 22 -> 24");
        assert_eq!(a[25].pubkey, key(BUYBACK_TA));
    }

    #[test]
    fn an_ordinary_pool_still_builds_twenty_four() {
        let d = vec![0u8; POOL_LEN];
        let p = Pool::decode(&d).unwrap();
        assert!(!p.is_cashback_coin);
        assert_eq!(p.sell_account_count(), 24);
        let ctx = PoolContext::from_pool(key(MINT), t22(), &d).unwrap();
        let a = sell_accounts(&ctx, &key(USER), &Pubkey::new_unique(), &cfg(), 1);
        assert_eq!(a.len(), 24);
        assert_eq!(a[21].pubkey, pool_v2(&key(MINT)));
        assert_eq!(a[22].pubkey, key(BUYBACK));
    }

    #[test]
    fn the_two_forms_agree_on_everything_before_index_21() {
        let ord = PoolContext::from_pool(key(MINT), t22(), &vec![0u8; POOL_LEN]).unwrap();
        let mut d = vec![0u8; POOL_LEN];
        d[OFF_IS_CASHBACK] = 1;
        let cash = PoolContext::from_pool(key(MINT), t22(), &d).unwrap();
        let u = key(USER);
        let ub = Pubkey::new_unique();
        let a = sell_accounts(&ord, &u, &ub, &cfg(), 1);
        let b = sell_accounts(&cash, &u, &ub, &cfg(), 1);
        for i in 0..21 {
            assert_eq!(a[i].pubkey, b[i].pubkey, "slot {i} diverged");
            assert_eq!(a[i].is_writable, b[i].is_writable, "slot {i} flags diverged");
        }
    }

    #[test]
    fn pool_decode_reads_the_mutable_coin_creator_and_boost() {
        let mut d = vec![0u8; POOL_LEN];
        let cc = Pubkey::new_unique();
        d[POOL_COIN_CREATOR_OFFSET..POOL_COIN_CREATOR_OFFSET + 32].copy_from_slice(cc.as_ref());
        d[OFF_BOOST_QUOTE..OFF_BOOST_QUOTE + 8].copy_from_slice(&999_u64.to_le_bytes());
        d[OFF_IS_MAYHEM] = 1;
        let p = Pool::decode(&d).unwrap();
        assert_eq!(p.coin_creator, cc);
        assert_eq!(p.boost_quote, 999);
        assert!(p.is_mayhem_mode);
        assert_eq!(p.sell_account_count(), 24, "mayhem mode is not cashback");
        assert!(Pool::decode(&d[..100]).is_err());
    }

    #[test]
    fn buying_then_selling_the_same_pool_is_flat_to_within_rounding() {
        // Both directions floor-divide, so an instant round trip can come back one
        // lamport ABOVE the input. That is integer rounding, not free money: the
        // real program applies the same integer math and then takes lp, protocol
        // and coin-creator fees on top, which dominate a single lamport by six
        // orders of magnitude. The test pins the tolerance so a genuine sign error
        // in the maths - which would show up as a large gain - still fails.
        let (b, q) = (206_900_000_000_000u64, 67_406_000_000u64);
        for quote_in in [1_000_000u64, 100_000_000, 5_000_000_000, 30_000_000_000] {
            let got = base_out(b, q, 0, quote_in);
            let back = quote_out(b - got, q + quote_in, 0, got);
            assert!(
                back <= quote_in + 1,
                "round trip of {quote_in} returned {back} - more than rounding"
            );
            // And it must not silently return nothing either.
            assert!(back > quote_in * 99 / 100, "round trip lost too much: {back}");
        }
    }

    // -----------------------------------------------------------------------
    // Buy. The account list was recovered twice, independently, and the two
    // passes agreed on all 26 slots including writable/signer flags.
    // -----------------------------------------------------------------------

    /// Observed on chain by both decoding passes.
    const GLOBAL_VOL_ACC: &str = "C2aFPdENg4A2HQsmrd5rTw5TaYBX5Ku887cWjbFKtZpw";

    #[test]
    fn global_volume_accumulator_matches_the_observed_address() {
        assert_eq!(global_volume_accumulator(), key(GLOBAL_VOL_ACC));
    }

    #[test]
    fn buy_shares_its_first_nineteen_slots_with_sell() {
        let ctx = PoolContext::new(key(MINT), Pubkey::new_unique(), t22());
        let user = key(USER);
        let uba = Pubkey::new_unique();
        // One cfg: it randomises its recipient array, so two calls disagree at
        // slot 9 for reasons that have nothing to do with Buy.
        let c = cfg();
        let s = sell_accounts(&ctx, &user, &uba, &c, 0);
        let b = buy_accounts(&ctx, &user, &uba, &c, 0);
        for i in 0..19 {
            assert_eq!(b[i].pubkey, s[i].pubkey, "slot {i} pubkey");
            assert_eq!(b[i].is_writable, s[i].is_writable, "slot {i} writable");
            assert_eq!(b[i].is_signer, s[i].is_signer, "slot {i} signer");
        }
    }

    #[test]
    fn buy_inserts_both_volume_accumulators_even_without_cashback() {
        // The trap. On Sell these appear only for cashback pools; on Buy they are
        // mandatory. Getting this wrong binds fee_config at slot 19 on chain.
        let ctx = PoolContext::new(key(MINT), Pubkey::new_unique(), t22());
        let user = key(USER);
        let b = buy_accounts(&ctx, &user, &Pubkey::new_unique(), &cfg(), 0);
        assert_eq!(b.len(), 26, "ordinary pool Buy takes 26 accounts");
        assert_eq!(b[19].pubkey, global_volume_accumulator());
        assert!(!b[19].is_writable, "global accumulator is read-only");
        assert_eq!(b[20].pubkey, user_volume_accumulator(&user));
        assert!(b[20].is_writable, "user accumulator MUST be writable");
    }

    #[test]
    fn buy_tail_is_sells_tail_shifted_by_two() {
        let ctx = PoolContext::new(key(MINT), Pubkey::new_unique(), t22());
        let b = buy_accounts(&ctx, &key(USER), &Pubkey::new_unique(), &cfg(), 1);
        assert_eq!(b[21].pubkey, *FEE_CONFIG);
        assert_eq!(b[22].pubkey, *FEE_PROGRAM);
        assert_eq!(b[23].pubkey, key(POOL_V2));
        assert_eq!(b[24].pubkey, key(BUYBACK));
        assert_eq!(b[25].pubkey, key(BUYBACK_TA));
    }

    #[test]
    fn cashback_buy_inserts_one_account_where_cashback_sell_inserts_two() {
        let user = key(USER);
        let mut ctx = PoolContext::new(key(MINT), Pubkey::new_unique(), t22());
        ctx.is_cashback = true;
        let b = buy_accounts(&ctx, &user, &Pubkey::new_unique(), &cfg(), 1);
        assert_eq!(b.len(), 27);
        // Only the accumulator's WSOL ATA is added - the accumulator itself is
        // already a named account at slot 20.
        let uva = user_volume_accumulator(&user);
        assert_eq!(b[23].pubkey, pumpfun::ata(&uva, &WSOL_MINT, &TOKEN_PROGRAM));
        assert!(b[23].is_writable);
        assert_eq!(b[24].pubkey, key(POOL_V2), "pool_v2 shifts 23 -> 24");
        assert_eq!(b[25].pubkey, key(BUYBACK));
        assert_eq!(b[26].pubkey, key(BUYBACK_TA));
    }

    #[test]
    fn buy_account_counts_match_the_pool_flag() {
        let ordinary = Pool {
            base_mint: key(MINT),
            quote_mint: *WSOL_MINT,
            pool_base_token_account: key(POOL_BASE_TA),
            pool_quote_token_account: key(POOL_QUOTE_TA),
            coin_creator: Pubkey::new_unique(),
            is_mayhem_mode: false,
            is_cashback_coin: false,
            boost_quote: 0,
        };
        let mut cash = ordinary.clone();
        cash.is_cashback_coin = true;
        assert_eq!(ordinary.buy_account_count(), 26);
        assert_eq!(cash.buy_account_count(), 27);
        assert_eq!(ordinary.sell_account_count(), 24);
        assert_eq!(cash.sell_account_count(), 26);
    }

    #[test]
    fn buy_flags_match_the_observed_transactions() {
        let ctx = PoolContext::new(key(MINT), Pubkey::new_unique(), t22());
        let b = buy_accounts(&ctx, &Pubkey::new_unique(), &Pubkey::new_unique(), &cfg(), 0);
        let writable: Vec<usize> = b
            .iter()
            .enumerate()
            .filter(|(_, m)| m.is_writable)
            .map(|(i, _)| i)
            .collect();
        // The modal set across landed buys. Message-level flags from
        // getTransaction are an upper bound, not a per-instruction minimum, so
        // this pins our construction rather than proving minimality.
        assert_eq!(writable, vec![0, 1, 5, 6, 7, 8, 10, 17, 20, 25]);
        let signers: Vec<usize> = b
            .iter()
            .enumerate()
            .filter(|(_, m)| m.is_signer)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(signers, vec![1], "only the user signs");
    }

    #[test]
    fn buy_instruction_encodes_disc_and_amounts() {
        let ctx = PoolContext::new(key(MINT), Pubkey::new_unique(), t22());
        let ix = buy_ix(
            &ctx,
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &cfg(),
            0,
            771_855_190_066,
            355_082_050,
        );
        assert_eq!(ix.program_id, *PROGRAM);
        assert_eq!(ix.data.len(), 25, "modal form: args plus track_volume");
        assert_eq!(ix.data[24], TRACK_VOLUME_ON, "never send 0x00");
        assert_eq!(&ix.data[..8], &disc::BUY);
        assert_eq!(
            u64::from_le_bytes(ix.data[8..16].try_into().unwrap()),
            771_855_190_066,
            "arg0 is base_amount_out"
        );
        assert_eq!(
            u64::from_le_bytes(ix.data[16..24].try_into().unwrap()),
            355_082_050,
            "arg1 is the max_quote_amount_in cap"
        );
    }

    #[test]
    fn quote_in_for_base_out_inverts_base_out() {
        let (b, q, boost) = (206_900_000_000_000u64, 67_406_000_000u64, 17_584_500_000u64);
        for quote_in in [1_000_000u64, 100_000_000, 2_000_000_000, 20_000_000_000] {
            let got = base_out(b, q, boost, quote_in);
            let need = quote_in_for_base_out(b, q, boost, got).unwrap();
            // base_out floors, so recovering exactly that many tokens can cost one
            // lamport more than the original input. Overshooting by more than that
            // would mean the inverse is wrong.
            assert!(need <= quote_in + 1, "need {need} vs {quote_in}");
            assert!(
                base_out(b, q, boost, need) >= got,
                "paying {need} must still deliver {got}"
            );
        }
    }

    #[test]
    fn quote_in_for_base_out_refuses_to_drain_the_pool() {
        let (b, q) = (206_900_000_000_000u64, 67_406_000_000u64);
        assert_eq!(quote_in_for_base_out(b, q, 0, 0), Some(0));
        assert_eq!(quote_in_for_base_out(b, q, 0, b), None, "cannot take it all");
        assert_eq!(quote_in_for_base_out(b, q, 0, b + 1), None);
    }
}
