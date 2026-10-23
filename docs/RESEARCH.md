# Research notes

Everything the bot encodes was measured on mainnet, not taken from an IDL or a
blog post. This file records what was measured, how, and what is still guesswork,
so the constants in `src/` can be audited or re-derived later.

Method throughout: Solana JSON-RPC only — `getSignaturesForAddress`,
`getTransaction`, `getMultipleAccounts`, `getAccountInfo` — plus raw account and
instruction-data decoding.

---

## 1. The reference wallet

`niggerd597QYedtvjQDVHZTCCGyJrwHNm2i49dkm5zS` (the leading characters are a vanity
prefix ground into the keypair).

Window: 2026-08-29 22:32 → 2026-08-30 13:30 UTC, 15.0 hours.

| Metric | Value |
| --- | --- |
| Transactions | 52,000 |
| Landed | 427 (0.82%) |
| Reverted at ix4 with `0x6F` | 50,718 |
| Cost of all reverts | ~0.57 SOL (~11,140 lamports each) |
| Peak burst | 3,957 tx/min; 349 tx in one 400 ms slot |
| Distinct mints bid on | 119 in a 181-transaction sample |

Architecture: a private, single-tenant BPF program
`5kZmvKbaqNjSNEnQKEREEKu5r9JkaDRKrQMUvZagCMoz` (269,384-byte ELF, hand-rolled Rust,
no Anchor, **no hardcoded venue IDs** — every target program arrives as an account).
Upgrade authority `G6iAiWP7JTgtjvKFug25a4i28YTGF1HyQX1sP5TmUW8x`. All 25 sampled
transactions touching the program share one fee payer, so it is not a rented bot
service. Supporting infrastructure: a 77-entry lookup table
`7HyCbEs9usmw3g4zLs8XemB5y4bgSfr4ZNoxNrVnxQd1` (which carries
`jitodontfront111...`), four durable nonce accounts, a WSOL account pinned to
exactly 41.0000 SOL, and a profit-sweep wallet
`HQz2tzBi36sDiGzVH8d3Fd5Rx6Vdoc6vmijqyV3gpSB5`.

### Selection: there is none

The guard payload is **byte-identical across all 181 attempts examined**:

```
[disc 0x16][count 0x0a][10 x (u8 a, u8 b, u64 amount)][u64 tail]   = 110 bytes
```

* `b - a == 4` in all 1,810 records.
* Only five distinct `(a, b)` pairs: (7,11) (10,14) (12,16) (18,22) (28,32).
* Only four distinct amounts: 4.0, 4.3, 7.0, 10.0 SOL.
* The trailing `u64` is the only field that varies — 180 distinct values in
  40..9,971 across 181 attempts. That is a salt, which is what stops 3,957
  transactions a minute from colliding into duplicate signatures.

Nothing about the target is encoded. The wallet keeps a standing, revert-protected
bid against every live curve and lets the on-chain guard choose the moment.

**Unproven:** what `(a, b)` mean in their own units. They are consistent with
curve-depth bands and are used that way in `strategy.rs`, but confirming it needs
BPF disassembly. The *behaviour* below is measured and does not depend on it.

### What it actually buys

Reading the bonding curve of all 119 targeted mints: **113 still sit at exactly
30.000 virtual SOL / 0.000 real SOL / 793,100,000 tokens** — untouched curves that
never received a single buy from anyone. One had graduated.

Curve depth at the moment of every landed buy — it has **never** bought a fresh
curve:

| Curve depth before | Bot paid | Curve after |
| --- | --- | --- |
| 3.485 | 3.951 | 7.436 |
| 6.540 | 3.398 | 9.938 |
| 7.331 | 3.259 | 10.590 |
| 12.420 | 3.398 | 15.818 |
| 20.717 | 3.398 | 24.114 |
| 26.051 | 3.398 | 29.448 |
| 34.469 | 3.398 | 37.866 |
| **73.652** | **11.355** | **85.0073** |
| **74.212** | **10.795** | **85.0073** |
| **78.296** | **6.712** | **85.0073** |
| **83.144** | **1.863** | **85.0071** |
| **83.952** | **1.055** | **85.0071** |
| **85.002** | **0.0054** | **85.0071** |
| **85.002** | **0.0054** | **85.0071** |

Two regimes. Below ~35 SOL, a flat 3.3–4.0 SOL momentum entry. Above ~73 SOL it
buys **exactly the amount needed to hit the graduation line**, never overshooting.
Four of the seven completions were followed by `MigrateV2` **in the same slot**.

The last two rows are the whole thesis: the curve sat 0.0054 SOL short, and paying
that ~1 cent bought the exclusive right to create the PumpSwap pool and take the
opening print.

### Derived constant

`graduation_lamports()` is computed, not hardcoded from observation:

```
k          = 30e9 * 1.073e15
vt_final   = 1.073e15 - 793.1e12   = 279_900_000_000_000
vs_final   = k / vt_final          = 115_005_359_056
real_sol   = vs_final - 30e9       =  85_005_359_056 lamports
```

