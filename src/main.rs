//! pump.fun bonding-curve completion sniper.
//!
//! Defaults to paper mode. Live mode requires an explicit acknowledgement in the
//! config file, a keypair, and a non-public RPC endpoint - see `config.rs`.

use anyhow::{Context, Result};
use sniper::{backtest, config, feed, live, positions, pumpfun, resolve, risk, rpc, score, smart::SmartMoney, sol, strategy, tracker, train};
use clap::{Parser, Subcommand};
use config::{Config, Mode};
use risk::{RiskGate, Refusal};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use strategy::{Decision, Skip};
use tokio::sync::mpsc;
use tracker::Tracker;
use solana_sdk::signer::Signer;

#[derive(Parser)]
#[command(name = "sniper", about = "pump.fun curve-completion sniper")]
struct Cli {
    /// Path to the config file.
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,

    /// Override the configured mode.
    #[arg(long)]
    mode: Option<Mode>,

    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Stream curves and trade (or paper-trade) them.
    Run,
    /// Print the effective configuration and the derived constants, then exit.
    Explain,
    /// Record every curve update to a JSONL file for offline model fitting.
    Collect {
        #[arg(long, default_value = "curves.jsonl")]
        out: PathBuf,
        /// Stop after this many seconds. 0 runs until interrupted.
        #[arg(long, default_value_t = 0)]
        seconds: u64,
    },
    /// Fit the graduation model from collected trajectories.
    Train {
        #[arg(long, default_value = "data/curves.jsonl")]
        data: PathBuf,
        #[arg(long, default_value = "data/model.json")]
        out: PathBuf,
        #[arg(long, default_value_t = 30)]
        epochs: usize,
        /// Replay each positive this many times to offset the low base rate.
        #[arg(long, default_value_t = 8)]
        pos_weight: usize,
        /// Silence (seconds) after which a curve counts as dead.
        #[arg(long, default_value_t = 600)]
        settle_after: u64,
        /// Curves updated within this many seconds of the end are censored.
        #[arg(long, default_value_t = 900)]
        censor_tail: u64,
    },
    /// Replay collected trajectories through the live decision code.
    Backtest {
        #[arg(long, default_value = "data/curves.jsonl")]
        data: PathBuf,
        /// Apply the trained model as a veto, if one exists.
        #[arg(long)]
        use_model: bool,
        /// Sweep max_gap_lamports and print the cost/precision tradeoff.
        #[arg(long)]
        sweep_gap: bool,
        #[arg(long, default_value_t = 20)]
        show: usize,
    },
    /// Create the address lookup table the atomic completion play requires.
    ///
    /// Live-mode only: it signs and submits. The buy+migrate transaction is 1,484
    /// bytes without a table and 1,115 with one.
    CreateAlt,
    /// Generate a new signing keypair for live mode.
    ///
    /// Writes the Solana CLI JSON format (a 64-byte array), so the same file works
    /// with `solana` tooling. It REFUSES to overwrite an existing file: doing so
    /// would destroy the only copy of a key that may be holding funds.
    Keygen {
        #[arg(long, default_value = "wallet.json")]
        out: PathBuf,
    },
    /// Record what happens to fresh PumpSwap pools right after they are created.
    ///
    /// This is the measurement the strategy is missing. The curve feed goes blind
    /// the instant a coin graduates, which is precisely when the payoff is decided.
    /// Every profit number in this crate depends on demand arriving after the pool
    /// opens, and nothing here has ever observed whether it does.
    ///
    /// Read-only: it never signs anything. Run it for a few hours, then look at
    /// `gross_inflow` - that is other people's buying, in lamports.
    WatchPools {
        #[arg(long, default_value = "data/pools.jsonl")]
        out: PathBuf,
        /// Seconds to watch each pool after it is created.
        #[arg(long, default_value_t = 60)]
        window: u64,
    },
    /// Show the curves currently closest to graduation, then exit.
    Scan {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, default_value_t = 30)]
        seconds: u64,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "sniper=info".into()),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let mut cfg = if cli.config.exists() {
        Config::load(&cli.config)?
    } else {
        tracing::warn!(
            "no config at {} - using built-in defaults (paper mode)",
            cli.config.display()
        );
        Config::default()
    };
    if let Some(m) = cli.mode {
        cfg.mode = m;
    }
    cfg.validate()?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;

    match cli.cmd {
        Command::Explain => {
            explain(&cfg);
            Ok(())
        }
        Command::Scan { limit, seconds } => rt.block_on(scan(&cfg, limit, seconds)),
        Command::Collect { out, seconds } => rt.block_on(collect(&cfg, &out, seconds)),
        Command::Train { data, out, epochs, pos_weight, settle_after, censor_tail } => {
            train_cmd(&cfg, &data, &out, epochs, pos_weight, settle_after, censor_tail)
        }
        Command::Backtest { data, use_model, sweep_gap, show } => {
            backtest_cmd(&cfg, &data, use_model, sweep_gap, show)
        }
        Command::Keygen { out } => keygen(&out),
        Command::WatchPools { out, window } => rt.block_on(watch_pools(&cfg, &out, window)),
        Command::CreateAlt => rt.block_on(create_alt(&cfg)),
        Command::Run => rt.block_on(run(cfg)),
    }
}

