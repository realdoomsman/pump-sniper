//! Offline fitting from collected curve trajectories.
//!
//! `sniper collect` writes one JSON object per curve update. This turns that
//! stream into labelled training rows and fits [`crate::score::Model`].
//!
//! The whole difficulty here is **censoring**. A curve that was still climbing
//! when collection stopped has not failed — we simply do not know yet. Counting it
//! as a negative is the single easiest way to train a model that believes nothing
//! ever graduates, so those rows are dropped rather than labelled.

use crate::pumpfun;
use crate::score::{Features, Model};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

/// One line of `curves.jsonl`.
#[derive(Debug, Clone, Deserialize)]
pub struct Sample {
    pub ts: u64,
    pub curve: String,
    pub real_sol: u64,
    pub complete: bool,
}

/// Every observation of one curve, oldest first.
#[derive(Debug, Clone)]
pub struct Trajectory {
    pub curve: String,
    pub samples: Vec<Sample>,
}

/// What ultimately happened to a curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Reached the graduation line while we watched.
    Graduated,
    /// Went quiet well before collection ended and never graduated.
    Dead,
    /// Still active at the end — unknown, and must not be trained on.
    Censored,
}

/// How the trajectory stream is turned into labels.
#[derive(Debug, Clone, Copy)]
pub struct LabelPolicy {
    /// Silence for this long counts as dead.
    pub settle_after: Duration,
    /// Curves whose last update falls inside this much of the end of the data are
    /// censored regardless of silence, because we cannot see what happened next.
    pub censor_tail: Duration,
    /// Observations to sample per curve, spread across its trajectory.
    pub samples_per_curve: usize,
}

impl Default for LabelPolicy {
    fn default() -> Self {
        Self {
            settle_after: Duration::from_secs(600),
            censor_tail: Duration::from_secs(900),
            samples_per_curve: 6,
        }
    }
}

/// Read and group a JSONL collection file.
pub fn load(path: &Path) -> Result<Vec<Trajectory>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let mut by_curve: HashMap<String, Vec<Sample>> = HashMap::new();
    let mut bad = 0usize;
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Sample>(line) {
            Ok(s) => by_curve.entry(s.curve.clone()).or_default().push(s),
            // A collector killed mid-write leaves one torn line; that is not a
            // reason to throw away the whole dataset.
            Err(_) => bad += 1,
        }
    }
    if bad > 0 {
        tracing::debug!("skipped {bad} unparseable lines");
    }
    let mut out: Vec<Trajectory> = by_curve
        .into_iter()
        .map(|(curve, mut samples)| {
            samples.sort_by_key(|s| s.ts);
            Trajectory { curve, samples }
        })
        .collect();
    out.sort_by(|a, b| a.curve.cmp(&b.curve));
    Ok(out)
}

impl Trajectory {
    pub fn first_ts(&self) -> u64 {
        self.samples.first().map(|s| s.ts).unwrap_or(0)
    }
    pub fn last_ts(&self) -> u64 {
        self.samples.last().map(|s| s.ts).unwrap_or(0)
    }
    /// Deepest the curve got *while still live*.
    ///
    /// Post-graduation samples are excluded because pump.fun drains
    /// `real_sol_reserves` into the new pool at migration, so a graduated curve
    /// reads as 0 SOL afterwards.
    pub fn peak_depth(&self) -> u64 {
        self.samples
            .iter()
            .take_while(|s| !s.complete)
            .map(|s| s.real_sol)
            .max()
            .unwrap_or(0)
    }

    pub fn graduated(&self) -> bool {
        let line = pumpfun::graduation_lamports();
        self.samples
            .iter()
            .any(|s| s.complete || s.real_sol >= line)
    }

    /// True when the very first thing we saw was an already-graduated curve.
    ///
    /// These are useless as training data and actively harmful: the curve is
    /// drained to 0 SOL after migration, so it looks like a shallow curve that
    /// graduated. 36 of them in one collection run were being labelled as
    /// "graduated at under 1 SOL of depth", which is impossible and teaches the
    /// model exactly the wrong thing.
    pub fn only_seen_after_graduation(&self) -> bool {
        self.samples.first().is_some_and(|s| s.complete)
    }

