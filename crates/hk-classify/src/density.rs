//! Class-conditional densities and their χ² plausibilities (ADR-0016 §4.3–4.4).
//!
//! A **leaf is a class**, not a family: `wfm` and `ssb` are both analog but look nothing alike, and
//! one Gaussian over both would be so wide that everything fits it. Each class carries a
//! **diagonal Gaussian with shrinkage** over the `features@1` dimensions, fitted on the synthetic
//! **dev** grid ([`crate::synth::DEV_SEEDS`]) and shipped as versioned data
//! (`data/densities-1.json`, embedded at build time). A family's plausibility is its **best**
//! class: `p(x ∣ family) = max_class p(x ∣ class)`.
//!
//! Scoring uses only the dimensions the snippet measured, so an abstaining feature costs a
//! dimension rather than inventing a value. The score is **not** a raw density: `d² = Σ z²` over
//! `k` used dimensions is read through `P(χ²_k ≥ d²)` ([`crate::openset`]), which is
//! dimension-normalised — necessary because two families rarely score on the same subset — and
//! directly gives the open-set score `1 − max_c L_c`. Per-`z` clamping keeps one wild feature (a
//! spur, a clipped sample run) from vetoing an otherwise good class.
//!
//! **Fitting is dev-only** (`cargo run -p hk-classify --bin fit-densities`). The acceptance seeds
//! are never fitted, so the accuracy the tests report is blind (docs/10 §3.2).

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::features::{FEATURE_NAMES, Features};
use crate::openset::chi2_sf;

/// Version of the shipped density file.
pub const DENSITY_VERSION: u32 = 1;

/// Largest |z| one feature may contribute (robustness to a single wild feature).
pub const Z_CLAMP: f64 = 6.0;

/// Smallest fraction of the fitting samples a feature must be present in to enter a class's
/// density: a feature that usually abstains would otherwise make the class's `k` jump around.
pub const MIN_PRESENCE: f64 = 0.8;

/// Reference dimensionality of the evidence score: two classes whose mean squared z differs by 1
/// stand in the ratio `e^(K_REF/2)` ≈ 55:1, the likelihood ratio a Gaussian model would give over
/// [`K_REF`] independent dimensions. It makes the ranking independent of how many features each
/// class happens to use — the χ² tail cannot do that job, because it saturates near 1 for every
/// decent fit and so cannot separate a good class from a very good one.
pub const K_REF: f64 = 8.0;

/// Fallback for a class fitted on too few samples to take a 95th percentile.
const DEFAULT_M_P95: f64 = 2.0;

/// Fallback second-largest-|z| percentile, for a class fitted on too few samples to take one.
/// Deliberately permissive: an unfitted class should not start rejecting its own members.
const DEFAULT_Z2_P99: f64 = 4.0;

/// One dimension of a class's density.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dim {
    /// Feature name (one of [`FEATURE_NAMES`]).
    pub feature: String,
    /// Fitted mean.
    pub mean: f64,
    /// Fitted standard deviation, floored (shrinkage).
    pub sigma: f64,
    /// Relative weight in `d²` (1 unless a dimension is deliberately down-weighted).
    pub weight: f64,
}

