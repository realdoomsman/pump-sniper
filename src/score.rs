//! Online logistic model: does this curve reach graduation?
//!
//! This is the "learning" half of the bot. The reference wallet has no model at
//! all — it fires on curve depth and pays for it. We predict `P(graduate)` from
//! live features, size on that probability, and update the weights from realised
//! outcomes as they arrive.
//!
//! Deliberately a linear model fitted by SGD rather than anything larger. The
//! label rate is low, the features are few, and a model whose coefficients you can
//! read and sanity-check is worth more here than one you cannot. It also trains
//! incrementally with no framework, no snapshotting of a training set, and a
//! bounded, predictable cost per update on the hot path.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Number of model inputs. Keep in sync with [`Features::to_row`].
pub const N_FEATURES: usize = 6;

/// Human-readable names, index-aligned with [`Features::to_row`].
pub const FEATURE_NAMES: [&str; N_FEATURES] = [
    "depth",
    "velocity",
    "updates",
    "age",
    "smart_money",
    "unique_buyers",
];

/// Raw, unscaled observations about one curve at one instant.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Features {
    /// Real SOL currently in the curve, in lamports.
    pub depth_lamports: u64,
    /// Net inflow over the trailing window, lamports per second.
    pub velocity_lamports_s: f64,
    /// How many distinct account updates we have seen for this curve.
    pub updates: u32,
    /// How long we have been watching it.
    pub age: Duration,
    /// Summed reputation of wallets seen buying, from [`crate::smart`].
    pub smart_money: f64,
    /// Distinct buyers observed.
    pub unique_buyers: u32,
}

impl Features {
    /// Scale into roughly unit range so one SGD learning rate suits every input.
    ///
    /// Fixed divisors rather than a running standardiser: the scales here are
    /// known from the protocol itself (graduation is 85 SOL, holds last ~20s), and
    /// a moving normaliser would silently change the meaning of stored weights.
    pub fn to_row(&self) -> [f64; N_FEATURES] {
        let grad = crate::pumpfun::graduation_lamports() as f64;
        [
            (self.depth_lamports as f64 / grad).clamp(0.0, 1.5),
            (self.velocity_lamports_s / 1e9).clamp(-10.0, 10.0),
            (f64::from(self.updates).ln_1p() / 5.0).clamp(0.0, 2.0),
            (self.age.as_secs_f64() / 300.0).clamp(0.0, 4.0),
            self.smart_money.clamp(0.0, 5.0),
            (f64::from(self.unique_buyers).ln_1p() / 4.0).clamp(0.0, 2.0),
        ]
    }
}

/// Logistic regression with L2 shrinkage, fitted by SGD.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub weights: Vec<f64>,
    pub bias: f64,
    /// SGD step size.
    pub lr: f64,
    /// L2 penalty, applied per update.
    pub l2: f64,
    /// Labelled outcomes seen so far.
    pub trained_on: u64,
    /// Positive labels seen, for monitoring drift in the base rate.
    pub positives: u64,
}

impl Default for Model {
    fn default() -> Self {
        // Bias is a prior, not a guess: most curves die. A base rate of roughly 1
        // in 30 gives log(p/(1-p)) ~= -3.4. Depth starts as the only positive
        // weight because it is the one relationship established by measurement -
        // the reference wallet never bought below 3.49 SOL and everything it
        // completed was already deep. The rest start at zero and have to earn
        // their coefficient from outcomes.
        let mut weights = vec![0.0; N_FEATURES];
        weights[0] = 2.0;
        Self {
            weights,
            bias: -3.4,
            lr: 0.05,
            l2: 1e-4,
            trained_on: 0,
            positives: 0,
        }
    }
}

fn sigmoid(z: f64) -> f64 {
    if z >= 0.0 {
        1.0 / (1.0 + (-z).exp())
    } else {
        let e = z.exp();
        e / (1.0 + e)
    }
}

impl Model {
    /// P(this curve graduates), in 0..1.
    pub fn predict(&self, f: &Features) -> f64 {
        sigmoid(self.logit(f))
    }

    pub fn logit(&self, f: &Features) -> f64 {
        let row = f.to_row();
        let mut z = self.bias;
        for i in 0..N_FEATURES {
            z += self.weights.get(i).copied().unwrap_or(0.0) * row[i];
        }
        z
    }