fn explain(cfg: &Config) {
    let g = pumpfun::graduation_lamports();
    println!("mode                : {:?}", cfg.mode);
    println!("rpc.http            : {}", cfg.rpc.http);
    println!("rpc.ws              : {}", cfg.rpc.ws);
    println!(
        "jito block engine   : {}",
        cfg.rpc.jito_block_engine.as_deref().unwrap_or("(none)")
    );
    println!();
    println!("graduation line     : {} lamports ({:.6} SOL)", g, sol(g));
    println!(
        "max gap to close    : {:.4} SOL",
        sol(cfg.strategy.max_gap_lamports)
    );
    println!(
        "max position        : {:.4} SOL",
        sol(cfg.strategy.max_position_lamports)
    );
    println!(
        "demand gate         : >= {:.4} SOL inflow over {}s, >= {} updates",
        sol(cfg.strategy.min_inflow_lamports.unsigned_abs()),
        cfg.strategy.demand_window.as_secs(),
        cfg.strategy.min_updates
    );
    println!();
    println!("fee ladder:");
    for t in 0..cfg.fees.tiers() {
        println!(
            "  tier {t}: priority {:.6} SOL + tip {:.6} SOL = {:.6} SOL per attempt",
            sol(cfg.fees.priority_fee(t, cfg.fees.cu_limit_completion)),
            sol(cfg.fees.tip(t)),
            sol(cfg.fees.attempt_cost(t, cfg.fees.cu_limit_completion))
        );
    }
    println!();
    println!("learned state:");
    match score::Model::load(&cfg.model_path()) {
        Ok(m) => {
            println!(
                "  model           : {} outcomes ({} graduated){}",
                m.trained_on,
                m.positives,
                if m.is_fit_to_persist() { "" } else { "  [NOT yet trustworthy]" }
            );
            if let Some(br) = m.base_rate() {
                println!("  observed base   : {:.2}% of curves graduate", br * 100.0);
            }
            for (name, w) in m.ranked() {
                println!("    {name:<14} {w:>9.4}");
            }
            println!("    {:<14} {:>9.4}", "bias", m.bias);
        }
        Err(_) => println!(
            "  model           : none at {} - priors only",
            cfg.model_path().display()
        ),
    }
    match SmartMoney::load(&cfg.smart_path()) {
        Ok(s) => {
            println!("  wallets known   : {}", s.known_wallets());
            match s.base_rate() {
                Some(b) => println!("  smart base rate : {:.2}%", b * 100.0),
                None => println!("  smart base rate : not enough settled curves yet"),
            }
            let top = s.top(5, 5);
            if top.is_empty() {
                println!("  top wallets     : none with enough evidence yet");
            } else {
                println!("  top wallets by lift over base rate:");
                for (w, st, lift) in top {
                    println!(
                        "    {w}  {}/{} graduated  lift {:+.3}",
                        st.graduated, st.settled, lift
                    );
                }
            }
        }
        Err(_) => println!("  wallets known   : 0 (no reputation store yet)"),
    }
    println!();
    println!("risk:");
    println!("  max order         : {:.4} SOL", sol(cfg.risk.max_order_lamports));
    println!("  max exposure      : {:.4} SOL", sol(cfg.risk.max_exposure_lamports));
    println!(
        "  daily loss limit  : {:.4} SOL",
        sol(cfg.risk.daily_loss_limit_lamports)
    );
    println!("  max open          : {}", cfg.risk.max_open_positions);
}

/// Fit the model offline and write it out.
fn train_cmd(
    cfg: &Config,
    data: &std::path::Path,
    out: &std::path::Path,
    epochs: usize,
    pos_weight: usize,
    settle_after: u64,
    censor_tail: u64,
) -> Result<()> {
    let trajs = train::load(data)?;
    let policy = train::LabelPolicy {
        settle_after: Duration::from_secs(settle_after),
        censor_tail: Duration::from_secs(censor_tail),
        ..train::LabelPolicy::default()
    };
    let (rows, rep) = train::build_rows(&trajs, &policy, cfg.strategy.demand_window);

    println!("curves          : {}", rep.curves);
    println!(
        "  graduated     : {}  dead: {}  censored (excluded): {}",
        rep.graduated, rep.dead, rep.censored
    );
    println!("training rows   : {} ({} positive)", rep.rows, rep.positive_rows);
    println!();
    println!("graduation rate by peak depth reached:");
    println!("{:>10} {:>10} {:>8} {:>10} {:>9}", "from SOL", "to SOL", "curves", "graduated", "rate");
    for (lo, hi, seen, grad) in &rep.depth_buckets {
        if *seen == 0 {
            continue;
        }
        let hi_s = if *hi == u64::MAX { "inf".to_string() } else { format!("{:.1}", sol(*hi)) };
        println!(
            "{:>10.1} {:>10} {:>8} {:>10} {:>8.1}%",
            sol(*lo), hi_s, seen, grad,
            100.0 * *grad as f64 / *seen as f64
        );
    }

    if let Err(why) = train::usable(&rep) {
        anyhow::bail!("refusing to fit: {why}");
    }

    let (model, before, after) = train::fit(&rows, epochs, pos_weight);
    println!();
    println!("log loss        : {before:.4} -> {after:.4}");
    println!("fitted weights:");
    for (name, w) in model.ranked() {
        println!("  {name:<14} {w:>9.4}");
    }
    println!("  {:<14} {:>9.4}", "bias", model.bias);
    model.save(out)?;
    println!();
    println!("wrote {}", out.display());
    Ok(())
}