/// One class's conditional density.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassDensity {
    /// `hk-mod@1` class label (`2fsk`, `wfm`, …).
    pub class: String,
    /// The family it belongs to.
    pub family: String,
    /// Dimensions, in [`FEATURE_NAMES`] order.
    pub dims: Vec<Dim>,
    /// 95th percentile of the mean squared z over the class's **dev** samples: how badly a genuine
    /// member of this class may still fit. It calibrates the open-set score (see
    /// [`DensityModel::score_class`]).
    pub m_p95: f64,
    /// 99th percentile of the **second-largest |z|** over the class's dev samples (T-248).
    ///
    /// `m_p95` alone cannot reject an unlisted member of a family: `m` is a mean over ~27
    /// dimensions, and an unlisted member matches its listed siblings on almost all of them and
    /// differs on one to three, so the evidence that would reject it is averaged away. Measured on
    /// the held-out generators: VSB-AM sits at m 1.536 against `am`'s m_p95 of 2.468, and
    /// π/4-DQPSK at 1.727 against `qpsk`'s 1.998 — both comfortably inside, while sitting at z
    /// +3.28 and −4.68 on a single dimension.
    ///
    /// The **largest** |z| does not work either, and the dev distribution says why: genuine members
    /// routinely have one wild dimension (p99 of max |z| is at the [`Z_CLAMP`] of 6.0 for eleven of
    /// the twenty-one classes), because several `features@1` dimensions are heavy-tailed. That is
    /// the same fact [`Z_CLAMP`] exists for. The *second*-largest is the order statistic that
    /// survives it: one wild feature is what a genuine member has, two is what a non-member has.
    /// Measured dev p95 against held-out p95 — `am` 2.47 vs 3.58, `qpsk` 2.60 vs 4.04, `qam16`
    /// 2.19 vs 3.06 — a consistent gap where both `m` and max |z| overlap.
    ///
    /// **The 99th percentile, not the 95th.** A percentile of a max-type order statistic is
    /// exceeded by exactly that fraction of genuine members *by construction*, so a p95 tail
    /// penalises 5 % of real signals — and because the term is combined with `min`, each of those
    /// is a claim withheld. Measured on the acceptance grid, a p95 tail cost known-family top-1
    /// **0.9070 → 0.8770**, through the ADR-0016 §7 floor of 0.90, to buy held-out recall
    /// 0.7778 → 0.8788. The 99th is the operating point at which a rejection means "two dimensions
    /// out, which a genuine member essentially never is". The percentile is a model parameter
    /// fitted on the **dev** split, like every other number in this file.
    ///
    /// `0.0` means "not fitted", and [`DensityModel::score_class`] then applies no tail term at
    /// all, so an older density file keeps its previous behaviour exactly.
    #[serde(default)]
    pub z2_p99: f64,
    /// Fitting samples behind it (provenance).
    pub n: usize,
}

/// The density model of one taxonomy version.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DensityModel {
    /// [`DENSITY_VERSION`].
    pub version: u32,
    /// Taxonomy the labels belong to.
    pub taxonomy: String,
    /// Feature-set version the dimensions were computed with.
    pub features_version: u32,
    /// How the file was produced (provenance, for the report).
    pub fitted_on: String,
    /// Per-class densities.
    pub classes: Vec<ClassDensity>,
}

/// How well a family (through its best class) explains a feature vector.
#[derive(Clone, Debug, PartialEq)]
pub struct FamilyScore {
    /// Squared Mahalanobis distance over the used dimensions.
    pub d2: f64,
    /// Dimensions used.
    pub k: usize,
    /// Mean squared z, `d²/k`: how far the snippet sits from the class per dimension.
    pub m: f64,
    /// Relative evidence for ranking: `exp(−K_REF·m/2)`, comparable across classes that scored on
    /// different numbers of dimensions.
    ///
    /// It **underflows to exactly 0** for any poor fit, which is fine for a ranking (the winner is
    /// never the underflowing one) but destroys a comparison between two poor fits. Use
    /// [`FamilyScore::log_evidence`] where their *ratio* matters.
    pub evidence: f64,
    /// `ln(evidence)`, kept unexponentiated so two classes can be compared however badly both fit.
    ///
    /// A ratio taken from [`FamilyScore::evidence`] is `0/0` once both underflow; a difference of
    /// logs is exact over the whole range. The within-family class call
    /// ([`crate::tree::class_guess`]) needs that, because a class whose evidence underflowed would
    /// otherwise get probability exactly zero — an unrecoverable score no later stage can move
    /// (T-243).
    pub log_evidence: f64,
    /// Calibrated in-distribution plausibility, 0–1: 1 for anything fitting at least as well as
    /// the class's dev 95th percentile, falling away beyond it. The open-set score is
    /// `1 − max plausibility`.
    pub plausibility: f64,
    /// The class that fitted best.
    pub class: String,
    /// The dimension that fits worst, with its |z| (explanation and debugging).
    pub worst: Option<(String, f64)>,
}