    /// One SGD step against a realised outcome.
    ///
    /// Only call this with a *settled* label. A curve that is still climbing when
    /// you stop watching is censored, not a failure, and feeding it in as a
    /// negative teaches the model that everything dies.
    pub fn update(&mut self, f: &Features, graduated: bool) {
        let row = f.to_row();
        let p = sigmoid({
            let mut z = self.bias;
            for i in 0..N_FEATURES {
                z += self.weights[i] * row[i];
            }
            z
        });
        let err = p - if graduated { 1.0 } else { 0.0 };
        for i in 0..N_FEATURES {
            let grad = err * row[i] + self.l2 * self.weights[i];
            self.weights[i] -= self.lr * grad;
        }
        self.bias -= self.lr * err;
        self.trained_on += 1;
        if graduated {
            self.positives += 1;
        }
    }

    /// Observed base rate in the training stream, once there is enough to mean much.
    pub fn base_rate(&self) -> Option<f64> {
        (self.trained_on >= 50).then(|| self.positives as f64 / self.trained_on as f64)
    }

    /// Whether this model is worth writing to disk and reusing.
    ///
    /// A short live run can easily see a couple of graduations and no failures yet,
    /// because failures only settle after a long silence. Persisting that produces
    /// a model that waves everything through on the next start. Both classes must
    /// be present, in enough volume to mean something.
    pub fn is_fit_to_persist(&self) -> bool {
        const MIN_OUTCOMES: u64 = 50;
        self.trained_on >= MIN_OUTCOMES
            && self.positives > 0
            && self.positives < self.trained_on
    }