/// Replay recorded trajectories and report what the strategy would have done.
fn backtest_cmd(
    cfg: &Config,
    data: &std::path::Path,
    use_model: bool,
    sweep_gap: bool,
    show: usize,
) -> Result<()> {
    let trajs = train::load(data)?;
    if sweep_gap {
        let gaps = [
            12_000_000_000u64, 6_000_000_000, 4_000_000_000, 2_000_000_000,
            1_000_000_000, 500_000_000, 200_000_000, 100_000_000, 20_000_000,
        ];
        let pts = backtest::sweep_gap(&trajs, &cfg.strategy, &cfg.exit, &gaps, cfg.fees.attempt_cost(0, cfg.fees.cu_limit_completion));
        println!("Completion gap sweep over {} curves.
", trajs.len());
        println!("The completion buy is NOT a premium. Marked against the pool it");
        println!("actually opens - an ordinary one, boost 17.5845 SOL - it comes out");
        println!("flat to slightly positive at every gap. The gap therefore trades");
        println!("position size against demand quality, not against a premium.");
        println!("The mark is an upper bound: selling this size moves the pool price.");
        println!();
        println!("{:>12} {:>9} {:>10} {:>11} {:>14}", "max_gap SOL", "triggers", "precision", "premium", "per trigger");
        for p in &pts {
            println!(
                "{:>12.4} {:>9} {:>9.0}% {:>11.4} {:>14.4}",
                sol(p.max_gap), p.triggers, p.precision() * 100.0,
                p.marked_pnl as f64 / 1e9, p.cost_per_trigger() / 1e9
            );
        }
        println!();
        println!("Tighter is more ACCURATE - a curve already close to the line almost");
        println!("always reaches it - but it is no longer cheaper, because there is no");
        println!("premium left to save. What a wider gap really buys is a bigger");
        println!("position to unload into a pool whose depth is fixed near 67 SOL.");
        return Ok(());
    }
    let model = if use_model {
        match score::Model::load(&cfg.model_path()) {
            Ok(m) => {
                println!(
                    "using model trained on {} outcomes ({} graduated)
",
                    m.trained_on, m.positives
                );
                Some(m)
            }
            Err(e) => {
                println!("no usable model ({e}); running on thresholds only
");
                None
            }
        }
    } else {
        None
    };
    let per_attempt = cfg.fees.attempt_cost(0, cfg.fees.cu_limit_completion);
    let (trades, rep) =
        backtest::run(&trajs, &cfg.strategy, &cfg.exit, model.as_ref(), per_attempt);

    println!("curves replayed : {}", rep.curves_replayed);
    println!("triggers        : {}", rep.triggers);
    println!(
        "  completions   : {}  (curve actually graduated: {})",
        rep.completions, rep.completions_that_graduated
    );
    println!(
        "  momentum      : {}  ({} up, {} down)",
        rep.momentum, rep.momentum_wins, rep.momentum_losses
    );
    println!("fees paid       : {:.6} SOL", sol(rep.fees_paid));
    println!();
    println!("REALISED (momentum round trips, sold back into the curve)");
    println!("  P&L           : {:+.4} SOL", rep.realised_pnl as f64 / 1e9);
    if rep.momentum > 0 {
        println!(
            "  hit rate      : {:.1}%",
            100.0 * rep.momentum_wins as f64 / rep.momentum as f64
        );
    }
    println!();
    println!("MARKED (completions, valued at the measured pool open - NOT a fill)");
    println!("  P&L           : {:+.4} SOL", rep.marked_pnl as f64 / 1e9);
    if rep.completions > 0 {
        println!(
            "  graduated     : {:.1}% of completions",
            100.0 * rep.completions_that_graduated as f64 / rep.completions as f64
        );
    }

    if show > 0 && !trades.is_empty() {
        println!();
        println!("{:<46} {:<11} {:>10} {:>12} {:>10}", "curve", "kind", "in SOL", "out SOL", "pnl SOL");
        let mut sorted = trades.clone();
        sorted.sort_by_key(|t| -t.pnl());
        for t in sorted.iter().take(show) {
            let out = t.realised.or(t.marked).unwrap_or(0);
            println!(
                "{:<46} {:<11} {:>10.4} {:>12.4} {:>+10.4}",
                &t.curve[..t.curve.len().min(44)],
                match t.kind {
                    backtest::Kind::Completion => "completion",
                    backtest::Kind::Momentum => "momentum",
                },
                sol(t.gross_in),
                sol(out),
                t.pnl() as f64 / 1e9
            );
        }
    }
    println!();
    println!(
        "Caveats: fills ignore our own market impact on later prices; completions are marked, not filled; no latency or competition is modelled, so every trigger here fills and live most will not."
    );
    Ok(())
}

/// Append every curve update to JSONL. This is the training set: each line is one
/// observation of a curve, and whether that curve later graduated is the label.
async fn collect(cfg: &Config, out: &std::path::Path, seconds: u64) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(out)
        .with_context(|| format!("opening {}", out.display()))?;

    let (tx, mut rx) = mpsc::channel(16384);
    let ws = cfg.rpc.ws.clone();
    let mut feeder = tokio::spawn(async move { feed::run(&ws, tx).await });

    let started = std::time::Instant::now();
    let mut n = 0u64;
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        if seconds > 0 && started.elapsed() >= Duration::from_secs(seconds) {
            break;
        }
        tokio::select! {
            _ = &mut shutdown => break,
            res = &mut feeder => {
                match res {
                    Ok(Err(e)) => return Err(e).context("curve feed failed"),
                    Ok(Ok(())) => tracing::warn!("feed closed early"),
                    Err(e) => return Err(anyhow::anyhow!("feed task panicked: {e}")),
                }
                break;
            }
            msg = rx.recv() => {
                let Some(u) = msg else { break };
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0);
                writeln!(
                    file,
                    r#"{{"ts":{},"curve":"{}","real_sol":{},"virt_sol":{},"real_tok":{},"virt_tok":{},"complete":{},"lamports":{}}}"#,
                    ts, u.curve_key, u.state.real_sol_reserves, u.state.virtual_sol_reserves,
                    u.state.real_token_reserves, u.state.virtual_token_reserves,
                    u.state.complete, u.lamports
                )?;
                n += 1;
                if n % 2000 == 0 {
                    file.flush()?;
                    tracing::info!("collected {n} updates -> {}", out.display());
                }
            }
        }
    }
    file.flush()?;
    feeder.abort();
    tracing::info!("collected {n} updates into {}", out.display());
    Ok(())
}