Add the curve account's rent and you land on the 85.0071–85.0073 SOL account
balance observed on all seven completions. `pumpfun::tests` asserts the exact value.

### Economics over the window

Measured across 304 of the 427 landed transactions (71%; the rest were lost to
public-RPC rate limits):

* Trading P&L **+35.07 SOL**, of which 42.00 SOL was swept to the profit wallet in
  10 sweeps (largest 16.91 SOL).
* Priority fees and tips inside winning transactions: **−7.38 SOL**.
* Jito tips 9.82 SOL, Astralane 1.14, Temporal/Nozomi 0.36 (sampled).
* All 50,718 reverts together: **−0.57 SOL**.

Two complete round trips, traced leg by leg: **+23.20 SOL in 22 seconds** and
**−5.30 SOL in 24 seconds**. The loser is the important one — it bought 15.85 SOL
of a freshly migrated pool and dribbled out evenly into no demand.

---

## 2. Instruction map

Recovered by decoding live transaction data, including inner instructions.

**pump.fun** `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P`

| Discriminator | Name | Layout | Accts |
| --- | --- | --- | --- |
| `38fc74089edfcd5f` | `BuyExactSolIn` | `sol_in: u64, min_tokens_out: u64` | 18 |
| `5df6823ce7e940b2` | `SellV2` | `tokens: u64, min_sol_out: u64` | 26 |
| `9beae792ec9ea21e` | `Migrate` | *(no args)* | 27 |
| `bbcb121fceedfe29` | `MigrateV2` | *(no args)* | 29 |

**PumpSwap** `pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA`

| Discriminator | Name | Layout | Accts |
| --- | --- | --- | --- |
| `66063d1201daebea` | `Buy` | `base_out: u64, max_quote_in: u64` | 26 |
| `33e685a4017f83ad` | `Sell` | `base_in: u64, min_quote_out: u64` | 24 |
| `c62e1552b4d9e870` | `BuyExactQuoteIn` | `quote_in: u64, min_base_out: u64, u8` | 26 |
| `e992d18ecf6840bc` | `CreatePool` | 60 bytes | 18 |

`BuyExactSolIn` account order (from a landed transaction) is reproduced in
`pumpfun::buy_exact_sol_in`. Fees: a 3.440000000 SOL gross order delivered
3.397531 SOL into the curve — 0.95% protocol (split across two recipients) plus
0.30% creator, so **1.25% on top**, which is `FEE_BPS`.

---

## 3. The competition

> **Superseded — see section 6.** The table below counts `err == null` as a win.
> `Migrate` is a silent no-op when the pool already exists, so this mixes real wins
> with no-ops and the specific numbers are wrong. The corrected analysis is in
> section 6; the qualitative conclusion (this is not won by outspending) survives.

Found via `39azUYFWPz3VHgKCf3VChUwbpURdCHRxjWVowf5jUJjg`, a System-owned account
touched by every migration. It carries ~1,000 attempts per 7 hours with ~309
landing — effectively a network-wide migration index.

Sampling 79 of those transactions:

| | n | median fee | median tip | max tip |
| --- | --- | --- | --- | --- |
| Landed | 44 | 0.000055 SOL | **0.000000 SOL** | 0.1381 SOL |
| Failed | 35 | 0.000005 SOL | 0.0002 SOL | **0.5618 SOL** |

**Paying more correlated with losing.** The wallets that reliably land migrations
pay essentially nothing.

| Wallet | Character |
| --- | --- |
| `9C4nRvhhVquCKATjDCx5FKvNS9PNgNqgyWy9AcoDjYv5` | 24 of 44 wins. 999/1000 lifetime success, 0.01 tx/s, 38 SOL. Never races — behaves like a keeper. |
| `Gygj9QQby4j2jryqyqBHvLP7ctv2SaANgh4sCb69BUpA` | 10 wins. 1,047 SOL, 11.4 tx/s, 98% success. A large, well-targeted operation. |
| `Hc91reaK14RvLRhspA7KnMx86AfeZfhCMpResnMTpx4n` | Another sniper's guard program. **0 of 1,000 recent transactions landed** — same spray-and-revert shape as `5kZmv`. |

So the population splits into keepers that quietly win, one big operator, and a
tail of spray-and-revert snipers that mostly burn fees. The reference wallet is in
the third group and still made money — from the completion trick, not from tips.

---

## 4. What this changed in the bot

1. **Fee defaults are low, not high.** Tier 0 costs under 200,000 lamports.
   Escalation exists for genuinely contested blocks. Overpaying is the observed
   losing strategy. (`exec::FeeConfig::default`)
2. **A demand gate before committing.** Curve depth alone is not enough; require
   real inflow inside a trailing window across several distinct updates.
   (`strategy::complete_decision`)
3. **Front-loaded exits.** The winning trade took most of its size out in the
   first two legs. (`strategy::plan_exit`)
4. **Bundles instead of a guard program.** A Jito bundle that would fail is dropped
   whole and the tip is never paid, which is strictly cheaper than the reference
   wallet's ~11,140 lamports per revert. Deploying a guard program only pays off if
   you want to spray faster than bundles allow. (`exec`, `rpc::send_bundle`)

