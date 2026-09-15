//! Novelty and the interestingness score (ADR-0012 §4; docs/04 §2).
//!
//! S = w1·clip(SNR_dB/20, 0, 1) + w2·novelty + w3·H(p̂_class) + w4·1[decoder] + w5·periodicity
//! − w6·boring_prior.
//!
//! **C12 computes S; C04 consumes it** through [`InterestingnessProvider`], which hands the
//! scheduler immutable [`CandidateSet`] snapshots. The scheduler never recomputes a component.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use super::baseline::{Maturity, SiteKey};
use super::{ValidationError, ensure, ensure_in, ensure_opt_in, ensure_schema};
use crate::ids::{EmitterId, TrackId};
use crate::region::FreqRange;
use crate::time::Timestamp;

/// User-tunable score weights (§4.2). Versioned: every change stores a new version and every
/// candidate records the version it was scored with.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScoreWeights {
    /// Version (1 = defaults).
    pub version: u32,
    /// w1: SNR.
    pub snr: f64,
    /// w2: novelty.
    pub novelty: f64,
    /// w3: class uncertainty (normalised entropy).
    pub class_entropy: f64,
    /// w4: a decoder/recipe is available.
    pub decoder: f64,
    /// w5: periodicity strength.
    pub periodicity: f64,
    /// w6: boring prior (subtracted).
    pub boring: f64,
}

impl Default for ScoreWeights {
    /// Novelty-led defaults (unknown signals are the priority): 1, 2, 1, 0.5, 0.5, 1.
    fn default() -> Self {
        Self {
            version: 1,
            snr: 1.0,
            novelty: 2.0,
            class_entropy: 1.0,
            decoder: 0.5,
            periodicity: 0.5,
            boring: 1.0,
        }
    }
}

impl ScoreWeights {
    /// Largest attainable S (all positive terms at 1).
    pub fn max_score(&self) -> f64 {
        self.snr + self.novelty + self.class_entropy + self.decoder + self.periodicity
    }

    /// Each weight finite in `[0, 10]`; at least one positive term weight.
    pub fn validate(&self) -> Result<(), ValidationError> {
        for (v, f) in [
            (self.snr, "weights.snr"),
            (self.novelty, "weights.novelty"),
            (self.class_entropy, "weights.class_entropy"),
            (self.decoder, "weights.decoder"),
            (self.periodicity, "weights.periodicity"),
            (self.boring, "weights.boring"),
        ] {
            ensure_in(v, 0.0, 10.0, f)?;
        }
        ensure(
            self.max_score() > 0.0,
            "weights",
            "at least one positive term must be weighted",
        )?;
        ensure(self.version >= 1, "weights.version", "starts at 1")
    }
}

/// The measured inputs of S for one candidate (§4.1), each already in its term's range.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScoreComponents {
    /// SNR above the local floor, dB; `None` when unmeasured (term 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snr_db: Option<f64>,
    /// Novelty, 0–1 ([`NoveltyScore::novelty`]).
    pub novelty: f64,
    /// Normalised class entropy H/ln K, 0–1; `None` = never classified, scored as 1 (unknown is
    /// maximally uncertain, and unknown signals are the priority).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class_entropy: Option<f64>,
    /// A recipe/decoder matches the measured parameters.
    pub decoder_available: bool,
    /// Periodicity confidence, 0–1; `None` = no period found (term 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub periodicity: Option<f64>,
    /// Boring prior, 0–1 (§4.3).
    pub boring_prior: f64,
}

impl ScoreComponents {
    /// Checks term ranges.
    pub fn validate(&self) -> Result<(), ValidationError> {
        ensure_opt_in(self.snr_db, -50.0, 200.0, "components.snr_db")?;
        ensure_in(self.novelty, 0.0, 1.0, "components.novelty")?;
        ensure_opt_in(self.class_entropy, 0.0, 1.0, "components.class_entropy")?;
        ensure_opt_in(self.periodicity, 0.0, 1.0, "components.periodicity")?;
        ensure_in(self.boring_prior, 0.0, 1.0, "components.boring_prior")
    }
}

