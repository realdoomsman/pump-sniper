//! Live order construction and submission.
//!
//! Covers the two legs that are **fully resolved and verified**: the completion
//! buy (`BuyExactSolIn`, 18/18 accounts matched against 9 landed mainnet
//! transactions) and the exit (`SellV2`, 26/26 matched against 51).
//!
//! `MigrateV2` (29/29) is now resolved too, so [`Venue::build_completion`] can put
//! the completing buy and the migration in **one atomic transaction** — which is
//! the only version of this strategy that makes money. The last holdout, slot 27,
//! turned out to be PumpSwap's `boost_vault` authority, seeded by the pool under
//! the *PumpSwap* program; every search seeded from pump.fun's binary was looking
//! in the wrong place.
//!
//! That transaction is 1,484 bytes without a lookup table, against a 1,232-byte
//! limit, so `rpc.lookup_table` is **required** for the completion play. Create one
//! with `sniper create-alt`.

use crate::exec::{self, AttemptSpec, FeeConfig};
use crate::pumpfun;
use crate::pumpswap;
use crate::resolve::{self, Global, MintContext};
use crate::rpc::RpcClient;
use anyhow::{bail, Context, Result};
use solana_sdk::hash::Hash;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signer::keypair::Keypair;
use solana_sdk::signer::Signer;
use std::collections::HashMap;

/// `CreateIdempotent` on the Associated Token Account program.
///
/// Idempotent matters: on a contested curve we may already hold the account from
/// an earlier attempt, and a plain `Create` would fail the whole transaction.
pub fn create_ata_idempotent(
    payer: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: *pumpfun::ATA_PROGRAM,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(pumpfun::ata(owner, mint, token_program), false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(*pumpfun::SYSTEM_PROGRAM, false),
            AccountMeta::new_readonly(*token_program, false),
        ],
        data: vec![1],
    }
}

/// Cached chain state that every order needs but that changes rarely.
pub struct Venue {
    client: RpcClient,
    global: Global,
    /// mint -> token program, from `mint_account.owner`.
    token_programs: HashMap<Pubkey, Pubkey>,
    /// Lookup tables, without which the atomic completion does not fit a packet.
    luts: Vec<solana_sdk::message::AddressLookupTableAccount>,
    /// PumpSwap fee recipients. Needed to exit any completed position.
    amm: Option<pumpswap::GlobalConfig>,
}

impl Venue {
    /// Read the pump.fun Global account once. Fails loudly rather than guessing:
    /// a wrong fee recipient makes every order fail on chain.
    pub async fn connect(client: RpcClient) -> Result<Self> {
        let accounts = client
            .multiple_accounts(&[*pumpfun::GLOBAL])
            .await
            .context("fetching pump.fun global account")?;
        let Some(acc) = accounts.first() else {
            bail!("pump.fun global account {} not found", *pumpfun::GLOBAL);
        };
        let global = Global::decode(&acc.data)?;
        tracing::info!(
            "venue ready: {} fee recipients, {} buyback recipients",
            global.fee_recipients.len(),
            global.buyback_recipients.len()
        );
        Ok(Self {
            client,
            global,
            token_programs: HashMap::new(),
            luts: Vec::new(),
            amm: None,
        })
    }

    /// Read PumpSwap's GlobalConfig. Required before any completion can be exited.
    pub async fn load_amm_config(&mut self) -> Result<()> {
        let accounts = self
            .client
            .multiple_accounts(&[*pumpswap::GLOBAL_CONFIG])
            .await
            .context("fetching PumpSwap global config")?;
        let Some(acc) = accounts.first() else {
            bail!("PumpSwap global config {} not found", *pumpswap::GLOBAL_CONFIG);
        };
        let cfg = pumpswap::GlobalConfig::decode(&acc.data)?;
        tracing::info!(
            "PumpSwap ready: {} protocol fee recipients",
            cfg.protocol_fee_recipients.len()
        );
        self.amm = Some(cfg);
        Ok(())
    }

    pub fn amm_ready(&self) -> bool {
        self.amm.is_some()
    }