const BUILTIN: &str = include_str!("../data/densities-1.json");
const BUILTIN_BELOW_GATE: &str = include_str!("../data/densities-below-gate-1.json");

impl DensityModel {
    /// The shipped model (parsed once), fitted at and above each family's SNR gate: the one that
    /// ranks claimable families and calibrates the open set.
    pub fn builtin() -> &'static DensityModel {
        static MODEL: OnceLock<DensityModel> = OnceLock::new();
        MODEL.get_or_init(|| {
            let m: DensityModel =
                serde_json::from_str(BUILTIN).expect("data/densities-1.json parses");
            m.validate().expect("data/densities-1.json is valid");
            m
        })
    }

    /// The companion model fitted **below** each family's gate (`fit-densities` writes both).
    ///
    /// It answers one question only: could this snippet be a family whose SNR gate held it back?
    /// [`crate::classifier`] turns that plausibility into `unknown` mass and never into a claim, so
    /// this model can only make the classifier *less* confident. It exists because
    /// [`Self::builtin`] is fitted at and above the gate, which makes scoring a family below its
    /// own gate an extrapolation — and that is exactly where this question is asked.
    pub fn builtin_below_gate() -> &'static DensityModel {
        static MODEL: OnceLock<DensityModel> = OnceLock::new();
        MODEL.get_or_init(|| {
            let m: DensityModel = serde_json::from_str(BUILTIN_BELOW_GATE)
                .expect("data/densities-below-gate-1.json parses");
            m.validate()
                .expect("data/densities-below-gate-1.json is valid");
            m
        })
    }

    /// The density of one class.
    pub fn class(&self, class: &str) -> Option<&ClassDensity> {
        self.classes.iter().find(|c| c.class == class)
    }

    /// The classes fitted for `family`.
    pub fn classes_of<'a>(&'a self, family: &'a str) -> impl Iterator<Item = &'a ClassDensity> {
        self.classes.iter().filter(move |c| c.family == family)
    }

    /// Whether the model can score `family` at all.
    pub fn has_family(&self, family: &str) -> bool {
        self.classes.iter().any(|c| c.family == family)
    }

    /// Structural checks: known version, known feature names, positive sigmas and weights, no
    /// repeated class or dimension.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != DENSITY_VERSION {
            return Err(format!("density version {}", self.version));
        }
        for (i, c) in self.classes.iter().enumerate() {
            if self.classes[..i].iter().any(|o| o.class == c.class) {
                return Err(format!("class {} repeated", c.class));
            }
            if c.dims.is_empty() {
                return Err(format!("class {} has no dimensions", c.class));
            }
            for (j, d) in c.dims.iter().enumerate() {
                if !FEATURE_NAMES.contains(&d.feature.as_str()) {
                    return Err(format!("{}: unknown feature {}", c.class, d.feature));
                }
                if c.dims[..j].iter().any(|o| o.feature == d.feature) {
                    return Err(format!("{}: feature {} repeated", c.class, d.feature));
                }
                if !(d.sigma.is_finite() && d.sigma > 0.0) {
                    return Err(format!("{}/{}: sigma {}", c.class, d.feature, d.sigma));
                }
                if !(d.mean.is_finite() && d.weight.is_finite() && d.weight > 0.0) {
                    return Err(format!("{}/{}: mean or weight", c.class, d.feature));
                }
            }
            if !(c.m_p95.is_finite() && c.m_p95 > 0.0) {
                return Err(format!("{}: m_p95 {}", c.class, c.m_p95));
            }
        }
        Ok(())
    }

    /// Scores one class; `None` when it is unknown to the model or the snippet measured too few of
    /// its dimensions (fewer than half, and at least 3).
    ///
    /// Two numbers come out of one distance, because they answer different questions:
    /// - `evidence` **ranks** the known classes. The χ² tail cannot: it saturates near 1 for every
    ///   decent fit, so a good class and a much better one look alike.
    /// - `plausibility` decides **whether any of them applies at all**, and is calibrated on the
    ///   dev split: a sample fitting as well as the class's dev 95th percentile scores 1.0, and
    ///   the score falls away beyond it. Comparing the raw χ² tail with a threshold would reject
    ///   genuine members — a typical in-class sample sits at `d² ≈ k`, whose tail probability is
    ///   about 0.5 by construction.
    pub fn score_class(&self, class: &str, features: &Features) -> Option<FamilyScore> {
        let c = self.class(class)?;
        let a = accumulate(&c.dims, features)?;
        let m = a.d2 / a.weight;
        let dof = a.weight.round().max(1.0) as usize;
        let reference = chi2_sf(c.m_p95 * dof as f64, dof).max(1e-12);
        // Mean per-dimension log density, `−½z² − ln σ`, dropping the constant. The `ln σ` term is
        // what stops a broad class from winning everything: without it a class fitted loosely has
        // a small z for every input and would absorb its neighbours (measured: `analog`, whose
        // five classes span the widest feature space, took a third of the PSK/QAM and OOK
        // snippets). A tight class has to earn its win, and pays for its own width.
        let log_density = -0.5 * m - a.ln_sigma / a.weight;
        let log_evidence = K_REF * log_density;
        // The per-dimension tail term (T-248), calibrated exactly as `m_p95` is: a χ² tail
        // normalised by the class's own dev 95th percentile, so a member as extreme as the dev p95
        // scores 1.0 and the score falls away beyond it. It is combined with `min`, which makes
        // membership a **conjunction** — a genuine member is typical in its mean *and* in its
        // second-worst dimension — and guarantees the term can only ever lower a plausibility.
        // Nothing downstream can therefore gain a claim from it (see `classifier`).
        let tail = if c.z2_p99 > 0.0 {
            (chi2_sf(a.z2 * a.z2, 1) / chi2_sf(c.z2_p99 * c.z2_p99, 1).max(1e-12)).min(1.0)
        } else {
            1.0
        };
        Some(FamilyScore {
            d2: a.d2,
            k: a.k,
            m,
            evidence: log_evidence.exp(),
            log_evidence,
            plausibility: (chi2_sf(a.d2, dof) / reference).min(1.0).min(tail),
            class: c.class.clone(),
            worst: a.worst,
        })
    }

    /// Scores a family: its best-fitting class (ADR-0016 §4.3).
    pub fn score(&self, family: &str, features: &Features) -> Option<FamilyScore> {
        self.classes_of(family)
            .filter_map(|c| self.score_class(&c.class, features))
            .max_by(|a, b| a.evidence.total_cmp(&b.evidence))
    }

    /// Fits a model from labelled dev-grid vectors `(class, family, features)`: per class, every
    /// feature present in at least [`MIN_PRESENCE`] of its samples, with a shrunk sigma.
    pub fn fit(labelled: &[(String, String, Features)], fitted_on: &str) -> DensityModel {
        let scales = feature_scales(labelled);
        let mut order: Vec<(String, String)> = Vec::new();
        for (class, family, _) in labelled {
            let key = (class.clone(), family.clone());
            if !order.contains(&key) {
                order.push(key);
            }
        }
        let classes = order
            .into_iter()
            .map(|(class, family)| {
                let rows: Vec<&Features> = labelled
                    .iter()
                    .filter(|(c, _, _)| *c == class)
                    .map(|(_, _, v)| v)
                    .collect();
                let dims: Vec<Dim> = FEATURE_NAMES
                    .iter()
                    .enumerate()
                    .filter_map(|(i, name)| {
                        let vals: Vec<f64> = rows.iter().filter_map(|r| r.get(name)).collect();
                        if (vals.len() as f64) < MIN_PRESENCE * rows.len() as f64 || vals.len() < 4
                        {
                            return None;
                        }
                        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
                        let var = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>()
                            / (vals.len() - 1) as f64;
                        // Shrinkage: never trust a dimension to be tighter than 8 % of **the
                        // feature's own scale**, so one over-clean dev cell cannot make the class
                        // brittle on the air. 8 % is the S5 rule of thumb for how far a synthetic
                        // estimate moves on a real HackRF capture (REPORT.md §3.2), not a tuned
                        // value.
                        //
                        // The scale is the feature's spread **across the whole grid**
                        // ([`feature_scales`]), not the class's own mean. Shrinking towards the
                        // mean silently assumes every feature is a positive quantity measured in
                        // its own units, which several of `features@1` are not: `carrier_line_db`
                        // is a dB ratio, `symmetry` is signed and centred on zero, `flatness` and
                        // `c42_norm` are bounded. For those, 8 % of the mean is not 8 % of
                        // anything meaningful, and the floor collapses exactly where it was meant
                        // to protect — measured on the real 915 MHz FSK capture, `gfsk`'s
                        // `carrier_line_db` (mean 1.8 dB, so a 0.14 dB floor) put the burst at
                        // z = +124, and `2fsk`'s `symmetry` (mean ~0) at z = +34, when no dB
                        // measurement on an 8-bit front end is repeatable to 0.14 dB.
                        let sigma = var.sqrt().max(0.08 * scales[i]).max(1e-3);
                        Some(Dim {
                            feature: (*name).to_owned(),
                            mean,
                            sigma,
                            weight: 1.0,
                        })
                    })
                    .collect();
                // How badly a genuine member of this class fits its own density: the dev 95th
                // percentile of the mean squared z. This is the open-set calibration, and it is
                // measured on the dev split only.
                let mut ms: Vec<f64> = rows
                    .iter()
                    .filter_map(|r| accumulate(&dims, r).map(|a| a.d2 / a.weight))
                    .collect();
                ms.sort_by(f64::total_cmp);
                let m_p95 = if ms.len() >= 20 {
                    ms[((ms.len() as f64 * 0.95) as usize).min(ms.len() - 1)].max(0.5)
                } else {
                    DEFAULT_M_P95
                };
                // The same calibration for the tail statistic, on the same dev rows.
                let mut z2s: Vec<f64> = rows
                    .iter()
                    .filter_map(|r| accumulate(&dims, r).map(|a| a.z2))
                    .collect();
                z2s.sort_by(f64::total_cmp);
                let z2_p99 = if z2s.len() >= 20 {
                    z2s[((z2s.len() as f64 * 0.99) as usize).min(z2s.len() - 1)].max(1.0)
                } else {
                    DEFAULT_Z2_P99
                };
                ClassDensity {
                    class,
                    family,
                    dims,
                    m_p95,
                    z2_p99,
                    n: rows.len(),
                }
            })
            .collect();
        DensityModel {
            version: DENSITY_VERSION,
            taxonomy: hk_model::classify::TaxonomyRef::current().to_string(),
            features_version: crate::features::FEATURES_VERSION,
            fitted_on: fitted_on.to_owned(),
            classes,
        }
    }
}