/// S for `c` under `w` (docs/04 §2).
pub fn interestingness(w: &ScoreWeights, c: &ScoreComponents) -> f64 {
    let snr = c.snr_db.map_or(0.0, |s| (s / 20.0).clamp(0.0, 1.0));
    w.snr * snr
        + w.novelty * c.novelty
        + w.class_entropy * c.class_entropy.unwrap_or(1.0)
        + w.decoder * f64::from(u8::from(c.decoder_available))
        + w.periodicity * c.periodicity.unwrap_or(0.0)
        - w.boring * c.boring_prior
}

/// S normalised by [`ScoreWeights::max_score`], clipped to `[0, 1]`: the bandit prior and the
/// `novelty <score>` reason value.
pub fn normalised(w: &ScoreWeights, s: f64) -> f64 {
    let m = w.max_score();
    if m > 0.0 {
        (s / m).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Maps a robust z-score to novelty: 0 below `z_min`, 1 at or above `z_sat`, linear between
/// (defaults 3 and 10).
pub fn novelty_from_z(z: f64, z_min: f64, z_sat: f64) -> f64 {
    if !z.is_finite() || z_sat <= z_min {
        return 0.0;
    }
    ((z - z_min) / (z_sat - z_min)).clamp(0.0, 1.0)
}

/// Novelty of seeing `k` new emitters in `observed_s` seconds when the site's baseline rate is
/// `rate_per_s` new emitters per observed second (§4.4): the Poisson upper tail
/// p = P(X ≥ k | μ = rate·T), mapped as clip(−log10 p / 6, 0, 1). Normalised by observation time
/// by construction: more observation raises the expected count, so the same k is less novel.
pub fn new_emitter_novelty(k: u64, rate_per_s: f64, observed_s: f64) -> f64 {
    if k == 0 {
        return 0.0;
    }
    let mu = (rate_per_s.max(0.0) * observed_s.max(0.0)).max(1e-12);
    // P(X ≥ k) = 1 − Σ_{i<k} e^−μ μ^i / i!, in log space for small tails.
    let k = k.min(200);
    let mut term = (-mu).exp();
    let mut cdf = 0.0;
    for i in 0..k {
        if i > 0 {
            term *= mu / i as f64;
        }
        cdf += term;
    }
    let tail = if cdf < 0.999_999 {
        1.0 - cdf
    } else {
        // Leading term of the tail: e^−μ μ^k / k!.
        let ln_fact: f64 = (1..=k).map(|i| (i as f64).ln()).sum();
        (-mu + k as f64 * mu.ln() - ln_fact).exp()
    };
    (-(tail.max(1e-300)).log10() / 6.0).clamp(0.0, 1.0)
}

/// Novelty of one candidate against its baseline (§4.4), with the evidence kept.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoveltyScore {
    /// Combined novelty, 0–1: the maximum of the available component novelties (a candidate is
    /// novel if any aspect is). 0 when immature or provenance-explained.
    pub novelty: f64,
    /// Level z-score vs the pool (winsorised mean/σ, σ floored at 1 dB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level_z: Option<f64>,
    /// Occupancy z-score vs the pool (binomial, effective samples).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occupancy_z: Option<f64>,
    /// [`new_emitter_novelty`] of the candidate's region.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_emitter: Option<f64>,
    /// Observed seconds behind the current value.
    pub observed_s: f64,
    /// Baseline maturity.
    pub maturity: Maturity,
    /// A front-end/provenance step explains the change (novelty forced to 0).
    pub provenance_explained: bool,
}

impl NoveltyScore {
    /// Checks ranges and the zero rules.
    pub fn validate(&self) -> Result<(), ValidationError> {
        ensure_in(self.novelty, 0.0, 1.0, "novelty.novelty")?;
        ensure_opt_in(self.new_emitter, 0.0, 1.0, "novelty.new_emitter")?;
        ensure_in(self.observed_s, 0.0, f64::MAX, "novelty.observed_s")?;
        ensure(
            self.maturity.is_mature() || self.novelty == 0.0,
            "novelty.novelty",
            "must be 0 while the baseline is immature",
        )?;
        ensure(
            !self.provenance_explained || self.novelty == 0.0,
            "novelty.novelty",
            "must be 0 when provenance explains the change",
        )
    }
}

/// What a candidate refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum CandidateSubject {
    /// An inventory emitter.
    Emitter {
        /// Id.
        id: EmitterId,
    },
    /// A track not (yet) in the inventory.
    Track {
        /// Id.
        id: TrackId,
    },
    /// Baseline cells with level/occupancy novelty but no detection.
    Cells {
        /// Pyramid scheme.
        scheme: u16,
        /// First level-0 cell.
        lo_cell: i64,
        /// One past the last cell.
        hi_cell: i64,
    },
}