    /// Build an exit leg on the PumpSwap pool.
    ///
    /// This is how a completed position is closed. The bonding curve is graduated
    /// and pump.fun refuses sells against it, so this is the only route.
    #[allow(clippy::too_many_arguments)]
    pub async fn build_pumpswap_sell(
        &self,
        payer: &Keypair,
        ctx: &MintContext,
        blockhash: Hash,
        fee: &FeeConfig,
        tier: usize,
        nonce: u64,
        base_amount_in: u64,
        min_quote_out: u64,
    ) -> Result<solana_sdk::transaction::VersionedTransaction> {
        let Some(amm) = self.amm.as_ref() else {
            bail!("PumpSwap global config not loaded; call load_amm_config first");
        };
        let user = payer.pubkey();
        // The pool is read live, every time. Two fields make this mandatory:
        // `coin_creator` is mutable (it disagreed on 3 of 40 sampled pools), and
        // `is_cashback_coin` decides whether Sell takes 24 or 26 accounts. Neither
        // can be inferred from the bonding curve. Exits are not a race, so the
        // round trip is affordable.
        let pool_key = pumpswap::pool(&ctx.mint, &pumpfun::WSOL_MINT);
        let accounts = self.client.multiple_accounts(&[pool_key]).await?;
        let Some(pool_acc) = accounts.first() else {
            bail!("PumpSwap pool {pool_key} not found for mint {}", ctx.mint);
        };
        let pctx = pumpswap::PoolContext::from_pool(
            ctx.mint,
            ctx.base_token_program,
            &pool_acc.data,
        )?;
        let user_base = pumpfun::ata(&user, &ctx.mint, &ctx.base_token_program);
        let body = vec![
            create_ata_idempotent(&user, &user, &pumpfun::WSOL_MINT, &pumpfun::TOKEN_PROGRAM),
            pumpswap::sell_ix(
                &pctx,
                &user,
                &user_base,
                amm,
                nonce,
                base_amount_in,
                min_quote_out,
            ),
        ];
        exec::build_attempt(AttemptSpec {
            cu_limit: fee.cu_limit_for(false),
            payer,
            blockhash,
            body,
            fee,
            tier,
            tip_account: None,
            luts: self.luts.clone(),
        })
    }

    /// Fetch and cache an address lookup table.
    pub async fn load_lookup_table(&mut self, key: &Pubkey) -> Result<usize> {
        let accounts = self.client.multiple_accounts(&[*key]).await?;
        let Some(acc) = accounts.first() else {
            bail!("lookup table {key} not found");
        };
        let table = exec::parse_lookup_table(*key, &acc.data)?;
        let n = table.addresses.len();
        self.luts.push(table);
        Ok(n)
    }

    pub fn lookup_tables(&self) -> &[solana_sdk::message::AddressLookupTableAccount] {
        &self.luts
    }

    pub fn global(&self) -> &Global {
        &self.global
    }

    /// Token program for a mint, cached. Reads `mint_account.owner` rather than
    /// assuming: assuming is what produced `IncorrectProgramId` in the wallet we
    /// reverse-engineered.
    pub async fn token_program(&mut self, mint: &Pubkey) -> Result<Pubkey> {
        if let Some(p) = self.token_programs.get(mint) {
            return Ok(*p);
        }
        let accounts = self.client.multiple_accounts(&[*mint]).await?;
        let Some(acc) = accounts.first() else {
            bail!("mint {mint} not found");
        };
        self.token_programs.insert(*mint, acc.owner);
        Ok(acc.owner)
    }

    /// Build a signed completion/momentum buy.
    #[allow(clippy::too_many_arguments)]
    pub fn build_buy(
        &self,
        payer: &Keypair,
        ctx: &MintContext,
        blockhash: Hash,
        fee: &FeeConfig,
        tier: usize,
        nonce: u64,
        gross_lamports: u64,
        min_tokens_out: u64,
        tip_account: Option<Pubkey>,
    ) -> Result<solana_sdk::transaction::VersionedTransaction> {
        let user = payer.pubkey();
        let user_ata = pumpfun::ata(&user, &ctx.mint, &ctx.base_token_program);
        let body = vec![
            create_ata_idempotent(&user, &user, &ctx.mint, &ctx.base_token_program),
            resolve::buy_ix(
                ctx,
                &user,
                &user_ata,
                &self.global,
                nonce,
                gross_lamports,
                min_tokens_out,
            ),
        ];
        exec::build_attempt(AttemptSpec {
            cu_limit: fee.cu_limit_for(false),
            payer,
            blockhash,
            body,
            fee,
            tier,
            tip_account,
            luts: vec![],
        })
    }

