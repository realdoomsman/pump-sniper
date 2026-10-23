# sniper

A pump.fun bonding-curve **completion** sniper, in Rust.

The edge is not speed for its own sake. A pump.fun curve graduates at exactly
`85,005,359,056` lamports of real reserves, and graduation is what creates the
PumpSwap pool. If you are the buy that crosses that line, you own the migration and
the opening print. On mainnet that has been bought for as little as **0.0054 SOL**
— the curve was that far short, and paying the remainder claimed the whole thing.

Everything here was derived by reverse-engineering live mainnet activity: a wallet
that fired 52,000 transactions in 15 hours at a 0.82% land rate, its private BPF
guard program, and 79 transactions from the network-wide migration index. The
measurements, including the ones that contradict the reference wallet, are in
[`docs/RESEARCH.md`](docs/RESEARCH.md).

## Status

| Area | State |
| --- | --- |
| Curve math, graduation constant, fee model | Done, unit-tested against observed mainnet values |
| Live curve feed (`programSubscribe`) | Done, verified against mainnet |
| Trade-event decoding (who is buying) | Done, decoder validated on 60/60 real events |
| Trade-event feed (`logsSubscribe`) | Done — 60k frames / 3.5k events per minute observed |
| Smart-money reputation | Done, unit-tested, live-captured 47 wallets |
| Graduation model + online learning | Done, unit-tested, trains from live outcomes |
| Offline fitting (`train`) | Done, with a guard against degenerate datasets |
| Demand tracking and strategy decisions | Done, unit-tested + paper-run on live data |
| Risk gate and kill switch | Done, unit-tested |
| Transaction assembly, fee ladder, bundle encoding | Done, unit-tested |
| Account resolution from a mint | Done — verified 9/9 buys, 51/51 sells, 38/38 migrations |
| **Live order submission (buy + sell)** | **Wired**, behind the safety gates |
| **Atomic completion (buy + MigrateV2)** | **Wired** — 29/29 accounts resolved |
| Exit scheduling | Done — tranches fire on a timer, positions close |
| **PumpSwap `Sell`** | **Done** — 24/24 slots, verified 73/73; completions can exit |
| PumpSwap `Buy` (the opening print) | Not built — this is the remaining edge |
| Backtest replay | Done — replays through the live decision code |

`cargo test` — 202 tests, all passing.

Verified end to end against mainnet, not just in unit tests. A 45-second `scan`
tracked 196 live curves, the nearest 5.12 SOL from graduating. A 75-second paper
`run` produced two genuine completion signals — depth 73.77 SOL → 11.37 SOL order,
and depth 79.90 SOL → 5.17 SOL order — the same shape as the reference wallet's
real completions (73.65 SOL → 11.36 SOL).

Two bugs that only live data would have caught, both now covered by regression
tests:

* The `dataSize` subscription filter was set to 81 bytes. Real bonding curves are
  **151 or 115 bytes** — two live account versions. A wrong `dataSize` filter does
  not error, it just silently delivers nothing forever.
* Momentum entries re-fired on the same curve on every account update: 12 orders in
  1.5 seconds, four on one curve, hitting the rate limiter instead of trading. Now
  the demand gate covers momentum too, and each curve is acted on once.

### Live execution

Buys, sells and the **atomic completion** are wired. Every account is derived from
the mint alone, verified against **9/9 landed buys, 51/51 sells and 38/38
migrations**. The mainnet addresses are baked in as test fixtures, so a pump.fun
upgrade that moves a PDA breaks the test suite rather than your money.

A completion puts the completing buy and `MigrateV2` in **one transaction**. That is
not a nicety: it is what beats the keeper, which cannot react in under ~1.2 s, and
it is the only way to own the migration rather than lose it to the keeper.

That transaction is **1,484 bytes without a lookup table** and **1,115 with one**,
against a 1,232-byte limit — so `rpc.lookup_table` is required:

```bash
cargo run --release -- --mode live create-alt   # prints the address
```

The last unresolved account, slot 27, turned out to be PumpSwap's `boost_vault`
authority seeded by the pool. Every search seeded from `pump.so` failed because the
literal lives in the *PumpSwap* binary; it surfaced by decoding the migration's
inner `init_boost` CPI instead.

## Quick start

```bash
cargo test
cargo run --release -- explain     # config, derived constants, and learned state
cargo run --release -- scan        # curves closest to graduating, right now
cargo run --release -- collect     # record trajectories for offline fitting
cargo run --release -- train       # fit the model (refuses degenerate data)
cargo run --release -- run         # paper trade, learning as it goes
```

`explain` prints the effective config, the derived graduation line, and what each
fee tier would actually cost.

Watch the live feed and list the curves closest to graduating — no key needed:

```bash
cargo run --release -- scan --seconds 60 --limit 20
```

Paper trade:

```bash
cp config.example.toml config.toml
cargo run --release -- run
```

## How it decides

Two plays plus one filter.

**Completion.** When a curve's gap to graduation is inside `max_gap_lamports`, buy
exactly that gap. This is where the money is.

**Momentum.** A depth-banded ladder well below graduation, mirroring the five
4-SOL-wide bands found in the reference guard's payload.

**The demand gate — the part the reference wallet does not have.** It fires on
curve depth alone. That is precisely how it paid 15.85 SOL to open a pool nobody
wanted and lost 5.30 SOL in 24 seconds. Before committing, this bot requires real
inflow inside a trailing window, across several distinct updates. A curve one
lamport from graduation with nothing flowing into it is not an opportunity.

There is no candidate screening, and that is on purpose: the reference wallet bid
on 119 distinct mints in a 181-transaction sample, of which **113 never received a
single buy from anyone**. Selection by identity is wasted work. Subscribe to every
curve and decide on state.

## The one thing to know before running this

A completion graduates the curve, and pump.fun refuses sells on a graduated curve —
its own error string is *"The bonding curve has completed and liquidity migrated"*.
So completions exit on PumpSwap, and that path is **now built**: `Sell` resolved
24/24 and verified against 73/73 landed mainnet instances.

**There is no opening discount, and the number that said there was one was a
phantom.** This README asserted a 20.71% discount for most of its life. It came
from `pool_open_discount(boost)`, which paired whatever boost you passed with a
hardcoded 67.406 SOL pool reserve. Ask it about `boost = 0` and it described a pool
holding 67.4 SOL of real quote and no boost — a configuration that, across **267
migrations decoded from mainnet blocks, has never existed.**

The two "regimes" are not two kinds of pool. They are two different graduation
rules, and they separate perfectly:

| `is_mayhem_mode` | curve raised | pool opens with | discount | n |
| --- | --- | --- | --- | --- |
| clear | 85.007 SOL | 67.406 real + 17.585 boost | **0.0244%** | 190 |
| set | median **0.107 SOL** | median **0.116 SOL**, boost 0 | 98.53% | 76 |

A mayhem pool is not a discounted graduation — it is a dead coin that graduated
under a different rule into a pool holding a tenth of a SOL. Its "98.53% discount"
is unreachable: you cannot buy 2 SOL of a pool that contains 0.116. And a
completion always drives a curve to the classic 85 SOL line, so it always lands in
the first row, where there is nothing to capture.

Confirmed live, ninety seconds after `sniper watch-pools` first ran:

```
new pool: raised 0.1996 SOL, opens with 0.1996, boost 0.0000
```

Live completions still refuse to open unless PumpSwap's config loaded successfully,
so the bot cannot take a position it has no route out of.

## Sizing: not a premium, a position

`max_gap_lamports` is still the most important number in the config, but not for
the reason this file used to give.

Marked against the pool it actually opens, the completion buy is **flat to slightly
positive at every gap** — it is not a premium and there is none to minimise. Over
12,985 recorded curves:

```bash
cargo run --release -- backtest --sweep-gap
```

| max_gap | triggers | precision | per trigger |
| --- | --- | --- | --- |
| 12.0 SOL | 93 | 75% | +0.7865 |
| 6.0 SOL | 69 | 91% | +0.1091 |
| 2.0 SOL | 52 | 94% | −0.0002 |
| 0.5 SOL | 29 | 100% | −0.0026 |

Note also that **precision is not 100% below 6 SOL**, as the old table claimed. It
is 75/91/94%, reaching 100% only at 0.5 SOL and below.

So the gap trades **position size against demand quality**. And those positive
numbers at wide gaps are a *mark*, not a fill: selling 12 SOL of tokens into a pool
whose depth is fixed near 67 SOL moves the price against you, which the mark
ignores entirely.

## What the backtest says: read this before funding anything

`sniper backtest` replays recorded curves through the same `evaluate_snapshot` the
live loop calls.

**The momentum ladder loses money, so it is off by default** — 397 trades, 26.7%
hit rate, −108.6 SOL, about −7% per round trip. It is the ladder from the reference
wallet's own guard payload. The bands stay commented out in `config.example.toml`
because a measured negative result is worth keeping, not because they work.

**The completion is roughly flat, and that is the whole problem.** It is a ticket,
not a prize. All profit has to come from demand arriving *after* the pool opens —
and nothing in this repository had ever measured whether it does. The curve feed
goes blind the instant a coin graduates; 362,920 recorded curve updates contain
zero post-migration prints.

That is what `sniper watch-pools` is for:

```bash
cargo run --release -- watch-pools --out data/pools.jsonl --window 60
```