/// One ranked candidate (§4.5).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    /// Subject.
    pub subject: CandidateSubject,
    /// Occupied extent, Hz.
    pub freq: FreqRange,
    /// S.
    pub score: f64,
    /// S normalised to 0–1 ([`normalised`]).
    pub score_norm: f64,
    /// Inputs of S.
    pub components: ScoreComponents,
    /// Novelty evidence.
    pub novelty: NoveltyScore,
    /// Share of member detections with a suspect flag.
    pub suspect_fraction: f64,
    /// Suspect and not yet trust-tested: the scheduler may spend one verification dwell on it and
    /// nothing else (§5.3).
    pub needs_verification: bool,
    /// Expected burst interval, s (dwell sizing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_interval_s: Option<f64>,
    /// Minimum on/off time, s (revisit ≤ half of it for complete capture).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_on_off_s: Option<f64>,
    /// Next expected burst.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_burst_eta: Option<Timestamp>,
}

impl Candidate {
    /// Checks ranges and that `score` matches the components under `w`.
    pub fn validate(&self, w: &ScoreWeights) -> Result<(), ValidationError> {
        self.components.validate()?;
        self.novelty.validate()?;
        ensure(
            self.freq.hi_hz > self.freq.lo_hz,
            "freq",
            "must be a positive extent",
        )?;
        ensure(
            (self.score - interestingness(w, &self.components)).abs() <= 1e-9,
            "score",
            "must equal interestingness(weights, components)",
        )?;
        ensure(
            (self.score_norm - normalised(w, self.score)).abs() <= 1e-9,
            "score_norm",
            "must equal normalised(weights, score)",
        )?;
        ensure(
            (self.components.novelty - self.novelty.novelty).abs() <= 1e-12,
            "components.novelty",
            "must equal novelty.novelty",
        )?;
        ensure_in(self.suspect_fraction, 0.0, 1.0, "suspect_fraction")?;
        ensure_opt_in(self.expected_interval_s, 0.0, 1e7, "expected_interval_s")?;
        ensure_opt_in(self.min_on_off_s, 0.0, 1e7, "min_on_off_s")
    }
}

/// An immutable, ranked snapshot published by C12 (§4.5).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateSet {
    /// [`super::ATTENTION_SCHEMA_VERSION`].
    pub schema: u32,
    /// Monotonic snapshot version (the provider's [`InterestingnessProvider::version`]).
    pub version: u64,
    /// Scoring time.
    pub t: Timestamp,
    /// Site key scored under.
    pub site: SiteKey,
    /// Weights used.
    pub weights: ScoreWeights,
    /// Candidates, `score` descending.
    pub candidates: Vec<Candidate>,
}

impl CandidateSet {
    /// An empty set (version 0): the bandit explores uniformly.
    pub fn empty(t: Timestamp) -> Self {
        Self {
            schema: super::ATTENTION_SCHEMA_VERSION,
            version: 0,
            t,
            site: SiteKey::Unassigned,
            weights: ScoreWeights::default(),
            candidates: Vec::new(),
        }
    }