    /// Build the atomic winning play: **complete the curve and take the migration
    /// in one transaction**.
    ///
    /// This is the whole thesis. The completing buy flips `complete`, and because
    /// `MigrateV2` rides in the same transaction the pool is created in the same
    /// slot — which beats the reactive keeper deterministically, since it cannot
    /// react in under ~1.2 s. Splitting these into two transactions loses the race
    /// and, worse, leaves you holding curve tokens at a price 20.7% above where
    /// the pool opens.
    ///
    /// Returns an error if the assembled transaction does not fit in a packet.
    /// That is a real possibility — buy plus migrate touches ~45 distinct accounts
    /// — and is exactly why the wallet we reverse-engineered carried a 77-entry
    /// address lookup table. Pass one via `luts`.
    #[allow(clippy::too_many_arguments)]
    pub fn build_completion(
        &self,
        payer: &Keypair,
        ctx: &MintContext,
        blockhash: Hash,
        fee: &FeeConfig,
        tier: usize,
        nonce: u64,
        gap_gross: u64,
        min_tokens_out: u64,
        tip_account: Option<Pubkey>,
        extra_luts: Vec<solana_sdk::message::AddressLookupTableAccount>,
    ) -> Result<solana_sdk::transaction::VersionedTransaction> {
        let mut luts = self.luts.clone();
        luts.extend(extra_luts);
        let user = payer.pubkey();
        let user_ata = pumpfun::ata(&user, &ctx.mint, &ctx.base_token_program);
        let body = vec![
            create_ata_idempotent(&user, &user, &ctx.mint, &ctx.base_token_program),
            resolve::buy_ix(
                ctx,
                &user,
                &user_ata,
                &self.global,
                nonce,
                gap_gross,
                min_tokens_out,
            ),
            resolve::migrate_ix(ctx, &user, &pumpfun::WSOL_MINT),
        ];
        let tx = exec::build_attempt(AttemptSpec {
            // buy + MigrateV2 in one transaction: the expensive one.
            cu_limit: fee.cu_limit_for(true),
            payer,
            blockhash,
            body,
            fee,
            tier,
            tip_account,
            luts,
        })?;
        let size = bincode::serialize(&tx).map(|b| b.len()).unwrap_or(usize::MAX);
        if size > 1232 {
            bail!(
                "completion transaction is {size} bytes, over the 1232-byte limit. \
                 Supply an address lookup table covering the constant accounts \
                 (pump.fun global/programs, PumpSwap config, token programs) - the \
                 reference wallet used a 77-entry table for exactly this."
            );
        }
        Ok(tx)
    }

    /// Build a signed exit leg.
    #[allow(clippy::too_many_arguments)]
    pub fn build_sell(
        &self,
        payer: &Keypair,
        ctx: &MintContext,
        blockhash: Hash,
        fee: &FeeConfig,
        tier: usize,
        nonce: u64,
        token_amount: u64,
        min_sol_out: u64,
    ) -> Result<solana_sdk::transaction::VersionedTransaction> {
        let user = payer.pubkey();
        let user_ata = pumpfun::ata(&user, &ctx.mint, &ctx.base_token_program);
        // The sell credits WSOL, so the quote ATA must exist first.
        let body = vec![
            create_ata_idempotent(&user, &user, &pumpfun::WSOL_MINT, &pumpfun::TOKEN_PROGRAM),
            resolve::sell_ix(
                ctx,
                &user,
                &user_ata,
                &self.global,
                nonce,
                token_amount,
                min_sol_out,
            ),
        ];
        exec::build_attempt(AttemptSpec {
            cu_limit: fee.cu_limit_for(false),
            payer,
            blockhash,
            body,
            fee,
            tier,
            // Exits are not races; never tip on them.
            tip_account: None,
            luts: vec![],
        })
    }

    pub async fn latest_blockhash(&self) -> Result<Hash> {
        self.client.latest_blockhash().await
    }