/// Each feature's natural scale over the whole fitting grid, in [`FEATURE_NAMES`] order: the
/// robust spread (median absolute deviation, scaled to a Gaussian σ) of every value the feature
/// took, across all classes.
///
/// This is what a per-class sigma is shrunk towards ([`DensityModel::fit`]). The MAD, rather than
/// the standard deviation, because several features are heavy-tailed across the taxonomy
/// (`gamma_max` spans 1.7 to 300, `mu42_a` 1.0 to 20): one extreme class would otherwise set a
/// floor so wide that every class in that dimension stopped discriminating. A feature the grid
/// never varies gets 0 and keeps the absolute `1e-3` floor.
fn feature_scales(labelled: &[(String, String, Features)]) -> Vec<f64> {
    FEATURE_NAMES
        .iter()
        .map(|name| {
            let mut vals: Vec<f64> = labelled
                .iter()
                .filter_map(|(_, _, f)| f.get(name))
                .collect();
            if vals.len() < 4 {
                return 0.0;
            }
            vals.sort_by(f64::total_cmp);
            let median = vals[vals.len() / 2];
            let mut dev: Vec<f64> = vals.iter().map(|v| (v - median).abs()).collect();
            dev.sort_by(f64::total_cmp);
            1.4826 * dev[dev.len() / 2]
        })
        .collect()
}