/// Watch the feed for a while and print the curves closest to graduating.
async fn scan(cfg: &Config, limit: usize, seconds: u64) -> Result<()> {
    let tracker = Arc::new(Tracker::new());
    let (tx, mut rx) = mpsc::channel(4096);
    let ws = cfg.rpc.ws.clone();

    let mut feeder = tokio::spawn(async move { feed::run(&ws, tx).await });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);

    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            // Surface feed failures instead of reporting an empty scan: a silently
            // dead subscription looks identical to a quiet market.
            res = &mut feeder => {
                match res {
                    Ok(Err(e)) => return Err(e).context("curve feed failed"),
                    Ok(Ok(())) => tracing::warn!("feed closed early"),
                    Err(e) => return Err(anyhow::anyhow!("feed task panicked: {e}")),
                }
                break;
            }
            msg = rx.recv() => match msg {
                Some(u) => { tracker.observe(Pubkey::default(), u.curve_key, u.state); }
                None => break,
            }
        }
    }
    feeder.abort();

    if tracker.is_empty() {
        tracing::warn!(
            "no curve updates arrived - the endpoint may not serve programSubscribe"
        );
    }

    println!("tracked {} curves in {seconds}s\n", tracker.len());
    println!("{:<46} {:>12} {:>10} {:>8}", "curve", "depth SOL", "gap SOL", "updates");
    for c in tracker.nearest_to_graduation(limit) {
        println!(
            "{:<46} {:>12.4} {:>10.4} {:>8}",
            c.curve_key.to_string(),
            sol(c.state.real_sol_reserves),
            sol(c.state.gap_to_graduation().unwrap_or(0)),
            c.updates()
        );
    }
    Ok(())
}

use solana_sdk::pubkey::Pubkey;