    /// Classify against the end of the whole dataset.
    pub fn outcome(&self, dataset_end_ms: u64, p: &LabelPolicy) -> Outcome {
        if self.only_seen_after_graduation() {
            return Outcome::Censored;
        }
        if self.graduated() {
            return Outcome::Graduated;
        }
        let quiet_ms = dataset_end_ms.saturating_sub(self.last_ts());
        if quiet_ms < p.censor_tail.as_millis() as u64 {
            return Outcome::Censored;
        }
        if quiet_ms >= p.settle_after.as_millis() as u64 {
            Outcome::Dead
        } else {
            Outcome::Censored
        }
    }

    /// Features as of sample `i`, using earlier samples for velocity.
    pub fn features_at(&self, i: usize, window: Duration) -> Features {
        let s = &self.samples[i];
        let cutoff = s.ts.saturating_sub(window.as_millis() as u64);
        let first = self.samples[..=i]
            .iter()
            .find(|x| x.ts >= cutoff)
            .unwrap_or(s);
        let dt = (s.ts.saturating_sub(first.ts)) as f64 / 1000.0;
        let velocity = if dt > 0.0 {
            (s.real_sol as f64 - first.real_sol as f64) / dt
        } else {
            0.0
        };
        Features {
            depth_lamports: s.real_sol,
            velocity_lamports_s: velocity,
            updates: (i + 1) as u32,
            age: Duration::from_millis(s.ts.saturating_sub(self.first_ts())),
            // The account feed carries no buyer identity, so these are genuinely
            // unknown offline. They stay 0 here and are supplied live from the
            // trade-event feed; the model simply learns nothing about them from
            // this dataset rather than learning something false.
            smart_money: 0.0,
            unique_buyers: 0,
        }
    }
}

/// A labelled training row.
#[derive(Debug, Clone, Copy)]
pub struct Row {
    pub features: Features,
    pub graduated: bool,
}

/// Summary of a fitting run.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub curves: usize,
    pub graduated: usize,
    pub dead: usize,
    pub censored: usize,
    pub rows: usize,
    pub positive_rows: usize,
    /// (lower_lamports, upper_lamports, seen, graduated).
    pub depth_buckets: Vec<(u64, u64, usize, usize)>,
    pub log_loss_before: f64,
    pub log_loss_after: f64,
}

const BUCKETS: [(u64, u64); 8] = [
    (0, 1_000_000_000),
    (1_000_000_000, 5_000_000_000),
    (5_000_000_000, 10_000_000_000),
    (10_000_000_000, 20_000_000_000),
    (20_000_000_000, 40_000_000_000),
    (40_000_000_000, 60_000_000_000),
    (60_000_000_000, 75_000_000_000),
    (75_000_000_000, u64::MAX),
];

/// Turn trajectories into labelled rows, dropping censored curves.
pub fn build_rows(
    trajs: &[Trajectory],
    policy: &LabelPolicy,
    window: Duration,
) -> (Vec<Row>, Report) {
    let mut rep = Report {
        curves: trajs.len(),
        ..Default::default()
    };
    let dataset_end = trajs.iter().map(|t| t.last_ts()).max().unwrap_or(0);
    let mut rows = Vec::new();
    let mut buckets: Vec<(u64, u64, usize, usize)> =
        BUCKETS.iter().map(|(a, b)| (*a, *b, 0, 0)).collect();

    for t in trajs {
        let outcome = t.outcome(dataset_end, policy);
        match outcome {
            Outcome::Graduated => rep.graduated += 1,
            Outcome::Dead => rep.dead += 1,
            Outcome::Censored => {
                rep.censored += 1;
                continue;
            }
        }
        let graduated = outcome == Outcome::Graduated;

        // Peak depth reached, bucketed - this is the base-rate table.
        let peak = t.peak_depth();
        for b in buckets.iter_mut() {
            if peak >= b.0 && peak < b.1 {
                b.2 += 1;
                if graduated {
                    b.3 += 1;
                }
            }
        }

        // Spread the sampled observations across the trajectory rather than taking
        // the first N, so deep and shallow states are both represented.
        // Only sample states from while the curve was live. A post-migration
        // sample has real_sol == 0 and a known label - pure leakage.
        let live = t.samples.iter().take_while(|s| !s.complete).count().max(1);
        let n = live;
        let take = policy.samples_per_curve.min(n);
        for k in 0..take {
            let i = if take == 1 { 0 } else { k * (n - 1) / (take - 1) };
            let f = t.features_at(i, window);
            // Never train on a state at or past the line: the label is trivially
            // known there and it teaches the model nothing but the threshold.
            if f.depth_lamports >= pumpfun::graduation_lamports() {
                continue;
            }
            rows.push(Row {
                features: f,
                graduated,
            });
            if graduated {
                rep.positive_rows += 1;
            }
        }
    }
    rep.rows = rows.len();
    rep.depth_buckets = buckets;
    (rows, rep)
}