/// Accumulates `d²`, the total weight, the dimension count and the worst-fitting dimension over
/// the features that are present; `None` when too few of them are (fewer than half the class's
/// dimensions, and at least 3) — a family is never claimed from a handful of features.
fn accumulate(dims: &[Dim], features: &Features) -> Option<Accumulated> {
    let mut d2 = 0.0;
    let mut weight = 0.0;
    let mut ln_sigma = 0.0;
    let mut k = 0usize;
    let mut worst: Option<(String, f64)> = None;
    // The two largest |z|, for the tail term (T-248). The second is the one that discriminates:
    // see [`ClassDensity::z2_p95`].
    let mut z1 = 0.0_f64;
    let mut z2 = 0.0_f64;
    for dim in dims {
        let Some(x) = features.get(&dim.feature) else {
            continue;
        };
        let z = ((x - dim.mean) / dim.sigma).clamp(-Z_CLAMP, Z_CLAMP);
        d2 += dim.weight * z * z;
        ln_sigma += dim.weight * dim.sigma.ln();
        weight += dim.weight;
        k += 1;
        let az = z.abs();
        if az > z1 {
            z2 = z1;
            z1 = az;
        } else if az > z2 {
            z2 = az;
        }
        if worst.as_ref().is_none_or(|(_, w)| az > *w) {
            worst = Some((dim.feature.clone(), az));
        }
    }
    if k < (dims.len() / 2).max(3) || weight <= 0.0 {
        return None;
    }
    Some(Accumulated {
        d2,
        weight,
        ln_sigma,
        k,
        z2,
        worst,
    })
}