async fn run(cfg: Config) -> Result<()> {
    let tracker = Arc::new(Tracker::new());
    let gate = Arc::new(RiskGate::new(cfg.risk.clone()));

    // Live wiring is assembled up front so a missing key or an unreadable Global
    // account stops the bot before it starts, not on the first opportunity.
    let mut venue = None;
    let mut signer = None;
    if cfg.mode == Mode::Live {
        let path = cfg
            .keypair_path
            .as_deref()
            .context("live mode requires keypair_path")?;
        let kp = live::load_keypair(path)?;
        tracing::warn!("LIVE MODE - signing as {}", kp.pubkey());
        let mut v = live::Venue::connect(rpc::RpcClient::new(cfg.rpc.http.clone())?).await?;
        match cfg.rpc.lookup_table.as_deref() {
            Some(t) => {
                let key: Pubkey = t.parse().context("rpc.lookup_table is not a pubkey")?;
                let n = v.load_lookup_table(&key).await?;
                tracing::info!("lookup table {key} loaded, {n} addresses");
            }
            None => tracing::warn!(
                "no rpc.lookup_table configured - completion trades will be refused                  (buy+migrate is 1484 bytes, limit is 1232). Run `sniper create-alt`."
            ),
        }
        v.load_amm_config().await?;
        signer = Some(kp);
        venue = Some(v);
    } else {
        tracing::info!("paper mode - decisions are logged, nothing is signed");
    }

    // Paper mode has no wallet, so it runs the gate against a notional balance.
    // Live mode must use the real one, or every risk check is theatre.
    let mut balance = cfg.risk.max_exposure_lamports + cfg.risk.min_reserve_lamports;
    if let (Some(v), Some(kp)) = (venue.as_ref(), signer.as_ref()) {
        balance = v.balance(&kp.pubkey()).await?;
        tracing::info!("wallet balance {:.6} SOL", sol(balance));
        if balance <= cfg.risk.min_reserve_lamports {
            anyhow::bail!(
                "balance {:.6} SOL is at or below the {:.6} SOL reserve; fund the wallet first",
                sol(balance),
                sol(cfg.risk.min_reserve_lamports)
            );
        }
    }

    let (tx, mut rx) = mpsc::channel(8192);
    let ws = cfg.rpc.ws.clone();
    tokio::spawn(async move {
        let mut backoff = Duration::from_millis(250);
        loop {
            match feed::run(&ws, tx.clone()).await {
                Ok(()) => return,
                Err(e) => {
                    tracing::warn!("feed dropped: {e}; reconnecting in {backoff:?}");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(15));
                }
            }
        }
    });

    // Trade events: the only source of buyer identity, and therefore the only
    // thing that makes smart-money scoring possible.
    let (etx, mut erx) = mpsc::channel(8192);
    let ws_logs = cfg.rpc.ws.clone();
    tokio::spawn(async move {
        let mut backoff = Duration::from_millis(250);
        loop {
            match feed::run_logs(&ws_logs, etx.clone()).await {
                Ok(()) => return,
                Err(e) => {
                    tracing::warn!("log feed dropped: {e}; reconnecting in {backoff:?}");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(15));
                }
            }
        }
    });

    let smart = Arc::new(SmartMoney::load(&cfg.smart_path()).unwrap_or_else(|_| SmartMoney::new()));
    let mut model = score::Model::load(&cfg.model_path()).unwrap_or_else(|_| {
        tracing::info!("no trained model at {} - using priors", cfg.model_path().display());
        score::Model::default()
    });
    tracing::info!(
        "smart money: {} wallets known, model trained on {} outcomes",
        smart.known_wallets(),
        model.trained_on
    );

    // One entry per curve. Without this the bot re-enters the same curve on every
    // account update: a paper run fired 12 orders in 1.5 seconds, four of them on
    // one curve, and hit the rate limiter instead of trading.
    let mut acted: HashSet<Pubkey> = HashSet::new();
    // Distinct buyers seen per curve, from the trade-event feed.
    let mut buyers: HashMap<Pubkey, HashSet<Pubkey>> = HashMap::new();
    // The account feed identifies a curve; only the trade event carries the mint,
    // and every account a live order needs is derived from the mint.
    let mut curve_mint: HashMap<Pubkey, Pubkey> = HashMap::new();
    // Last feature row per curve, so an outcome can be trained on when it settles.
    let mut last_feats: HashMap<Pubkey, score::Features> = HashMap::new();

    // Open positions and their exit schedules. Without this the bot buys and
    // never sells - fatal for a strategy whose thesis is a 20-second hold.
    let mut book = positions::Book::new();
    let mut exits = tokio::time::interval(Duration::from_millis(500));
    let mut housekeeping = tokio::time::interval(Duration::from_secs(30));
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                tracing::info!("shutting down");
                break;
            }
            _ = exits.tick() => {
                let now = now_ms();
                for leg in book.due(now) {
                    // Price the tranche against the curve as we last saw it.
                    let Some(c) = tracker.get(&leg.curve) else {
                        tracing::debug!(curve=%leg.curve, "exit leg with no curve state; skipping tick");
                        continue;
                    };
                    // Our own price model. In paper mode it is the answer; in live
                    // mode it is only a fallback, because booking a quote as though
                    // it were a fill makes P&L a restatement of our own model.
                    let quoted = positions::quote_exit_leg(leg.venue, &c.state, leg.tokens);

                    let proceeds = if let (Some(v), Some(kp)) = (venue.as_mut(), signer.as_ref()) {
                        let sig = match execute_sell(v, kp, &cfg, &leg, &c).await {
                            Ok(sig) => sig,
                            Err(e) => {
                                tracing::error!(curve=%leg.curve, "sell failed: {e}");
                                // Leave the leg due so it retries next tick.
                                continue;
                            }
                        };
                        match v.confirm(&sig, CONFIRM_TIMEOUT).await {
                            Ok(rpc::Landed::Ok) => {}
                            Ok(rpc::Landed::Failed) => {
                                tracing::warn!(curve=%leg.curve, "sell reverted: {sig}");
                                continue;
                            }
                            Ok(rpc::Landed::Unknown) => {
                                tracing::warn!(curve=%leg.curve, "sell not landed yet: {sig}");
                                continue;
                            }
                            Err(e) => {
                                tracing::error!(curve=%leg.curve, "could not confirm sell {sig}: {e}");
                                continue;
                            }
                        }
                        match v.realised(&sig, &kp.pubkey()).await {
                            Ok(r) => {
                                let got = r.proceeds();
                                tracing::info!(
                                    curve=%leg.curve,
                                    "SELL {}/{} {} tokens -> {:.6} SOL realised (quoted {:.6}) {sig}",
                                    leg.leg, leg.of, leg.tokens, sol(got), sol(quoted)
                                );
                                got
                            }
                            Err(e) => {
                                // Landed, but we could not read it back. Booking the
                                // quote is a guess; say so loudly rather than let it
                                // pass as a measurement.
                                tracing::error!(
                                    curve=%leg.curve,
                                    "sell {sig} landed but proceeds unreadable: {e}; booking the QUOTE"
                                );
                                quoted
                            }
                        }
                    } else {
                        tracing::info!(
                            curve=%leg.curve,
                            "PAPER SELL {}/{} {} tokens -> {:.6} SOL",
                            leg.leg, leg.of, leg.tokens, sol(quoted)
                        );
                        quoted
                    };
                    book.on_sold(&leg.curve, leg.tokens, proceeds, now);
                }
                for done in book.take_finished() {
                    tracing::info!(
                        curve=%done.curve,
                        "CLOSED after {:.1}s: in {:.4} SOL, out {:.4} SOL, pnl {:+.4} SOL",
                        done.age_ms(now) as f64 / 1000.0,
                        sol(done.cost), sol(done.proceeds), done.pnl() as f64 / 1e9
                    );
                    gate.settle(done.cost, done.pnl());
                    if gate.is_killed() {
                        tracing::error!("daily loss limit hit - stopping");
                    }
                }
                for k in book.stuck(now, 120_000) {
                    tracing::warn!(curve=%k, "position stuck for over 2 minutes");
                }
            }
            _ = housekeeping.tick() => {
                // Evicted curves are settled as failures. They went quiet without
                // graduating, which for a bot holding for ~20 seconds is a real
                // negative, not censoring.
                let stale = tracker.evict_keys(Duration::from_secs(180));
                for k in &stale {
                    smart.settle(k, false);
                    if let Some(f) = last_feats.remove(k) {
                        model.update(&f, false);
                    }
                    buyers.remove(k);
                    acted.remove(k);
                }
                tracing::info!(
                    "settled {} curves | tracking {} | buyers pending on {} curves |                      wallets known {} | model trained on {} ({} positive)",
                    stale.len(), tracker.len(), smart.pending_curves(),
                    smart.known_wallets(), model.trained_on, model.positives
                );
                let _ = smart.save(&cfg.smart_path());
                // Only persist a model that has seen both outcomes in volume;
                // otherwise the next start inherits "everything graduates".
                if model.is_fit_to_persist() {
                    let _ = model.save(&cfg.model_path());
                } else {
                    tracing::debug!(
                        "not persisting model yet: {} outcomes, {} positive",
                        model.trained_on, model.positives
                    );
                }
            }
            ev = erx.recv() => {
                let Some(e) = ev else { continue };
                if !e.is_buy {
                    continue;
                }
                let curve = pumpfun::bonding_curve(&e.mint);
                curve_mint.insert(curve, e.mint);
                smart.observe_buy(&curve, &e.user);
                buyers.entry(curve).or_default().insert(e.user);
            }
            msg = rx.recv() => {
                let Some(u) = msg else { break };
                let c = tracker.observe(Pubkey::default(), u.curve_key, u.state);

                let seen = buyers.get(&u.curve_key);
                let buyer_vec: Vec<Pubkey> = seen.map(|s| s.iter().copied().collect()).unwrap_or_default();
                let ctx = strategy::Context {
                    model: Some(&model),
                    smart_money: smart.curve_score(&buyer_vec),
                    unique_buyers: buyer_vec.len() as u32,
                };
                last_feats.insert(u.curve_key, strategy::features(&c, &cfg.strategy, &ctx));

                // A graduation is a settled positive for everyone who bought in.
                if u.state.complete {
                    smart.settle(&u.curve_key, true);
                    if let Some(f) = last_feats.remove(&u.curve_key) {
                        model.update(&f, true);
                    }
                    buyers.remove(&u.curve_key);
                    continue;
                }

                if acted.contains(&u.curve_key) {
                    continue;
                }
                let decision = strategy::evaluate_with(&c, &cfg.strategy, &ctx);
                let (gross, label) = match &decision {
                    Decision::Complete { gross, net, .. } => {
                        (*gross, format!("COMPLETE gap {:.6} SOL", sol(*net)))
                    }
                    Decision::Momentum { gross, .. } => (*gross, "MOMENTUM".to_string()),
                    Decision::Skip(s) => {
                        if matches!(s, Skip::DemandTooWeak { .. }) {
                            tracing::debug!(curve=%c.curve_key, "skip: {s:?}");
                        }
                        continue;
                    }
                };

                if let Err(r) = gate.reserve(gross, balance) {
                    log_refusal(&c.curve_key, &r);
                    continue;
                }
                acted.insert(u.curve_key);

                tracing::info!(
                    curve = %c.curve_key,
                    depth = format!("{:.4}", sol(c.state.real_sol_reserves)),
                    order = format!("{:.6}", sol(gross)),
                    "{label}"
                );

                if let (Some(v), Some(kp)) = (venue.as_mut(), signer.as_ref()) {
                    let min_out = match &decision {
                        Decision::Complete { min_tokens_out, .. }
                        | Decision::Momentum { min_tokens_out, .. } => *min_tokens_out,
                        Decision::Skip(_) => 0,
                    };
                    let atomic = matches!(decision, Decision::Complete { .. });
                    match execute(v, kp, &cfg, &curve_mint, &c, gross, min_out, atomic).await {
                        Ok(sig) => {
                            // Submitting is not landing. Everything below this point
                            // - the position, the exit schedule, the P&L - used to be
                            // built on nothing but the node echoing our signature back.
                            match v.confirm(&sig, CONFIRM_TIMEOUT).await {
                                Ok(rpc::Landed::Ok) => {
                                    tracing::info!(curve = %c.curve_key, "LANDED {sig}")
                                }
                                Ok(rpc::Landed::Failed) => {
                                    tracing::warn!(curve = %c.curve_key, "REVERTED {sig}");
                                    gate.settle(gross, 0);
                                    acted.remove(&u.curve_key);
                                    continue;
                                }
                                Ok(rpc::Landed::Unknown) => {
                                    tracing::warn!(
                                        curve = %c.curve_key,
                                        "not landed within {CONFIRM_TIMEOUT:?}; treating as lost: {sig}"
                                    );
                                    gate.settle(gross, 0);
                                    acted.remove(&u.curve_key);
                                    continue;
                                }
                                Err(e) => {
                                    // We genuinely do not know. Releasing is the safe
                                    // side: worst case we hold tokens we forgot about,
                                    // which the reconciliation log will surface.
                                    tracing::error!(
                                        curve = %c.curve_key,
                                        "could not confirm {sig}: {e}; releasing"
                                    );
                                    gate.settle(gross, 0);
                                    acted.remove(&u.curve_key);
                                    continue;
                                }
                            }
                        }
                        Err(e) => {
                            // Do NOT fall through. This used to log and carry on to
                            // open a position and schedule sells for tokens we never
                            // bought, holding the risk reservation the whole time.
                            tracing::error!(curve = %c.curve_key, "submission failed: {e}");
                            gate.settle(gross, 0);
                            acted.remove(&u.curve_key);
                            continue;
                        }
                    }
                }

                // Open the position and schedule its exit. The risk gate stays
                // reserved until the last tranche clears - that is what makes
                // max_open_positions mean anything.
                let tokens = match &decision {
                    Decision::Complete { net, .. } => c.state.tokens_out(*net),
                    Decision::Momentum { gross, .. } => {
                        c.state.tokens_out(pumpfun::net_for_gross(*gross))
                    }
                    Decision::Skip(_) => 0,
                };
                // `unwrap_or_default()` here yielded the all-zeros pubkey, which is a
                // valid-looking Pubkey that resolves to nothing. The position would
                // open and every exit against it would fail forever.
                let Some(mint) = curve_mint.get(&u.curve_key).copied() else {
                    tracing::error!(
                        curve = %c.curve_key,
                        "no mint known for this curve; cannot schedule an exit"
                    );
                    gate.settle(gross, 0);
                    acted.remove(&u.curve_key);
                    continue;
                };
                // A completion graduates the curve, so it can only ever be sold on
                // the PumpSwap pool the migration creates.
                let exit_venue = if matches!(decision, Decision::Complete { .. }) {
                    positions::ExitVenue::PumpSwap
                } else {
                    positions::ExitVenue::BondingCurve
                };
                let plan = strategy::plan_exit(tokens, &cfg.exit);
                if !book.open(u.curve_key, mint, exit_venue, tokens, gross, plan, now_ms()) {
                    // Could not schedule an exit; release the reservation rather
                    // than hold tokens with no way out.
                    tracing::warn!(curve=%c.curve_key, "could not open position; releasing");
                    gate.settle(gross, 0);
                }
            }
        }
    }
    let (exposure, loss, open, killed) = gate.snapshot();
    tracing::info!(
        "final: exposure {:.4} SOL, realised loss {:.4} SOL, open {open}, killed {killed}",
        sol(exposure),
        sol(loss)
    );
    Ok(())
}