---

## 5. Open questions

* Semantics of the `(a, b)` band pairs — needs BPF disassembly of `5kZmv`.
* Whether `9C4nRvhh` is privileged or merely fast. If migrations are effectively
  keeper-run, the completion buy still pays (you own the curve's last tokens) but
  owning the migration itself is much harder than the reference wallet's six wins
  suggest.
* Relay attribution beyond Jito's eight canonical tip accounts is inferred from
  address vanity prefixes and is **unverified** — see `OBSERVED_TIPS`.
* P&L covers 71% of landed transactions; failure costs are extrapolated from a
  sampled mean fee.

---

## 6. Second pass: what the deeper research changed

A five-way parallel investigation with adversarial verification on every finding.
Several of my earlier conclusions did not survive it.

### CORRECTION: "the winners pay nothing" was measuring the wrong thing

Section 3 above reported that landed migrations paid a median tip of zero while
failed ones paid more. That table is **contaminated and should not be used**.

`Migrate` / `MigrateV2` returns `Ok` and **does nothing** when the pool already
exists. Counting `err == null` as a win therefore counts no-ops as wins: 13 of 50
of `9C4nRvhh`'s landed migrations created no pool, and 7 of 18 of `Gygj9QQ`'s.

The correct definition of a win is an inner `CreatePool` in the logs. On 45
decoded **real** 85-SOL graduations:

| | share |
| --- | --- |
| `9C4nRvhh` | 40% |
| rotating burner wallets (~4 operators by fee fingerprint) | 60% |

Median total cost to win: **55,000–105,000 lamports (~$0.01)**. So the shape of
the original conclusion — that this is not won by outspending — survives; the
specific numbers in that table do not.

### The keeper is not privileged, and the play is contestable

`9C4nRvhh` does not appear anywhere in pump.fun's 1045-byte Global account (the
search method was validated: `39azUYFW` *does* appear, at offset 113). It is a
reactive keeper: plain `MigrateV2`, a fixed 55,000-lamport fee, **never a tip**,
firing a median of 3 slots (~1.2 s) after the last successful transaction on the
curve. Its balance declines monotonically — 38.767 → 38.301 SOL over 8.5 hours,
never credited, never holding a token. A pure cost centre.

`Gygj9QQ` is not a migration bot at all. It is the fee payer for a 52,483-SOL
treasury running an ~8 tx/s micro-scalper on a different AMM, and won **zero** of
the 45 real graduations.

**Why the play is contestable:** everyone who completes the curve themselves wins
the migration in the *same slot*, every time, because the completing buy and the
migrate are atomic. The keeper is structurally always second. That is exactly the
strategy this bot implements. Addressable market ≈ 300–370 real graduations/day.

One number to design against: the new pool opens at **67.406 SOL / 206,900,000
tokens**, mid 3.258e-7 SOL/token. Size the opening buy against *that*, not against
the 85 SOL curve figure.

### Account resolution: solved

Every slot of `BuyExactSolIn` (18) and `SellV2` (26) is now resolvable from a mint
address alone — recovered by pulling pump.fun's program binary, reading its Anchor
string table for account names and PDA seed literals, then re-deriving and matching
against **9/9 landed buys and 51/51 landed sells, 21 rules, zero mismatches**.
Implemented in `src/resolve.rs`, with the mainnet addresses as test fixtures.

Three rules that were not guessable:

* `bonding_curve_v2` = PDA(`["bonding-curve-v2", mint]`, pump) and `sharing_config`
  = PDA(`["sharing-config", mint]`, **fee program**) are placeholders that do not
  exist on chain. `getAccountInfo` returns null forever; never treat that as an
  error. `sharing_config` being seeded under the fee program is why pump-seeded
  searches never found it.
* `fee_recipient` and `buyback_fee_recipient` come from two independent arrays in
  the Global account (offsets 41, 162, 741) and are only membership-checked, so any
  member works.
* The token program is per mint — read `mint_account.owner`. The quote side is
  always legacy SPL Token; pump.fun's binary asserts it. The reference wallet's
  `IncorrectProgramId` was self-inflicted, from its own `CloseAccount` housekeeping,
  not a pump.fun account error.

### MigrateV2: 28 of 29 slots, and an honest blocker

Resolved by the same method:

| slot | account |
| --- | --- |
| 4 | bonding_curve |
| 5 / 6 | associated bonding curve, base / quote |
| 10 | pool = PDA(`["pool", u16(0), pool_authority, base_mint, quote_mint]`, PumpSwap) |
| 11 | pool_authority = PDA(`["pool-authority", mint]`, pump) |
| 12 / 13 | pool_authority ATAs, mint / WSOL |
| 15 | lp_mint = PDA(`["pool_lp_mint", pool]`, PumpSwap) |
| 16 | ATA(pool_authority, lp_mint, **Token-2022**) |
| 17 / 18 | pool ATAs, base / quote |
| 28 | ATA(slot 27, WSOL, Tokenkeg) |
| 0,1,8,9,14,19–26 | constants |