    /// Checks schema, weights, every candidate and the ordering.
    pub fn validate(&self) -> Result<(), ValidationError> {
        ensure_schema(self.schema)?;
        self.weights.validate()?;
        for c in &self.candidates {
            c.validate(&self.weights)?;
        }
        ensure(
            self.candidates.windows(2).all(|p| p[0].score >= p[1].score),
            "candidates",
            "must be sorted by score, descending",
        )
    }
}

/// C12 → C04 seam (§4.6). Implementations are cheap to query: the scheduler calls
/// [`version`](Self::version) at every decision boundary and [`snapshot`](Self::snapshot) only
/// when the version moved; neither may block on scoring work.
pub trait InterestingnessProvider: Send + Sync {
    /// Version of the latest snapshot (0 = nothing published).
    fn version(&self) -> u64;
    /// The latest snapshot.
    fn snapshot(&self) -> Arc<CandidateSet>;
}

/// A provider that never has candidates: the bandit explores uniformly.
#[derive(Clone, Debug)]
pub struct EmptyInterestingness(Arc<CandidateSet>);

impl Default for EmptyInterestingness {
    fn default() -> Self {
        Self(Arc::new(CandidateSet::empty(Timestamp::UNIX_EPOCH)))
    }
}

impl InterestingnessProvider for EmptyInterestingness {
    fn version(&self) -> u64 {
        0
    }
    fn snapshot(&self) -> Arc<CandidateSet> {
        Arc::clone(&self.0)
    }
}

/// A publish/subscribe provider: the producer [`publish`](Self::publish)es snapshots, the
/// scheduler reads them. The stub T-120 codes against before T-119 (tests and the simulator publish
/// hand-built or detection-count sets) and the handoff the real C12 thread uses afterwards.
#[derive(Debug)]
pub struct SharedInterestingness {
    version: AtomicU64,
    latest: Mutex<Arc<CandidateSet>>,
}

impl Default for SharedInterestingness {
    fn default() -> Self {
        Self {
            version: AtomicU64::new(0),
            latest: Mutex::new(Arc::new(CandidateSet::empty(Timestamp::UNIX_EPOCH))),
        }
    }
}

impl SharedInterestingness {
    /// Validates and publishes `set`, assigning it the next version; returns that version.
    pub fn publish(&self, mut set: CandidateSet) -> Result<u64, ValidationError> {
        let mut latest = self.latest.lock().unwrap_or_else(|p| p.into_inner());
        set.version = latest.version + 1;
        set.validate()?;
        let v = set.version;
        *latest = Arc::new(set);
        self.version.store(v, Ordering::Release);
        Ok(v)
    }
}