/// Create and populate the lookup table, then print its address for the config.
/// Create a fresh signing keypair and write it in Solana CLI format.
///
/// Never overwrites. A keypair file is the only copy of the key: if it holds funds
/// and you clobber it, the funds are gone, and no error message afterwards can
/// undo that.
fn keygen(out: &std::path::Path) -> Result<()> {
    use solana_sdk::signer::Signer;

    if out.exists() {
        anyhow::bail!(
            "{} already exists - refusing to overwrite. If you really want a new \
             key, move the old file aside first.",
            out.display()
        );
    }
    if let Some(dir) = out.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }

    let kp = solana_sdk::signature::Keypair::new();
    let bytes = kp.to_bytes().to_vec();
    std::fs::write(out, serde_json::to_string(&bytes)?)
        .with_context(|| format!("writing {}", out.display()))?;

    // Best-effort tightening. Windows ACLs are not POSIX modes, so this is a
    // readonly flag rather than real protection - say so rather than imply more.
    let mut perms = std::fs::metadata(out)?.permissions();
    perms.set_readonly(true);
    let _ = std::fs::set_permissions(out, perms);

    println!();
    println!("  public key : {}", kp.pubkey());
    println!("  keypair    : {}", out.display());
    println!();
    println!("  FUND THIS ADDRESS, then back the file up somewhere off this machine.");
    println!("  Anyone who reads that file controls the wallet. It is not encrypted.");
    println!();
    println!("  Next:  sniper create-alt        (needs ~0.01 SOL, signs and submits)");
    println!();
    Ok(())
}