**Slot 27, solved:** `PDA(["boost_vault", pool], PumpSwap)` — PumpSwap's boost
vault authority. Confirmed **38/38**, as is slot 28 = `ATA(slot27, quote, Tokenkeg)`,
which receives the ~17.59 SOL migration remainder.

It resisted every earlier search for one reason: **the seed literal lives in the
PumpSwap binary, not `pump.so`**, so a cross product of ~900 literals extracted
from pump.fun's program could never contain it. It surfaced by decoding the
migration's *inner* instructions instead — MigrateV2 CPIs into PumpSwap's
`init_boost` (disc `8ce9215e845ac28f` = `sha256("global:init_boost")[..8]`), whose
14-account list carries the pool at index 0 and this authority at index 7, naming
the role and pinning the seed to the pool rather than to any creator.

Flags, from three *top-level* migrations rather than CPI unions: writable
`{1,4,5,6,7,10,11,12,13,15,16,17,18,28}`, signer `{7}`.

`MigrateV2` is now implemented, and so is the atomic play.

### The atomic completion needs a lookup table — it is not optional

Buy + migrate serialises to **1,484 bytes** against Solana's 1,232-byte limit. With
a table covering the 16 constant accounts it drops to **1,115 bytes**. That is why
the reference wallet carried a 77-entry lookup table, and why `rpc.lookup_table` is
required for the completion play. `sniper create-alt` builds one.

### Event layouts

The `Program data:` payload starts with the event discriminator directly — the
Anchor `e445a52e51cb9a1d` CPI marker prefixes only *inner-instruction* data, in
0/254 log lines. Our decoder accepts both, so it was already correct.

`TradeEvent` `bddb7fd34ee661ee` was independently confirmed on **71 transactions**,
every field cross-checked against token deltas, direction, curve lamports and the
curve invariant. Seven discriminators appear across the cached data, and an eighth
from a third program showed up when the census widened — so the enumeration is not
closed and the decoder must fall through gracefully on unknown discriminators. It
does.

---

## 7. The backtest, and what it says about the strategy

> **SUPERSEDED — see section 16.** The 20.71% opening discount this section relies on was computed from constants, not observed, and describes a pool configuration that has never existed on mainnet. The measured numbers are in section 16.

`sniper backtest` replays recorded trajectories through
`strategy::evaluate_snapshot` — the exact function the live loop calls, so the
result is about the real strategy rather than a reimplementation of it.

Over **4,230 replayed curves / 92,750 recorded updates**:

| | triggers | result |
| --- | --- | --- |
| Momentum ladder | 397 | **−108.6 SOL realised, 26.7% hit rate** |
| Completion | 12 | **−16.6 SOL marked**; 7 of 12 curves actually graduated |

### The momentum ladder loses money

This is the ladder copied from the reference wallet's guard payload (five 4-SOL
bands, 4.0–10.0 SOL orders). Replayed honestly it loses about 7% per round trip
across 397 trades. Buying at 7–32 SOL of depth and exiting ~20 seconds later is a
losing trade on this data: most curves stall, and the front-loaded exit sells into
nothing.

A methodology note that mattered: the first run reported −342.6 SOL at an 11.6% hit
rate. That was wrong. The recorded trajectory comes from a world where we did not
trade, so pricing our sells into it charges us for money our own buy had put into
the curve. Carrying our own position through the exit moves the result to −108.6
SOL / 26.7% — a 3× error, and the corrected number is still a clear loss. A flat
round trip now costs ~2.5% (the fee, twice) instead of an impossible ~20%.

### Why completion alone is a losing trade — the key economic fact

A pump.fun curve's marginal price at graduation is `115.005 SOL / 279.9M tokens`.
The pool it migrates into opens at `67.406 SOL / 206.9M tokens`.

**The pool opens 20.71% below the price the last curve buyer paid.** Measured
independently at 20.69% from the migration transactions themselves.

So completing a curve and *holding* through migration is structurally a ~20% loss
before fees. That is exactly what the backtest shows, and it is not a flaw in the
strategy — it is the strategy being only half-implemented. The completion play pays
because you also take the migration and buy the new pool **at that 20% discount**,
then sell into the retail flow that arrives afterwards. The completing buy and the
opening buy are one trade, not two.

Which puts the unresolved `MigrateV2` slot 27 in its proper place: it is not a
missing nice-to-have, it is the half of the trade that makes the other half
profitable. `backtest::pool_open_discount()` derives the number and a test asserts
it, so if pump.fun ever changes the migration ratio the suite fails loudly.

### What the backtest still cannot tell you

* No latency and no competition — every trigger fills. Live, most will not.
* Other traders do not react to our order.
* Completions are *marked*, never filled; the post-migration venue is not in the
  data at all.

---

## 8. A bug that would have stranded every completion

Found while reviewing the shipped code, before any live trade.

**A completion buy graduates the curve. pump.fun then refuses every sell against
it** — its own binary carries the string *"The bonding curve has completed and
liquidity migrated"*. But the exit scheduler routed every tranche to pump.fun
`SellV2` on the bonding curve, with no venue check anywhere.