/// Why a dataset cannot be fitted yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unusable {
    NoRows { censored: usize },
    NoPositives { rows: usize },
    NoNegatives { rows: usize },
    Degenerate { positive_rows: usize, rows: usize },
}

impl std::fmt::Display for Unusable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoRows { censored } => write!(
                f,
                "no settled curves yet ({censored} still censored) - collect for longer"
            ),
            Self::NoPositives { rows } => write!(
                f,
                "{rows} rows but no graduations - the model cannot learn a positive class"
            ),
            Self::NoNegatives { rows } => write!(
                f,
                "{rows} rows and every one graduated - fitting this teaches the model \
                 that everything succeeds. Collect until dead curves settle"
            ),
            Self::Degenerate {
                positive_rows,
                rows,
            } => write!(
                f,
                "{positive_rows} of {rows} rows are positive; that class balance is an \
                 artifact of censoring, not a real base rate"
            ),
        }
    }
}

/// Refuse to fit a dataset that would produce a confidently wrong model.
///
/// The failure this exists to prevent is real and was hit on the first live run:
/// after 20 minutes of collection almost every curve was still censored, leaving
/// 38 rows that were *all* graduations. Fitting them produced a model with a +4.7
/// bias that scored every curve near 1.0 and would have waved through every trade.
pub fn usable(rep: &Report) -> Result<(), Unusable> {
    if rep.rows == 0 {
        return Err(Unusable::NoRows {
            censored: rep.censored,
        });
    }
    if rep.positive_rows == 0 {
        return Err(Unusable::NoPositives { rows: rep.rows });
    }
    let negatives = rep.rows - rep.positive_rows;
    if negatives == 0 {
        return Err(Unusable::NoNegatives { rows: rep.rows });
    }
    // Real pump.fun graduation rates are low single-digit percent. Anything above
    // half is censoring, not signal.
    if rep.positive_rows * 2 > rep.rows {
        return Err(Unusable::Degenerate {
            positive_rows: rep.positive_rows,
            rows: rep.rows,
        });
    }
    Ok(())
}

fn log_loss(m: &Model, rows: &[Row]) -> f64 {
    if rows.is_empty() {
        return f64::NAN;
    }
    let mut acc = 0.0;
    for r in rows {
        let p = m.predict(&r.features).clamp(1e-9, 1.0 - 1e-9);
        acc += if r.graduated { -p.ln() } else { -(1.0 - p).ln() };
    }
    acc / rows.len() as f64
}