It records every fresh pool's quote reserve for its first minute — reading the SPL
token account directly rather than guessing at PumpSwap's once-observed `BuyEvent`
layout. The field to look at is `gross_inflow`: other people's buying, in lamports.
Until that file has real volume in it, **no sizing decision here is
evidence-based.**

## What it learns, and what it refuses to learn

Two stores under `data/`, both updated live and both persisted.

**`model.json` — P(this curve graduates).** Logistic regression over six features:
depth, inflow velocity, update count, age, smart-money score, unique buyers. Fitted
by SGD from realised outcomes, so it adapts without a training framework. Linear on
purpose — you can read the coefficients and tell whether it has learned something
sane, which matters more here than capacity.

The model is a **veto, never a licence**. It runs after the deterministic gates, so
a mis-fitted model can only make the bot trade less. There is a test for exactly
that: a model trained to predict 0.99 on a curve with no demand evidence still does
not get it traded.

**`smart.json` — wallet reputation.** Trade events carry the buyer's pubkey, so
every buy is attributed. A wallet's score is its **lift over the base rate**, shrunk
by a Beta prior: one lucky hit scores near zero, and a wallet performing at the base
rate contributes nothing. Unknown wallets score exactly 0 and never move a decision.

### The guards matter more than the learning

Both were added because the naive version failed on real data:

* **Censoring.** A curve still climbing when you stop watching has not failed. The
  first `train` run had 882 of 892 curves censored, leaving 38 rows that were *all*
  graduations; fitting them produced a +4.7 bias that scored everything near 1.0.
  `train` now refuses any dataset with no negatives, or an implausible class balance,
  and says why.
* **Online persistence.** A short live run sees a couple of graduations before any
  failure settles. The model is only written to disk once it has ≥50 outcomes with
  both classes present.

Neither guard makes the bot smarter. They stop it becoming confidently wrong, which
on this dataset is the more likely failure by far.

## Fees: lower than you would guess

The counterintuitive result from the competitor data. Across 79 sampled
transactions on the migration index:

| | median fee | median tip | max tip |
| --- | --- | --- | --- |
| Landed | 0.000055 SOL | **0** | 0.1381 SOL |
| Failed | 0.000005 SOL | 0.0002 SOL | **0.5618 SOL** |

Paying more correlated with *losing*. The wallets that reliably land migrations pay
essentially nothing, while the reference wallet burned up to 3.83 SOL on a single
Jito tip and 1.98 SOL on a single priority fee. Tier 0 here costs under 200,000
lamports and the ladder is for genuinely contested blocks. Do not raise it without
a reason you can point at.

## Revert protection without a guard program

The reference wallet deploys its own BPF program whose real job is to abort when
the opportunity is gone, so the tip transfer rolls back. It still pays base and
priority fee on every miss: ~11,140 lamports, 0.57 SOL across 50,718 misses.

Set `rpc.jito_block_engine` and submit as a bundle instead. A bundle that would
fail is dropped whole — nothing reaches the chain and the tip is never paid. Same
free-option property, no program to deploy. A guard program is only worth it if you
want to spray faster than bundles will accept.

## Safety

Live mode is refused unless **all** of these hold:

- `mode = "live"`
- `i_understand_this_spends_real_money = true`
- `keypair_path` is set
- `rpc.http` is not the public endpoint

On top of that, `risk.rs` enforces per-order size, total exposure, max concurrent
positions, a minimum fee reserve, an orders-per-minute cap, and a daily loss limit
that trips a one-way kill switch. Every check is a refusal, never a silent
downsize.

## What this is not

Not a money printer. The reference wallet made roughly +35 SOL over 15 hours on
measured trades, and one of the two round trips traced end-to-end lost 5.30 SOL.
This is a competitive, negative-sum race against operators who are better
capitalised and better connected than a fresh deployment will be — one of them
holds 1,047 SOL and lands 98% of what it sends. Read `docs/RESEARCH.md` before
putting real money behind it, including the open questions at the end.

## Layout

```
src/
  pumpfun.rs   curve state, bonding math, PDAs, instruction encoding
  events.rs    pump.fun TradeEvent decoding - who is buying
  feed.rs      programSubscribe curve feed + logsSubscribe trade feed
  tracker.rs   live curve registry + inflow/velocity history
  smart.rs     wallet reputation, shrunk lift over the base rate
  score.rs     online logistic model for P(graduate)
  train.rs     offline fitting, censoring-aware labelling
  strategy.rs  completion & momentum decisions, model veto, exit planning
  risk.rs      limits and the kill switch
  exec.rs      fee ladder, tip accounts, transaction assembly
  rpc.rs       JSON-RPC client + Jito bundle submission
  config.rs    config + the live-mode gate
docs/RESEARCH.md
```