So the completion play — the entire thesis — bought, migrated, and then failed all
six exit tranches forever, retrying every 500 ms. The position would have been
stranded and the fees burned indefinitely.

Fixes, all shipped:

* `positions::ExitVenue` now travels with each position. A completion is marked
  `PumpSwap` at the moment it is opened.
* The exit loop refuses a `PumpSwap` leg with a clear error instead of submitting a
  transaction that is guaranteed to fail and still pay.
* **Live completions are refused entirely** while the PumpSwap exit is unbuilt
  (`allow_unexitable_completions = false`). Taking a trade you cannot close is
  worse than not trading.

The real fix is PumpSwap `Buy`/`Sell` account resolution, which is the same job
already done for pump.fun and is in progress.

## 9. Momentum is switched off, on the evidence

> **SUPERSEDED — see section 16.** The 20.71% opening discount this section relies on was computed from constants, not observed, and describes a pool configuration that has never existed on mainnet. The measured numbers are in section 16.

`momentum_bands` now defaults to empty. The five bands copied from the reference
wallet's guard payload were replayed over 4,230 curves: **397 trades, 26.7% hit
rate, −108.6 SOL**, about −7% per round trip.

They are kept in the code and in `config.example.toml`, commented out, because a
measured negative result is worth preserving. `sniper backtest` reproduces it.

With momentum off, the default config takes only completions — 31 triggers over the
replayed data, 67.7% of which really did graduate, marked at **−42.96 SOL**. That
number is not a strategy failure: it is the 20.71% pool-open discount from section 7
applied 31 times. It is the precise measure of how much the missing opening-buy leg
is worth.

---

## 10. Sizing the completion: it is an option premium, not a purchase

> **SUPERSEDED — see section 16.** The 20.71% opening discount this section relies on was computed from constants, not observed, and describes a pool configuration that has never existed on mainnet. The measured numbers are in section 16.

The −42.96 SOL of marked completion losses in section 9 was not noise, and not
really a strategy failure either. It was a **sizing** error, and the fix is worth
more than anything else in this section.

Think about what a completion buy actually is. You pay the remaining gap to flip the
curve to `complete`. What you receive is two things: the curve's last tokens
(immediately worth 20.7% less, per section 7) and **the exclusive right to create
the pool and buy it at that discount**. The second thing is the whole point — and
its value does not depend on how large a gap you closed. The premium does, almost
exactly linearly.

So: minimise the premium subject to still getting opportunities. Sweeping
`max_gap_lamports` over 5,677 recorded curves (`sniper backtest --sweep-gap`):

| max_gap | triggers | precision | premium | per trigger |
| --- | --- | --- | --- | --- |
| 12.0 SOL | 36 | 72% | −50.48 | **−1.4021** |
| 6.0 SOL | 23 | 100% | −16.49 | −0.7172 |
| 4.0 SOL | 22 | 100% | −10.95 | −0.4976 |
| **2.0 SOL** | **19** | **100%** | **−4.85** | **−0.2553** |
| 1.0 SOL | 14 | 100% | −1.56 | −0.1117 |
| 0.5 SOL | 11 | 100% | −0.69 | −0.0626 |
| 0.02 SOL | 1 | 100% | −0.003 | −0.0032 |

Two effects, both favouring a tight gap:

* **Cost per trade falls ~440×** across the range.
* **Precision rises to 100%.** At a 12 SOL gap only 72% of triggered curves actually
  graduated; at 6 SOL and below, every single one did. A curve already near the line
  almost always reaches it — a wide gap is really a bet that a distant curve will
  climb, which is a different and much worse trade.

The only cost is opportunity count, 36 → 1.

The default is now **2.0 SOL**: half the opportunities of the widest setting, at
100% precision, for a fifth of the premium. It should be re-tuned once PumpSwap
lands and the opening-buy edge is directly measurable rather than assumed.

Worth noting the reference wallet got this wrong in both directions. It closed gaps
up to **11.36 SOL** — squarely in the bad region — but its cheapest completion was
**0.0054 SOL**, which is close to optimal. It had no way to tell the two apart.

---

## 11. PumpSwap: the venue the profit actually lives on

Resolving `Sell` (disc `33e685a4017f83ad`, 24 accounts) unblocks the stranded-exit
bug from section 8. Every slot re-derived and matched against **73/73** landed
mainnet instances. Two rules that would not have been guessed:

* `coin_creator_vault_authority` = `PDA(["creator_vault", coin_creator], PumpSwap)`
  — **underscore**, under PumpSwap. pump.fun's own equivalent is `"creator-vault"`
  with a **hyphen**, under pump.fun. Two different accounts that look alike.
* `pool_v2` = `PDA(["pool-v2", mint], PumpSwap)` — seeded by the **mint**, not the
  pool, and never allocated on chain. Another required placeholder.

Also measured: `user_base_token_account` is **not** constrained to be the canonical
ATA. 0/73 samples were, and all landed; the program only checks owner and mint.