/// Fit a model over the rows for `epochs` passes.
///
/// Positives are heavily outnumbered, so they are replayed `pos_weight` times per
/// pass. Without that the fitted model just predicts "never" and scores well on
/// log loss while being useless.
pub fn fit(rows: &[Row], epochs: usize, pos_weight: usize) -> (Model, f64, f64) {
    let mut m = Model::default();
    let before = log_loss(&m, rows);
    for _ in 0..epochs.max(1) {
        for r in rows {
            let reps = if r.graduated { pos_weight.max(1) } else { 1 };
            for _ in 0..reps {
                m.update(&r.features, r.graduated);
            }
        }
    }
    let after = log_loss(&m, rows);
    // Report distinct labelled rows, not SGD steps. Epochs and positive replay
    // inflate the raw counters by ~240x, which made `explain` claim 206,010
    // outcomes from a 6,867-row dataset.
    m.trained_on = rows.len() as u64;
    m.positives = rows.iter().filter(|r| r.graduated).count() as u64;
    (m, before, after)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn traj(curve: &str, depths: &[(u64, u64)], complete: bool) -> Trajectory {
        let mut samples: Vec<Sample> = depths
            .iter()
            .map(|(ts, sol)| Sample {
                ts: *ts,
                curve: curve.into(),
                real_sol: *sol,
                complete: false,
            })
            .collect();
        if complete {
            if let Some(last) = samples.last_mut() {
                last.complete = true;
            }
        }
        Trajectory {
            curve: curve.into(),
            samples,
        }
    }

    #[test]
    fn a_curve_first_seen_already_graduated_is_excluded() {
        // pump.fun drains real_sol into the pool at migration, so such a curve
        // reads as 0 SOL and would be labelled "graduated at 0 depth".
        let mut t = traj("a", &[(0, 0), (1000, 0)], false);
        for s in t.samples.iter_mut() {
            s.complete = true;
        }
        assert!(t.only_seen_after_graduation());
        assert_eq!(t.outcome(999_999, &LabelPolicy::default()), Outcome::Censored);
        let (rows, rep) = build_rows(&[t], &LabelPolicy::default(), Duration::from_secs(30));
        assert!(rows.is_empty());
        assert_eq!(rep.graduated, 0);
        assert_eq!(rep.censored, 1);
    }

    #[test]
    fn peak_depth_ignores_post_migration_drain() {
        let mut t = traj("a", &[(0, 10_000_000_000), (1000, 0)], false);
        t.samples[1].complete = true;
        assert_eq!(t.peak_depth(), 10_000_000_000, "drain must not erase the peak");
    }

    #[test]
    fn graduation_is_detected_by_flag_or_by_depth() {
        assert!(traj("a", &[(0, 1), (1000, 2)], true).graduated());
        let line = pumpfun::graduation_lamports();
        assert!(traj("b", &[(0, 1), (1000, line)], false).graduated());
        assert!(!traj("c", &[(0, 1), (1000, 2)], false).graduated());
    }

    #[test]
    fn a_curve_still_active_at_the_end_is_censored_not_dead() {
        let p = LabelPolicy::default();
        let t = traj("a", &[(0, 1_000_000_000), (10_000, 2_000_000_000)], false);
        // Dataset ends right after its last update.
        assert_eq!(t.outcome(10_000, &p), Outcome::Censored);
    }

    #[test]
    fn a_long_silent_curve_is_dead() {
        let p = LabelPolicy::default();
        let t = traj("a", &[(0, 1_000_000_000)], false);
        let end = 10_000_000; // ~2.7 hours later
        assert_eq!(t.outcome(end, &p), Outcome::Dead);
    }

    #[test]
    fn graduated_beats_censoring() {
        let p = LabelPolicy::default();
        let t = traj("a", &[(0, 1), (10, 2)], true);
        assert_eq!(t.outcome(10, &p), Outcome::Graduated);
    }

    #[test]
    fn censored_curves_never_become_training_rows() {
        let p = LabelPolicy::default();
        let live = traj("live", &[(9_000_000, 5_000_000_000)], false);
        let dead = traj("dead", &[(0, 5_000_000_000)], false);
        let (rows, rep) = build_rows(&[live, dead], &p, Duration::from_secs(30));
        assert_eq!(rep.censored, 1);
        assert_eq!(rep.dead, 1);
        assert!(rows.iter().all(|r| !r.graduated));
        assert!(!rows.is_empty());
    }

    #[test]
    fn velocity_is_computed_from_earlier_samples() {
        let t = traj("a", &[(0, 0), (1_000, 1_000_000_000), (2_000, 2_000_000_000)], false);
        let f = t.features_at(2, Duration::from_secs(30));
        // 2 SOL over 2 seconds = 1 SOL/s.
        assert!((f.velocity_lamports_s - 1e9).abs() < 1e6, "{}", f.velocity_lamports_s);
    }

    #[test]
    fn rows_past_the_graduation_line_are_excluded() {
        let line = pumpfun::graduation_lamports();
        let t = traj("a", &[(0, line + 1), (1000, line + 2)], true);
        let (rows, _) = build_rows(&[t], &LabelPolicy::default(), Duration::from_secs(30));
        assert!(rows.is_empty(), "must not train on post-graduation states");
    }

    #[test]
    fn fitting_reduces_log_loss_on_separable_data() {
        let mut trajs = Vec::new();
        for i in 0..40 {
            // Winners climb high then graduate.
            trajs.push(traj(
                &format!("w{i}"),
                &[(0, 1_000_000_000), (5_000, 40_000_000_000), (10_000, 70_000_000_000)],
                true,
            ));
            // Losers stall shallow and go quiet.
            trajs.push(traj(
                &format!("l{i}"),
                &[(0, 100_000_000), (5_000, 200_000_000)],
                false,
            ));
        }
        let policy = LabelPolicy {
            censor_tail: Duration::from_millis(1),
            settle_after: Duration::from_millis(1),
            samples_per_curve: 3,
        };
        let (rows, rep) = build_rows(&trajs, &policy, Duration::from_secs(30));
        assert_eq!(rep.graduated, 40);
        assert_eq!(rep.dead, 40);
        assert!(!rows.is_empty());

        let (m, before, after) = fit(&rows, 20, 1);
        assert!(after < before, "log loss {before} -> {after}");

        let deep = Features {
            depth_lamports: 70_000_000_000,
            velocity_lamports_s: 8e9,
            updates: 3,
            age: Duration::from_secs(10),
            smart_money: 0.0,
            unique_buyers: 0,
        };
        let shallow = Features {
            depth_lamports: 150_000_000,
            velocity_lamports_s: 0.02e9,
            updates: 2,
            age: Duration::from_secs(5),
            smart_money: 0.0,
            unique_buyers: 0,
        };
        assert!(
            m.predict(&deep) > m.predict(&shallow),
            "deep {} vs shallow {}",
            m.predict(&deep),
            m.predict(&shallow)
        );
    }

    #[test]
    fn depth_buckets_count_by_peak_depth() {
        // censor_tail must be 0 here: this curve's last update *is* the end of the
        // dataset, so any non-zero tail would (correctly) censor it.
        let policy = LabelPolicy {
            censor_tail: Duration::ZERO,
            settle_after: Duration::ZERO,
            samples_per_curve: 2,
        };
        let t = traj("a", &[(0, 100_000_000), (1000, 3_000_000_000)], false);
        let (_, rep) = build_rows(&[t], &policy, Duration::from_secs(30));
        let b = rep
            .depth_buckets
            .iter()
            .find(|b| b.0 == 1_000_000_000)
            .unwrap();
        assert_eq!(b.2, 1, "peak 3 SOL belongs in the 1-5 SOL bucket");
    }

    #[test]
    fn fitting_reports_rows_not_sgd_steps() {
        let rows: Vec<Row> = (0..100)
            .map(|i| Row {
                features: Features {
                    depth_lamports: 10_000_000_000,
                    velocity_lamports_s: 1e9,
                    updates: 5,
                    age: Duration::from_secs(10),
                    smart_money: 0.0,
                    unique_buyers: 0,
                },
                graduated: i % 10 == 0,
            })
            .collect();
        let (m, _, _) = fit(&rows, 30, 8);
        assert_eq!(m.trained_on, 100, "must count rows, not updates");
        assert_eq!(m.positives, 10);
    }

    #[test]
    fn all_positive_datasets_are_refused() {
        // The exact failure seen on the first live run.
        let rep = Report { rows: 38, positive_rows: 38, censored: 882, ..Default::default() };
        assert_eq!(usable(&rep), Err(Unusable::NoNegatives { rows: 38 }));
    }

    #[test]
    fn all_negative_datasets_are_refused() {
        let rep = Report { rows: 100, positive_rows: 0, ..Default::default() };
        assert_eq!(usable(&rep), Err(Unusable::NoPositives { rows: 100 }));
    }

    #[test]
    fn empty_datasets_are_refused() {
        let rep = Report { rows: 0, censored: 500, ..Default::default() };
        assert_eq!(usable(&rep), Err(Unusable::NoRows { censored: 500 }));
    }

    #[test]
    fn implausible_class_balance_is_refused() {
        let rep = Report { rows: 100, positive_rows: 80, ..Default::default() };
        assert!(matches!(usable(&rep), Err(Unusable::Degenerate { .. })));
    }

    #[test]
    fn a_realistic_class_balance_is_accepted() {
        let rep = Report { rows: 1000, positive_rows: 30, ..Default::default() };
        assert!(usable(&rep).is_ok());
    }

    #[test]
    fn load_survives_a_torn_final_line() {
        let dir = std::env::temp_dir();
        let path = dir.join("sniper_train_torn.jsonl");
        std::fs::write(
            &path,
            "{\"ts\":1,\"curve\":\"a\",\"real_sol\":5,\"complete\":false}\n{\"ts\":2,\"cur",
        )
        .unwrap();
        let t = load(&path).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].samples.len(), 1);
        let _ = std::fs::remove_file(&path);
    }
}