/// What one class's dimensions contributed to a feature vector.
struct Accumulated {
    d2: f64,
    weight: f64,
    /// Σ w·ln σ over the dimensions used.
    ln_sigma: f64,
    k: usize,
    /// Second-largest |z| over the dimensions used ([`ClassDensity::z2_p95`]).
    z2: f64,
    worst: Option<(String, f64)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_model::classify::HK_MOD_V1;

    fn on_the_mean(dims: &[Dim]) -> Features {
        let mut v = Features {
            values: vec![None; FEATURE_NAMES.len()],
            reasons: Vec::new(),
        };
        for d in dims {
            let i = FEATURE_NAMES.iter().position(|n| *n == d.feature).unwrap();
            v.values[i] = Some(d.mean);
        }
        v
    }

    #[test]
    fn the_shipped_model_covers_every_class_except_the_ones_that_abstain() {
        let m = DensityModel::builtin();
        m.validate().unwrap();
        assert_eq!(m.taxonomy, "hk-mod@1");
        assert_eq!(m.features_version, crate::features::FEATURES_VERSION);
        for f in HK_MOD_V1.families {
            // DSSS has no estimator and no generator in M3: the tree denies it outright
            // (ADR-0016 §1), so it is deliberately unfitted.
            if f.name == "dsss" {
                assert!(!m.has_family("dsss"));
                continue;
            }
            assert!(m.has_family(f.name), "no density for {}", f.name);
            for class in f.classes {
                let c = m
                    .class(class)
                    .unwrap_or_else(|| panic!("no density for class {class}"));
                assert_eq!(c.family, f.name);
                assert!(c.dims.len() >= 6, "{class} has {} dims", c.dims.len());
                assert!(c.n >= 20, "{class} fitted on {} samples", c.n);
            }
        }
    }

    #[test]
    fn the_shipped_models_carry_a_fitted_tail_percentile() {
        // `z2_p99` defaults to 0.0 ("not fitted"), which disables the tail term silently. A shipped
        // file that lost it would quietly restore the pre-T-248 open set, so both models are
        // checked rather than trusted.
        for (name, model) in [
            ("densities-1.json", DensityModel::builtin()),
            (
                "densities-below-gate-1.json",
                DensityModel::builtin_below_gate(),
            ),
        ] {
            for c in &model.classes {
                assert!(
                    c.z2_p99 > 0.0,
                    "{name}: {} has no fitted z2_p99 — re-run \
                     `cargo run -p hk-classify --bin fit-densities`",
                    c.class
                );
            }
        }
    }