The `coin_creator` these derive from sits at pool bytes `211..243` and **equals the
bonding curve's `creator`** (verified 12/12). So every PumpSwap account resolves
from the curve we already decode, before the pool exists — no extra round trip on
the hot path.

### MigrateV2 slot 27, independently confirmed

A separate three-method investigation reached the same answer as section 6 by
disassembling both binaries: slots 27/28 are `remaining_accounts` (pump.fun's Anchor
struct declares only 27), forwarded to PumpSwap's undocumented `InitBoost`. The
~17.59 SOL landing there is a **boost / buy-and-burn reserve, not a creator fee** —
a correction to what section 6 implied. Confirmed 35/35 against our 38/38.

## 12. Two more bugs of the same family

Both found by running the thing rather than reading it.

**Paper mode priced every PumpSwap exit at zero.** A migrated curve does not merely
carry `complete = true` — pump.fun **zeroes every reserve**. Verified in the
collected feed: completed curves read `virt_sol=0, virt_tok=0, real_sol=0,
real_tok=0`. Quoting `sol_out` against that returns 0, so every completion exit was
reported as a 100% loss.

Pricing now dispatches on `ExitVenue`, and `positions::quote_exit_leg` makes it
unit-testable with the real drained shape as the fixture.

**A test that asserted the wrong thing.** My first version of that regression test
built its "graduated" curve by applying a buy and setting the flag — which leaves
the reserves populated and quotes 0.404 SOL, *above* the pool's 0.326. The test
failed, and it was right to: the fixture was fiction. Modelling the actual zeroed
account made it pass for the real reason.

The recurring shape across all four bugs in this file: **a wrong assumption that
fails silently.** A `dataSize` filter that matches nothing, a subscription error
that is swallowed, a censored label counted as a failure, a dead curve quoted at
zero. None of them error; they all just quietly produce a plausible wrong number.

---

## 13. The correction that matters most: there is usually no opening discount

> **SUPERSEDED — see section 16.** The 20.71% opening discount this section relies on was computed from constants, not observed, and describes a pool configuration that has never existed on mainnet. The measured numbers are in section 16.

Sections 7, 9 and 10 all rest on one number — the pool opening 20.71% below the
final curve price. **That number was wrong for most pools**, and the reason is a
field we had not found.

PumpSwap's Pool account carries `boost_quote` at offset 245: **virtual quote
liquidity** held in the struct rather than in the token account, and the constant
product is evaluated against `quote + boost`. Ignoring it reproduces **0 of 79**
observed mainnet fills; including it reproduces **79 of 79**.

Once it is included, the opening discount is **bimodal**, measured over 38 live
migrated pools:

| pool | `boost_quote` | opening discount | share |
| --- | --- | --- | --- |
| `is_mayhem_mode` (byte 243) | 0 SOL | **20.71%** | 15/38 |
| ordinary | 17.5845 SOL | **0.02%** | 23/38 |

The 17.5845 SOL is almost exactly the ~17.59 SOL the migration sends to the boost
vault. It is not taken away — on an ordinary pool it is **credited straight back as
virtual liquidity**, so the pool opens at essentially the same price the curve
closed at.

**Consequences, stated plainly:**

* On roughly 60% of pools there is **no opening discount to capture**. The
  completion play has no edge there at all — you pay the premium and buy the pool
  at fair value.
* The edge is real but confined to **mayhem-mode pools**, about 40% of migrations.
* Everything earlier in this document that treats 20.71% as universal is wrong.
  `backtest::pool_open_discount` now takes the boost as an argument and the test
  asserts both regimes.

Whether `is_mayhem_mode` can be predicted *before* the pool exists is unresolved,
and it is now the question the strategy lives or dies on. If it cannot, the play is
a coin flip between a 20.7% edge and none.

## 14. Ship-blockers found in code written this session

Adversarial verification of the PumpSwap work caught three, all in code that had
already been committed and tested:

1. **`coin_creator` is mutable.** It disagreed with the `CreatePool` argument on 3
   of 40 pools, and one pool's live value differs today from what its own sells
   used. Deriving it from the bonding curve's `creator` — which is right most of
   the time — silently yields the wrong `coin_creator_vault_authority` and the
   transaction fails on chain. It is now read live from the pool on every exit.
2. **Cashback pools take a 26-account Sell, not 24** (4 of 40 pools, byte 244).
   Now **resolved and implemented** — see section 15.
3. **The boost term above**, which made every price quote wrong.

Two things that survived: `pool_v2 = PDA(["pool-v2", mint], PumpSwap)` reproduced
**73/73** under my own derivation even though the verifying agent could not recover
the seed, and the `Sell` slot table held 24/24 on independent re-derivation.

---

## 15. Cashback pools: resolved, not refused

The 26-account `Sell` variant was initially handled by refusing those pools. That
was the safe call at the time but the wrong end state, and a challenge to it turned
out to be well-placed.

First, the shape had to be pinned. The two extra accounts are **inserted, not
appended** — `buyback_fee_recipient` moves from index 22 to 24, and slot 21 stops
being `pool_v2`. So a 24-account Sell sent to a cashback pool binds the wrong
account from index 21 onward; the forms are genuinely different and cannot be
substituted.