/// Stream fresh pools and record their first minute of quote-reserve movement.
async fn watch_pools(cfg: &Config, out: &std::path::Path, window: u64) -> Result<()> {
    let out = out.to_string_lossy().to_string();
    if let Some(dir) = std::path::Path::new(&out).parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).ok();
        }
    }
    tracing::info!(
        "watching new PumpSwap pools for {window}s each -> {out} (ctrl-c to stop)"
    );
    let window = Duration::from_secs(window.max(1));
    let mut backoff = Duration::from_millis(250);
    loop {
        match sniper::poolwatch::run(&cfg.rpc.ws, &out, window).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!("pool feed dropped: {e}; reconnecting in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(15));
            }
        }
    }
}

async fn create_alt(cfg: &Config) -> Result<()> {
    use solana_address_lookup_table_interface::instruction::{create_lookup_table, extend_lookup_table};
    if cfg.mode != Mode::Live {
        anyhow::bail!("create-alt signs and submits; run it with --mode live");
    }
    let path = cfg.keypair_path.as_deref().context("keypair_path required")?;
    let kp = live::load_keypair(path)?;
    let authority = kp.pubkey();
    let client = rpc::RpcClient::new(cfg.rpc.http.clone())?;

    let slot = client.slot().await?;
    let (create_ix, table) = create_lookup_table(authority, authority, slot);
    let addrs = resolve::constant_accounts();
    let extend_ix = extend_lookup_table(table, authority, Some(authority), addrs.clone());

    let blockhash = client.latest_blockhash().await?;
    let tx = sniper::exec::build_attempt(sniper::exec::AttemptSpec {
        // Creating and extending a lookup table is cheap and not a race.
        cu_limit: cfg.fees.cu_limit_for(false),
        payer: &kp,
        blockhash,
        body: vec![create_ix, extend_ix],
        fee: &cfg.fees,
        tier: 0,
        tip_account: None,
        luts: vec![],
    })?;
    let sig = client
        .send_transaction(&sniper::exec::encode_tx(&tx)?, false)
        .await?;

    println!("lookup table : {table}");
    println!("addresses    : {}", addrs.len());
    println!("signature    : {sig}");
    println!();
    println!("Add to config.toml under [rpc]:");
    println!("  lookup_table = \"{table}\"");
    println!();
    println!("A table is only usable one slot after creation - wait a few seconds.");
    Ok(())
}