    /// Weights paired with their names, largest magnitude first — for `explain`.
    pub fn ranked(&self) -> Vec<(&'static str, f64)> {
        let mut v: Vec<_> = FEATURE_NAMES
            .iter()
            .enumerate()
            .map(|(i, n)| (*n, self.weights.get(i).copied().unwrap_or(0.0)))
            .collect();
        v.sort_by(|a, b| b.1.abs().partial_cmp(&a.1.abs()).unwrap_or(std::cmp::Ordering::Equal));
        v
    }

    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let mut m: Model = serde_json::from_str(&raw)?;
        if m.weights.len() != N_FEATURES {
            m.weights.resize(N_FEATURES, 0.0);
        }
        Ok(m)
    }

    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feats(depth_sol: f64, vel_sol: f64, updates: u32) -> Features {
        Features {
            depth_lamports: (depth_sol * 1e9) as u64,
            velocity_lamports_s: vel_sol * 1e9,
            updates,
            age: Duration::from_secs(30),
            smart_money: 0.0,
            unique_buyers: updates,
        }
    }

    #[test]
    fn sigmoid_is_stable_at_extremes() {
        assert!(sigmoid(1000.0) > 0.999);
        assert!(sigmoid(-1000.0) < 0.001);
        assert!(sigmoid(0.0) - 0.5 < f64::EPSILON);
        assert!(sigmoid(-1e308).is_finite());
    }

    #[test]
    fn default_model_is_pessimistic_about_shallow_curves() {
        let m = Model::default();
        let p = m.predict(&feats(0.5, 0.0, 1));
        assert!(p < 0.10, "shallow curve scored {p}");
    }

    #[test]
    fn deeper_curves_score_higher() {
        let m = Model::default();
        let shallow = m.predict(&feats(5.0, 0.0, 5));
        let deep = m.predict(&feats(80.0, 0.0, 5));
        assert!(deep > shallow, "deep {deep} should beat shallow {shallow}");
    }

    #[test]
    fn probabilities_stay_in_range() {
        let m = Model::default();
        for d in [0.0, 1.0, 42.0, 85.0, 200.0] {
            for v in [-50.0, 0.0, 50.0] {
                let p = m.predict(&feats(d, v, 3));
                assert!((0.0..=1.0).contains(&p), "p={p} out of range");
            }
        }
    }

    #[test]
    fn training_moves_prediction_toward_the_label() {
        let mut m = Model::default();
        let f = feats(40.0, 1.0, 10);
        let before = m.predict(&f);
        for _ in 0..200 {
            m.update(&f, true);
        }
        let after = m.predict(&f);
        assert!(after > before, "{after} should exceed {before}");
        assert_eq!(m.trained_on, 200);
        assert_eq!(m.positives, 200);
    }

    #[test]
    fn negative_labels_push_the_other_way() {
        let mut m = Model::default();
        let f = feats(40.0, 1.0, 10);
        let before = m.predict(&f);
        for _ in 0..200 {
            m.update(&f, false);
        }
        assert!(m.predict(&f) < before);
        assert_eq!(m.positives, 0);
    }

    #[test]
    fn the_model_can_separate_two_populations() {
        // Winners are deep and moving; losers are shallow and flat. After training
        // the model must rank them apart - this is the whole point of the thing.
        let mut m = Model::default();
        for _ in 0..400 {
            m.update(&feats(70.0, 2.0, 20), true);
            m.update(&feats(2.0, 0.0, 2), false);
        }
        let win = m.predict(&feats(70.0, 2.0, 20));
        let lose = m.predict(&feats(2.0, 0.0, 2));
        assert!(win > 0.6, "winner scored {win}");
        assert!(lose < 0.2, "loser scored {lose}");
    }

    #[test]
    fn base_rate_needs_enough_samples() {
        let mut m = Model::default();
        assert_eq!(m.base_rate(), None);
        for i in 0..100 {
            m.update(&feats(10.0, 0.0, 3), i % 10 == 0);
        }
        let br = m.base_rate().expect("enough samples");
        assert!((br - 0.10).abs() < 1e-9, "base rate {br}");
    }

    #[test]
    fn weights_stay_finite_under_extreme_input() {
        let mut m = Model::default();
        let wild = Features {
            depth_lamports: u64::MAX,
            velocity_lamports_s: f64::MAX,
            updates: u32::MAX,
            age: Duration::from_secs(u32::MAX as u64),
            smart_money: 1e300,
            unique_buyers: u32::MAX,
        };
        for _ in 0..100 {
            m.update(&wild, true);
        }
        assert!(m.weights.iter().all(|w| w.is_finite()), "{:?}", m.weights);
        assert!(m.bias.is_finite());
        assert!((0.0..=1.0).contains(&m.predict(&wild)));
    }

    #[test]
    fn a_model_with_only_positives_is_not_fit_to_persist() {
        // The exact shape seen on a live run: two graduations, no failures yet.
        let mut m = Model::default();
        for _ in 0..2 {
            m.update(&feats(70.0, 2.0, 20), true);
        }
        assert!(!m.is_fit_to_persist());
    }

    #[test]
    fn a_model_with_only_negatives_is_not_fit_to_persist() {
        let mut m = Model::default();
        for _ in 0..200 {
            m.update(&feats(2.0, 0.0, 2), false);
        }
        assert!(!m.is_fit_to_persist());
    }

    #[test]
    fn a_model_with_too_few_outcomes_is_not_fit_to_persist() {
        let mut m = Model::default();
        for i in 0..20 {
            m.update(&feats(20.0, 1.0, 5), i % 4 == 0);
        }
        assert!(!m.is_fit_to_persist());
    }

    #[test]
    fn a_balanced_well_fed_model_is_fit_to_persist() {
        let mut m = Model::default();
        for i in 0..200 {
            m.update(&feats(20.0, 1.0, 5), i % 10 == 0);
        }
        assert!(m.is_fit_to_persist());
    }

    #[test]
    fn ranked_orders_by_magnitude() {
        let mut m = Model::default();
        m.weights = vec![0.1, -5.0, 0.2, 0.0, 1.0, 0.0];
        let r = m.ranked();
        assert_eq!(r[0].0, "velocity");
        assert_eq!(r[1].0, "smart_money");
    }

    #[test]
    fn model_round_trips_through_disk() {
        let dir = std::env::temp_dir().join("sniper_model_test");
        let path = dir.join("m.json");
        let mut m = Model::default();
        m.update(&feats(50.0, 1.0, 8), true);
        m.save(&path).unwrap();
        let back = Model::load(&path).unwrap();
        assert_eq!(back.trained_on, 1);
        assert_eq!(back.weights.len(), N_FEATURES);
        let _ = std::fs::remove_file(&path);
    }
}