Separation across the cache is perfect and no pool ever mixed the two:

| pools | flag byte 244 | Sell accounts |
| --- | --- | --- |
| 10 | 0 | 24, in 73/73 sells |
| 1 | 1 | 26, in 6/6 sells |

Then the three divergent slots resolved cleanly:

```
21  ATA(user_volume_accumulator, WSOL, Tokenkeg)
22  PDA(["user_volume_accumulator", user], PumpSwap)
23  pool_v2                        <- was slot 21 in the 24-account form
24  buyback_fee_recipient          <- was slot 22
25  ATA(buyback_fee_recipient, WSOL, Tokenkeg)
```

Which makes sense on inspection: a **volume rebate has to know your volume**, so the
cashback form carries the user's volume accumulator and nowhere else does. The seed
literal `user_volume_accumulator` also appears in the PumpSwap binary's string table
adjacent to `pool-v2`, corroborating it independently of the address match.

Note this is PumpSwap's accumulator, seeded under PumpSwap — pump.fun has an
identically-named account under its own program, and they are different addresses.

**Confidence.** The layout is confirmed on 6/6 cashback sells, but all six share one
user and one mint, so the *shape* is solid while the *seeds* rest on one
(user, mint) pair plus the binary corroboration. Tests assert the insertion point
explicitly, so a wrong seed would surface as a failing test rather than a silent
mis-encode. `Pool::sell_account_count` now drives the form, and `PoolContext` carries
`is_cashback` read live from the pool.

---

## 16. The phantom discount, and what replaced it

This supersedes sections 7, 9, 10 and 13 wherever they touch the pool opening price.

### The bug

`backtest::pool_open_discount(boost_quote)` took one argument and paired it
internally with two constants: `POOL_OPEN_QUOTE_LAMPORTS = 67.406 SOL` and
`POOL_OPEN_BASE_TOKENS = 206.9M`. Calling it with `boost = 0` therefore asked for
the discount of a pool holding **67.4 SOL of real quote and no boost**, and it
answered **20.71%** without hesitation.

Across **267 migrations decoded from mainnet blocks, that pool has never existed.**

The function could not fail. Its inputs were constants, its arithmetic was valid,
and a test asserted the output. It is the same shape as every other bug in section
12: a wrong assumption that produces a plausible number instead of an error.

Worse, `mark_at_pool_open(tokens)` defaulted to `boost = 0` and was used to mark
**every completion in every backtest run**. Its doc comment called this "the
conservative mark". It was not conservative; it was fictional, and it understated
every completion by about 20.7%.

### The measurement

The two "regimes" were never two kinds of pool. They are two different graduation
rules, and they separate perfectly — both off-diagonal cells are empty:

| `is_mayhem_mode` (byte 243) | curve raised | pool opens with | discount | n |
| --- | --- | --- | --- | --- |
| clear | 85.007 SOL (classic) | 67.4059 real + 17.5845 boost | **0.0244%** | 190 |
| set | median **0.107 SOL** | median **0.116 SOL**, boost 0 | 98.53% | 76 |

A mayhem pool is not a discounted graduation. It is a dead coin that graduated
under a different rule into a pool holding about a tenth of a SOL. Its enormous
"discount" is unreachable: you cannot buy 2 SOL of a pool that contains 0.116.

Confirmed live within ninety seconds of `sniper watch-pools` first running:

```
new pool: raised 0.1996 SOL, opens with 0.1996, boost 0.0000
```

### The consequence: the completion buy is not a premium

A completion always drives a curve to the classic 85.007 SOL line, so it always
produces the first row — where there is no discount to capture and none to pay.
Re-running the sweep with completions marked against the pool they actually open,
over **12,985 recorded curves**:

| max_gap | triggers | precision | per trigger |
| --- | --- | --- | --- |
| 12.0 SOL | 93 | 75% | +0.7865 |
| 6.0 SOL | 69 | 91% | +0.1091 |
| 4.0 SOL | 62 | 92% | +0.0316 |
| 2.0 SOL | 52 | 94% | −0.0002 |
| 1.0 SOL | 39 | 95% | −0.0033 |
| 0.5 SOL | 29 | 100% | −0.0026 |
| 0.2 SOL | 18 | 100% | −0.0013 |

Two things change. The old table's premium column (−1.4021 down to −0.0626) was
entirely an artifact of the fictional mark. And **precision is not 100% below 6
SOL** — it is 75/91/92/94/95%, reaching 100% only at 0.5 SOL and below.

So `max_gap` no longer trades premium against opportunity. It trades **position
size against demand quality**, which is a different and harder question.

### What has NOT been established

The positive numbers at wide gaps are a **mark, not a fill**. `mark_completion`
values tokens at the pool's opening mid; selling them moves that price, and a
12 SOL completion is large against a 67 SOL pool. `backtest::tests::
the_mark_overstates_what_a_completion_could_actually_realise` pins this down.

Which leaves the one question everything now depends on:

> **How much quote does a fresh pool actually take in, in its first minute?**