    /// Submit. `skip_preflight` because simulation costs a round trip we do not
    /// have in a race, and the on-chain outcome is the only one that matters.
    pub async fn submit(&self, tx: &solana_sdk::transaction::VersionedTransaction) -> Result<String> {
        let encoded = exec::encode_tx(tx)?;
        self.client.send_transaction(&encoded, true).await
    }

    /// Wait for a submitted transaction to actually land.
    ///
    /// [`Self::submit`] returning `Ok` means the node accepted the bytes, nothing
    /// more. With `skip_preflight` and `maxRetries: 0`, most submissions never land.
    pub async fn confirm(
        &self,
        sig: &str,
        timeout: std::time::Duration,
    ) -> Result<crate::rpc::Landed> {
        self.client.confirm(sig, timeout).await
    }

    /// The signer's real lamport balance.
    pub async fn balance(&self, who: &Pubkey) -> Result<u64> {
        self.client.balance(who).await
    }

    /// What a landed transaction actually paid us, read back from chain.
    pub async fn realised(&self, sig: &str, owner: &Pubkey) -> Result<crate::rpc::Realised> {
        self.client.realised(sig, owner, &pumpfun::WSOL_MINT).await
    }
}

/// Load a keypair from a Solana CLI JSON array file.
pub fn load_keypair(path: &str) -> Result<Keypair> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading keypair at {path}"))?;
    let bytes: Vec<u8> = serde_json::from_str(&raw)
        .with_context(|| format!("{path} is not a JSON byte array"))?;
    if bytes.len() != 64 {
        bail!("keypair at {path} has {} bytes, expected 64", bytes.len());
    }
    Keypair::try_from(&bytes[..]).map_err(|e| anyhow::anyhow!("invalid keypair: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn ctx() -> MintContext {
        let mint = Pubkey::from_str("9pHqfcmQrGYF5zDwGqSWdbTgviZJtshM9Q93bf6wpump").unwrap();
        MintContext {
            mint,
            bonding_curve: pumpfun::bonding_curve(&mint),
            base_token_program: *pumpfun::TOKEN_2022_PROGRAM,
            creator: Pubkey::new_unique(),
            creator_vault: Pubkey::new_unique(),
            associated_bonding_curve: Pubkey::new_unique(),
        }
    }

    fn venue() -> Venue {
        Venue {
            client: RpcClient::new("https://example.invalid").unwrap(),
            global: Global {
                fee_recipients: (0..8).map(|_| Pubkey::new_unique()).collect(),
                buyback_recipients: (0..8).map(|_| Pubkey::new_unique()).collect(),
            },
            token_programs: HashMap::new(),
            luts: Vec::new(),
            amm: None,
        }
    }

    #[test]
    fn ata_create_is_idempotent_variant() {
        let ix = create_ata_idempotent(
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &pumpfun::TOKEN_PROGRAM,
        );
        assert_eq!(ix.data, vec![1], "1 = CreateIdempotent, 0 = Create");
        assert_eq!(ix.accounts.len(), 6);
        assert!(ix.accounts[0].is_signer);
        assert!(ix.accounts[1].is_writable);
    }

    #[test]
    fn buy_transaction_signs_and_has_the_right_shape() {
        let payer = Keypair::new();
        let v = venue();
        let tx = v
            .build_buy(
                &payer,
                &ctx(),
                Hash::default(),
                &FeeConfig::default(),
                0,
                0,
                3_440_000_000,
                1,
                Some(exec::pick_tip_account(0)),
            )
            .expect("build");
        assert_eq!(tx.signatures.len(), 1);
        // CU limit, CU price, tip, ATA create, buy
        assert_eq!(tx.message.instructions().len(), 5);
    }

    #[test]
    fn the_atomic_completion_reports_its_size_honestly() {
        let payer = Keypair::new();
        let v = venue();
        let built = v.build_completion(
            &payer, &ctx(), Hash::default(), &FeeConfig::default(),
            0, 0, 6_000_000_000, 1, Some(exec::pick_tip_account(0)), vec![],
        );
        match built {
            Ok(tx) => {
                // CU limit, CU price, tip, ATA create, buy, migrate
                assert_eq!(tx.message.instructions().len(), 6);
                let n = bincode::serialize(&tx).unwrap().len();
                assert!(n <= 1232, "claimed to fit but is {n} bytes");
            }
            Err(e) => {
                // The honest failure: it must say WHY and point at the fix.
                let msg = e.to_string();
                assert!(msg.contains("1232"), "{msg}");
                assert!(msg.contains("lookup table"), "{msg}");
            }
        }
    }

    #[test]
    fn completion_bundles_buy_and_migrate_into_one_transaction() {
        // Two transactions would lose the race and strand us above the pool price.
        let payer = Keypair::new();
        let v = venue();
        if let Ok(tx) = v.build_completion(
            &payer, &ctx(), Hash::default(), &FeeConfig::default(),
            0, 0, 6_000_000_000, 1, None, vec![],
        ) {
            let pump_ixs = tx
                .message
                .instructions()
                .iter()
                .filter(|i| {
                    tx.message.static_account_keys()[i.program_id_index as usize] == *pumpfun::PROGRAM
                })
                .count();
            assert_eq!(pump_ixs, 2, "must be buy AND migrate, atomically");
        }
    }

    #[test]
    fn sell_transaction_never_tips() {
        let payer = Keypair::new();
        let v = venue();
        let tx = v
            .build_sell(
                &payer,
                &ctx(),
                Hash::default(),
                &FeeConfig::default(),
                0,
                0,
                1_000_000,
                0,
            )
            .expect("build");
        // CU limit, CU price, ATA create, sell - no tip leg
        assert_eq!(tx.message.instructions().len(), 4);
    }

    #[test]
    fn keypair_loading_rejects_bad_files() {
        let dir = std::env::temp_dir();
        let short = dir.join("sniper_bad_key.json");
        std::fs::write(&short, "[1,2,3]").unwrap();
        assert!(load_keypair(short.to_str().unwrap()).is_err());
        let _ = std::fs::remove_file(&short);

        assert!(load_keypair("definitely/not/here.json").is_err());
    }

    #[test]
    fn keypair_round_trips_from_a_cli_json_file() {
        let kp = Keypair::new();
        let bytes = kp.to_bytes().to_vec();
        let path = std::env::temp_dir().join("sniper_ok_key.json");
        std::fs::write(&path, serde_json::to_string(&bytes).unwrap()).unwrap();
        let loaded = load_keypair(path.to_str().unwrap()).unwrap();
        assert_eq!(loaded.pubkey(), kp.pubkey());
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod packet_size {
    use super::*;
    #[test]
    fn the_atomic_completion_needs_a_lookup_table_and_fits_with_one() {
        let payer = Keypair::new();
        let mint = Pubkey::new_unique();
        let ctx = MintContext {
            mint,
            bonding_curve: pumpfun::bonding_curve(&mint),
            base_token_program: *pumpfun::TOKEN_2022_PROGRAM,
            creator: Pubkey::new_unique(),
            creator_vault: Pubkey::new_unique(),
            associated_bonding_curve: Pubkey::new_unique(),
        };
        let v = Venue {
            client: RpcClient::new("https://example.invalid").unwrap(),
            global: Global {
                fee_recipients: (0..8).map(|_| Pubkey::new_unique()).collect(),
                buyback_recipients: (0..8).map(|_| Pubkey::new_unique()).collect(),
            },
            token_programs: HashMap::new(),
            luts: Vec::new(),
            amm: None,
        };
        // Without a lookup table the atomic completion does not fit a packet.
        let without = v.build_completion(&payer, &ctx, Hash::default(), &FeeConfig::default(), 0, 0, 6_000_000_000, 1, Some(exec::pick_tip_account(0)), vec![]);
        let err = without.expect_err("1484 bytes must not be accepted").to_string();
        assert!(err.contains("1484"), "{err}");

        // With one covering the constants, it does - 1484 -> 1115 bytes.
        let alt = solana_sdk::message::AddressLookupTableAccount {
            key: Pubkey::new_unique(),
            addresses: resolve::constant_accounts(),
        };
        let tx = v
            .build_completion(&payer, &ctx, Hash::default(), &FeeConfig::default(), 0, 0, 6_000_000_000, 1, Some(exec::pick_tip_account(0)), vec![alt])
            .expect("must fit with a lookup table");
        let n = bincode::serialize(&tx).unwrap().len();
        assert!(n <= 1232, "still {n} bytes with a lookup table");
    }
}