    #[test]
    fn the_tail_term_can_only_lower_a_plausibility() {
        // The safety property the classifier relies on (T-248): adding the tail term withholds
        // claims, it never creates one. A vector on a class's mean has a second-largest |z| of 0,
        // so the term is exactly 1 and nothing moves; a vector with two wild dimensions is what it
        // is for.
        let m = DensityModel::builtin();
        let c = m.class("am").unwrap();
        let on_mean = on_the_mean(&c.dims);
        let s = m.score_class("am", &on_mean).expect("scored");
        assert!(s.plausibility > 0.99, "{s:?}");
        let mut two_wild = on_mean.clone();
        for d in c.dims.iter().take(2) {
            let i = FEATURE_NAMES.iter().position(|n| *n == d.feature).unwrap();
            two_wild.values[i] = Some(d.mean + 6.0 * d.sigma);
        }
        let wild = m.score_class("am", &two_wild).expect("scored");
        assert!(
            wild.plausibility < s.plausibility,
            "two wild dimensions must not stay fully plausible: {wild:?}"
        );
    }

    #[test]
    fn a_family_scores_as_its_best_class() {
        let m = DensityModel::builtin();
        // A vector sitting on `wfm`'s mean is highly plausible analog, through `wfm`.
        let wfm = m.class("wfm").unwrap();
        let v = on_the_mean(&wfm.dims);
        let s = m.score("analog", &v).expect("scored");
        assert_eq!(s.class, "wfm");
        assert!(s.plausibility > 0.99, "{s:?}");
        assert!(s.d2 < 1e-9 && s.m < 1e-9);
        let far_evidence = s.evidence;
        // Far from every class of the family is implausible, and the evidence collapses.
        let mut far = v.clone();
        for d in &wfm.dims {
            let i = FEATURE_NAMES.iter().position(|n| *n == d.feature).unwrap();
            far.values[i] = Some(d.mean + 30.0 * d.sigma);
        }
        let far_score = m.score("analog", &far).unwrap();
        assert!(far_score.plausibility < 1e-3, "{far_score:?}");
        assert!(
            far_score.evidence < far_evidence * 1e-6,
            "a distant vector must lose by orders of magnitude: {far_score:?}"
        );
        assert!(m.score("dsss", &v).is_none(), "dsss is never scored");
        assert!(m.score("not-a-family", &v).is_none());
    }

    #[test]
    fn scoring_abstains_with_too_few_measured_features() {
        let m = DensityModel::builtin();
        let c = m.class("2fsk").unwrap();
        let mut sparse = Features {
            values: vec![None; FEATURE_NAMES.len()],
            reasons: Vec::new(),
        };
        for d in c.dims.iter().take(2) {
            let i = FEATURE_NAMES.iter().position(|n| *n == d.feature).unwrap();
            sparse.values[i] = Some(d.mean);
        }
        assert!(m.score_class("2fsk", &sparse).is_none());
        assert!(m.score("fsk", &sparse).is_none());
    }

    #[test]
    fn fitting_keeps_only_features_that_are_usually_present() {
        let mut rows = Vec::new();
        for i in 0..10 {
            let mut f = Features {
                values: vec![None; FEATURE_NAMES.len()],
                reasons: Vec::new(),
            };
            f.values[0] = Some(1.0 + 0.01 * f64::from(i));
            // Present in 2 of 10 samples only: dropped.
            if i < 2 {
                f.values[1] = Some(5.0);
            }
            rows.push(("2fsk".to_owned(), "fsk".to_owned(), f));
        }
        let m = DensityModel::fit(&rows, "unit test");
        let c = m.class("2fsk").unwrap();
        assert_eq!(c.dims.len(), 1, "{:?}", c.dims);
        assert_eq!(c.dims[0].feature, FEATURE_NAMES[0]);
        assert!(c.dims[0].sigma > 0.0);
        assert_eq!((c.n, c.family.as_str()), (10, "fsk"));
        // Too few samples for a 95th percentile: the default stands in.
        assert_eq!(c.m_p95, DEFAULT_M_P95);
        m.validate().unwrap();
    }
}