/// Wall clock in milliseconds.
/// How long to wait for a completion to land before writing it off.
///
/// A completion that has not landed in ~12 slots has lost the race; the pool it
/// wanted to create already exists. Waiting longer only delays the next attempt.
const CONFIRM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Submit one exit tranche.
async fn execute_sell(
    venue: &mut live::Venue,
    signer: &solana_sdk::signer::keypair::Keypair,
    cfg: &Config,
    leg: &positions::DueLeg,
    tracked: &tracker::TrackedCurve,
) -> Result<String> {
    let token_program = venue.token_program(&leg.mint).await?;
    let ctx = resolve::MintContext::new(leg.mint, &tracked.state, token_program)?;
    let blockhash = venue.latest_blockhash().await?;
    let nonce = u64::from(leg.curve.as_ref()[1]) + leg.leg as u64;

    // A graduated curve can only be sold on PumpSwap; pump.fun refuses it.
    if leg.venue == positions::ExitVenue::PumpSwap {
        let tx = venue
            .build_pumpswap_sell(signer, &ctx, blockhash, &cfg.fees, 0, nonce, leg.tokens, 0)
            .await?;
        return venue.submit(&tx).await;
    }
    // Exits are not a race: cheapest tier, no tip, and a slippage floor rather
    // than min_sol_out = 0, which would accept any price at all.
    let expected = tracked.state.sol_out(leg.tokens);
    let min_out = expected
        .saturating_mul(10_000 - cfg.strategy.slippage_bps.min(10_000))
        / 10_000;
    let tx = venue.build_sell(
        signer, &ctx, blockhash, &cfg.fees, 0, nonce, leg.tokens, min_out,
    )?;
    venue.submit(&tx).await
}

fn log_refusal(curve: &Pubkey, r: &Refusal) {
    match r {
        Refusal::KillSwitch => tracing::error!("refused: kill switch"),
        _ => tracing::debug!(%curve, "refused: {r:?}"),
    }
}

/// Build and submit one real buy.
///
/// Every account is derived from the mint plus the curve state already held — no
/// extra round trip except the mint's token program, which is cached.
async fn execute(
    venue: &mut live::Venue,
    signer: &solana_sdk::signer::keypair::Keypair,
    cfg: &Config,
    curve_mint: &HashMap<Pubkey, Pubkey>,
    tracked: &tracker::TrackedCurve,
    gross: u64,
    min_tokens_out: u64,
    atomic_completion: bool,
) -> Result<String> {
    // The mint only becomes known once a trade event has been seen for this curve.
    // Refusing here is correct: a buy built against a guessed mint would fail on
    // chain and still pay the fee.
    let mint = *curve_mint
        .get(&tracked.curve_key)
        .with_context(|| format!("no mint known yet for curve {}", tracked.curve_key))?;

    let token_program = venue.token_program(&mint).await?;
    let ctx = resolve::MintContext::new(mint, &tracked.state, token_program)?;
    let blockhash = venue.latest_blockhash().await?;

    // Spread the fee recipient and tip account across attempts.
    let nonce = u64::from(tracked.curve_key.as_ref()[0]) + tracked.updates() as u64;
    let tier = cfg
        .fees
        .max_affordable_tier(gross, cfg.fees.cu_limit_for(atomic_completion))
        .unwrap_or(0);
    let tip = cfg
        .rpc
        .jito_block_engine
        .as_ref()
        .map(|_| sniper::exec::pick_tip_account(nonce));

    // A completion MUST carry the migration in the same transaction. Two separate
    // transactions lose the race to the keeper and leave us holding curve tokens
    // 20.7% above where the pool opens - a losing trade by construction.
    let tx = if atomic_completion {
        venue.build_completion(
            signer, &ctx, blockhash, &cfg.fees, tier, nonce, gross, min_tokens_out, tip, vec![],
        )?
    } else {
        venue.build_buy(
            signer, &ctx, blockhash, &cfg.fees, tier, nonce, gross, min_tokens_out, tip,
        )?
    };
    venue.submit(&tx).await
}