impl InterestingnessProvider for SharedInterestingness {
    fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }
    fn snapshot(&self) -> Arc<CandidateSet> {
        Arc::clone(&self.latest.lock().unwrap_or_else(|p| p.into_inner()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attention::baseline::BaselineResolution;

    fn mature() -> Maturity {
        Maturity::Mature {
            resolution: BaselineResolution::AllHours,
        }
    }

    fn candidate(w: &ScoreWeights, novelty: f64, boring: f64) -> Candidate {
        let components = ScoreComponents {
            snr_db: Some(30.0),
            novelty,
            class_entropy: Some(0.5),
            decoder_available: true,
            periodicity: None,
            boring_prior: boring,
        };
        let score = interestingness(w, &components);
        Candidate {
            subject: CandidateSubject::Cells {
                scheme: 1,
                lo_cell: 1,
                hi_cell: 2,
            },
            freq: FreqRange::new(1.0, 2.0),
            score,
            score_norm: normalised(w, score),
            components,
            novelty: NoveltyScore {
                novelty,
                level_z: Some(8.0),
                occupancy_z: None,
                new_emitter: None,
                observed_s: 100.0,
                maturity: mature(),
                provenance_explained: false,
            },
            suspect_fraction: 0.0,
            needs_verification: false,
            expected_interval_s: None,
            min_on_off_s: None,
            next_burst_eta: None,
        }
    }

    #[test]
    fn score_formula() {
        let w = ScoreWeights::default();
        let c = candidate(&w, 0.5, 0.25);
        // 1·clip(30/20) + 2·0.5 + 1·0.5 + 0.5·1 + 0.5·0 − 1·0.25 = 2.75
        assert!((c.score - 2.75).abs() < 1e-12);
        assert!((c.score_norm - 2.75 / 5.0).abs() < 1e-12);
        let unclassified = ScoreComponents {
            class_entropy: None,
            ..c.components
        };
        assert!(
            (interestingness(&w, &unclassified) - 3.25).abs() < 1e-12,
            "unknown class = max entropy"
        );
        assert_eq!(normalised(&w, -3.0), 0.0);
    }

    #[test]
    fn weights_validate_and_reject_unknown_fields() {
        ScoreWeights::default().validate().unwrap();
        let neg = ScoreWeights {
            novelty: -1.0,
            ..ScoreWeights::default()
        };
        assert_eq!(neg.validate().unwrap_err().field, "weights.novelty");
        let zero = ScoreWeights {
            snr: 0.0,
            novelty: 0.0,
            class_entropy: 0.0,
            decoder: 0.0,
            periodicity: 0.0,
            ..ScoreWeights::default()
        };
        assert_eq!(zero.validate().unwrap_err().field, "weights");
        let mut v = serde_json::to_value(ScoreWeights::default()).unwrap();
        v["w7"] = serde_json::json!(1.0);
        assert!(serde_json::from_value::<ScoreWeights>(v).is_err());
    }

    #[test]
    fn novelty_mappings() {
        assert_eq!(novelty_from_z(2.0, 3.0, 10.0), 0.0);
        assert_eq!(novelty_from_z(6.5, 3.0, 10.0), 0.5);
        assert_eq!(novelty_from_z(f64::NAN, 3.0, 10.0), 0.0);
        assert_eq!(new_emitter_novelty(0, 1e-3, 3600.0), 0.0);
        // Expected 3.6 new emitters in an hour: seeing 3 is not novel; seeing 30 is.
        assert!(new_emitter_novelty(3, 1e-3, 3600.0) < 0.1);
        assert!(new_emitter_novelty(30, 1e-3, 3600.0) > 0.9);
        // Same count, ten times the observation: less novel (observation-time normalisation).
        assert!(new_emitter_novelty(5, 1e-4, 36_000.0) < new_emitter_novelty(5, 1e-4, 3600.0));
    }

    #[test]
    fn novelty_is_zero_when_immature_or_explained() {
        let w = ScoreWeights::default();
        let mut c = candidate(&w, 0.5, 0.0);
        c.validate(&w).unwrap();
        c.novelty.maturity = Maturity::Immature { observed_s: 10.0 };
        assert!(c.novelty.validate().is_err());
        c.novelty.maturity = mature();
        c.novelty.provenance_explained = true;
        assert!(c.novelty.validate().is_err());
    }

    #[test]
    fn shared_provider_publishes_validated_sorted_snapshots() {
        let w = ScoreWeights::default();
        let p = SharedInterestingness::default();
        assert_eq!(p.version(), 0);
        assert!(p.snapshot().candidates.is_empty());
        let mut set = CandidateSet::empty(Timestamp::from_unix_nanos(1));
        set.candidates = vec![candidate(&w, 1.0, 0.0), candidate(&w, 0.1, 0.0)];
        assert_eq!(p.publish(set.clone()).unwrap(), 1);
        assert_eq!(p.version(), 1);
        assert_eq!(p.snapshot().version, 1);
        set.candidates.reverse();
        assert_eq!(p.publish(set).unwrap_err().field, "candidates");
        assert_eq!(p.version(), 1, "a refused snapshot is not published");
        let dynp: &dyn InterestingnessProvider = &EmptyInterestingness::default();
        assert_eq!(dynp.version(), 0);
    }
}