A completion is roughly flat on its own tokens, so all profit has to come from
demand arriving *after* the pool opens. Nothing in this repository has ever
measured that — the curve feed goes blind at migration, and 362,920 recorded curve
updates contain zero post-migration prints.

`src/poolwatch.rs` and `sniper watch-pools` exist to close that gap. They watch the
pool's quote token account directly — an ordinary SPL token account with `amount`
at offset 64 — rather than decoding PumpSwap's `BuyEvent`, which has been observed
exactly once and whose layout is unverified. Guessing it would have reintroduced
precisely the failure this section documents.

Until that data exists, no sizing decision here is evidence-based.

---

## 17. The PumpSwap Buy leg

Recovered twice, independently, and the two passes agreed on all 26 slots including
writable and signer flags.

* Pass A sampled recent PumpSwap program traffic.
* Pass B went through the reference wallet's own pool buys, the program's on-chain
  Anchor IDL, and its ELF string table.

`Buy` = `66063d1201daebea` = `sha256("global:buy")[..8]`, confirmed on landed
transactions. Arguments are `(base_amount_out: u64, max_quote_amount_in: u64)` —
**exact-out**, with the second argument a slippage cap rather than the amount
spent. Some senders pass `u64::MAX` for it.

**The one thing that is not guessable from `Sell`:** slots 0..=18 are byte-identical
to `sell_accounts`, and then Buy inserts two accounts that Sell does not carry at
all —

| slot | account | |
| --- | --- | --- |
| 19 | `global_volume_accumulator` | read-only, `PDA(["global_volume_accumulator"], PumpSwap)` |
| 20 | `user_volume_accumulator` | **writable** |

— and everything from Sell's index 19 onward shifts by two. On `Sell` these appear
only for cashback pools. On `Buy` they are **mandatory even for ordinary pools**:
19/19 and 20/20 non-cashback buys carried both, and the on-chain IDL declares them
as named accounts. Deriving the list by analogy from `Sell` binds `fee_config` at
slot 19 and the transaction fails on chain.

The cashback variant inserts **one** account (`ATA(uva, WSOL, Tokenkeg)` at index
23), not two as cashback `Sell` does — because the accumulator itself is already a
named account at slot 20.

Account counts: **26** ordinary, **27** cashback. A 23-account form exists in 2025
traffic and must not be emitted.

**Payload length**, censused over 114 landed buys because the two passes disagreed:
25 bytes with a trailing `0x01` (87), 24 with the byte omitted (22), 26 with `0x0101`
(4), 25 with `0x00` (1). All four land; we send the modal 25. The byte is
`track_volume`, an IDL `OptionBool`, which is *not* a Rust `Option` — it is one raw
byte. `0x00` must not be sent: its single observation came from a wallet whose
accumulator already existed, so whether it skips the 1,844,400-lamport rent is
unproven.

Three things a reader of the account table must **not** infer:

* **Buy does not create your base token account.** Of 17 sampled buys whose slot-5
  account started empty, 16 were created by a separate top-level ATA instruction.
  Prepend one. Buy *does* create the `user_volume_accumulator` at slot 20.
* **The base token program is per mint.** Two `*pump` mints in one day's traffic are
  owned by classic Tokenkeg, not Token-2022. Read `mint_account.owner`; hardcoding
  it derives the wrong ATA for slots 5 and 7.
* **Writable flags read out of `getTransaction` are message-level**, so they are an
  upper bound rather than a per-instruction minimum. The set
  `{0,1,5,6,7,8,10,17,20,25}` is modal, not proven minimal.

---

## 18. Ship-blockers found and fixed this session

Four, all of which would have failed quietly rather than loudly.

1. **The completion could never land.** `cu_limit` was 260,000 for every play. A
   landed `MigrateV2` *on its own* was observed consuming **354,648 CU**, and the
   wallet that sent it requested 714,286. Our completion does that plus a buy. Now
   split into `cu_limit` (120,000) and `cu_limit_completion` (600,000), with a
   config validation that refuses anything below 450,000 and a regression test. The
   headroom costs about 0.000017 SOL at the working fee tier.

2. **Nothing ever checked whether a transaction landed.** `submit()` returns the
   node's echo of the signature, which proves the bytes parsed and nothing more —
   and with `skip_preflight` and `maxRetries: 0`, most submissions never land. The
   loop opened a position and scheduled sells regardless. Now `rpc::confirm` polls
   `getSignatureStatuses` and a completion that has not landed inside 5 seconds is
   written off and its reservation released.

3. **A failed submission still opened a position.** The `Err` arm logged and fell
   through, scheduling sells for tokens that were never bought while holding the
   risk reservation. Same for an unknown mint, which `unwrap_or_default()` turned
   into the all-zeros pubkey — a valid-looking key that resolves to nothing, so
   every exit against it would fail forever.

4. **Live P&L was a restatement of our own price model.** `proceeds` came from
   `quote_exit_leg` in *both* modes, so no slippage, partial fill or fee could ever
   appear in a reported number. Live now reads the realised balance deltas back off
   chain via `rpc::realised`, and says so loudly when it cannot.
