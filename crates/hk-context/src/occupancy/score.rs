//! Interestingness scoring and candidate publication (T-119, ADR-0012 §4.1–§4.6).
//!
//! C12 turns measured [`CandidateInput`]s into a ranked, validated `CandidateSet` under the
//! weights in force and publishes it through `SharedInterestingness` ([`Scorer`]): at most every
//! 10 s of sample clock (60 s in low-power mode), and only when the set changed.
//!
//! The boring prior ([`boring_prior`]) is measured first; a band-plan allocation suggestion adds at
//! most [`ALLOCATION_BORING_MAX`], only after the emitter was blindly discovered and
//! characterised, and never for an off-raster/mismatched emitter (§4.3; docs/04 §2).

use hk_model::attention::baseline::SiteKey;
use hk_model::attention::score::{
    Candidate, CandidateSet, CandidateSubject, NoveltyScore, ScoreComponents, ScoreWeights,
    SharedInterestingness, interestingness, normalised,
};
use hk_model::attention::{ATTENTION_SCHEMA_VERSION, ValidationError};
use hk_model::region::FreqRange;
use hk_model::time::Timestamp;

/// Largest share of the boring prior a C17 allocation suggestion may contribute (§4.3).
pub const ALLOCATION_BORING_MAX: f64 = 0.3;
/// Re-scoring interval, sample-clock seconds.
pub const RESCORE_S: f64 = 10.0;
/// Re-scoring interval in low-power mode.
pub const RESCORE_LOW_POWER_S: f64 = 60.0;
/// FCO above which a subject is "always on" (in every mature pool).
pub const ALWAYS_ON_FCO: f64 = 0.95;
/// Novelty below which a subject is "stable".
pub const STABLE_NOVELTY: f64 = 0.1;
/// Revisits needed to call a subject stable.
pub const STABLE_MIN_REVISITS: usize = 10;
/// Class entropy below which an emitter is well characterised.
pub const CHARACTERISED_ENTROPY: f64 = 0.2;

/// Measured evidence behind the boring prior (§4.3).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BoringEvidence {
    /// FCO > 0.95 in every mature pool ([`always_on`]).
    pub always_on: bool,
    /// Novelty < 0.1 over ≥ 10 revisits ([`stable`]).
    pub stable: bool,
    /// Class entropy < 0.2 or valid decodes ([`well_characterised`]).
    pub well_characterised: bool,
    /// The user tagged the emitter or a covering Selection "boring".
    pub user_boring: bool,
    /// A C17 allocation suggestion fits the measured emission.
    pub allocation_suggested: bool,
    /// The emitter was found by blind detection and characterised (the suggestion may count).
    pub blindly_characterised: bool,
    /// The emission mismatches the allocation (off raster, wrong width/mode): zeroes the C17 part.
    pub allocation_mismatch: bool,
}

/// True when `pool_fcos` (one per mature pool) is non-empty and every value exceeds 0.95.
pub fn always_on(pool_fcos: &[f64]) -> bool {
    !pool_fcos.is_empty() && pool_fcos.iter().all(|f| *f > ALWAYS_ON_FCO)
}

/// True over ≥ 10 revisits all with novelty < 0.1.
pub fn stable(recent_novelty: &[f64]) -> bool {
    recent_novelty.len() >= STABLE_MIN_REVISITS
        && recent_novelty.iter().all(|n| *n < STABLE_NOVELTY)
}

/// True when classified with entropy < 0.2, or valid decodes exist.
pub fn well_characterised(class_entropy: Option<f64>, valid_decodes: bool) -> bool {
    valid_decodes || class_entropy.is_some_and(|h| h < CHARACTERISED_ENTROPY)
}

/// The boring prior, 0–1: the user tag alone is 1; otherwise always-on 0.4 + stable 0.15 + well
/// characterised 0.15 (measured, ≤ 0.7) plus at most 0.3 from an allocation suggestion.
pub fn boring_prior(e: &BoringEvidence) -> f64 {
    if e.user_boring {
        return 1.0;
    }
    let measured = 0.4 * f64::from(u8::from(e.always_on))
        + 0.15 * f64::from(u8::from(e.stable))
        + 0.15 * f64::from(u8::from(e.well_characterised));
    let allocation = if e.allocation_suggested && e.blindly_characterised && !e.allocation_mismatch
    {
        ALLOCATION_BORING_MAX
    } else {
        0.0
    };
    (measured + allocation).clamp(0.0, 1.0)
}

/// Everything C12 measured about one candidate.
#[derive(Clone, Debug, PartialEq)]
pub struct CandidateInput {
    /// Subject.
    pub subject: CandidateSubject,
    /// Occupied extent, Hz.
    pub freq: FreqRange,
    /// SNR above the local floor, dB.
    pub snr_db: Option<f64>,
    /// Novelty evidence.
    pub novelty: NoveltyScore,
    /// Normalised class entropy (`None` = never classified = maximally uncertain).
    pub class_entropy: Option<f64>,
    /// A recipe matches the measured parameters.
    pub decoder_available: bool,
    /// Period confidence.
    pub periodicity: Option<f64>,
    /// Boring evidence.
    pub boring: BoringEvidence,
    /// Share of member detections flagged suspect.
    pub suspect_fraction: f64,
    /// A trust test has already classified it.
    pub trust_tested: bool,
    /// Expected burst interval, s.
    pub expected_interval_s: Option<f64>,
    /// Minimum on/off time, s.
    pub min_on_off_s: Option<f64>,
    /// Next expected burst.
    pub next_burst_eta: Option<Timestamp>,
}

/// Scores one input under `w`.
pub fn score_candidate(w: &ScoreWeights, input: &CandidateInput) -> Candidate {
    let components = ScoreComponents {
        snr_db: input.snr_db.map(|s| s.clamp(-50.0, 200.0)),
        novelty: input.novelty.novelty,
        class_entropy: input.class_entropy.map(|h| h.clamp(0.0, 1.0)),
        decoder_available: input.decoder_available,
        periodicity: input.periodicity.map(|p| p.clamp(0.0, 1.0)),
        boring_prior: boring_prior(&input.boring),
    };
    let score = interestingness(w, &components);
    let suspect_fraction = input.suspect_fraction.clamp(0.0, 1.0);
    Candidate {
        subject: input.subject,
        freq: input.freq,
        score,
        score_norm: normalised(w, score),
        components,
        novelty: input.novelty,
        suspect_fraction,
        needs_verification: suspect_fraction >= 0.5 && !input.trust_tested,
        expected_interval_s: input.expected_interval_s,
        min_on_off_s: input.min_on_off_s,
        next_burst_eta: input.next_burst_eta,
    }
}

/// Scores and ranks: `score` descending, ties by frequency (deterministic). Version 0; the
/// provider assigns the published version.
pub fn rank(
    t: Timestamp,
    site: SiteKey,
    w: ScoreWeights,
    inputs: &[CandidateInput],
) -> CandidateSet {
    let mut candidates: Vec<Candidate> = inputs.iter().map(|i| score_candidate(&w, i)).collect();
    candidates.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(a.freq.lo_hz.total_cmp(&b.freq.lo_hz))
            .then(a.freq.hi_hz.total_cmp(&b.freq.hi_hz))
    });
    CandidateSet {
        schema: ATTENTION_SCHEMA_VERSION,
        version: 0,
        t,
        site,
        weights: w,
        candidates,
    }
}

/// Rate-limited, change-only publisher.
#[derive(Debug, Default)]
pub struct Scorer {
    last_run: Option<Timestamp>,
    last_published: Option<(SiteKey, ScoreWeights, Vec<Candidate>)>,
}

impl Scorer {
    /// A scoring pass is due at `t`.
    pub fn due(&self, t: Timestamp, low_power: bool) -> bool {
        let every = if low_power {
            RESCORE_LOW_POWER_S
        } else {
            RESCORE_S
        };
        self.last_run
            .is_none_or(|l| (t.as_unix_nanos() - l.as_unix_nanos()) as f64 / 1e9 >= every || t < l)
    }

    /// Scores `inputs` if due and publishes when the ranked set differs from the last published
    /// one. Returns the new version when published.
    pub fn run(
        &mut self,
        t: Timestamp,
        low_power: bool,
        site: SiteKey,
        w: ScoreWeights,
        inputs: &[CandidateInput],
        provider: &SharedInterestingness,
    ) -> Result<Option<u64>, ValidationError> {
        if !self.due(t, low_power) {
            return Ok(None);
        }
        self.last_run = Some(t);
        let set = rank(t, site, w, inputs);
        if self
            .last_published
            .as_ref()
            .is_some_and(|(s, lw, c)| *s == site && *lw == w && *c == set.candidates)
        {
            return Ok(None);
        }
        let kept = (site, w, set.candidates.clone());
        let v = provider.publish(set)?;
        self.last_published = Some(kept);
        Ok(Some(v))
    }
}

#[cfg(test)]
mod tests {
    use hk_model::attention::baseline::{BaselineResolution, Maturity};
    use hk_model::attention::score::InterestingnessProvider;

    use super::*;

    fn novelty(n: f64) -> NoveltyScore {
        NoveltyScore {
            novelty: n,
            level_z: None,
            occupancy_z: Some(3.0 + 7.0 * n),
            new_emitter: None,
            observed_s: 900.0,
            maturity: Maturity::Mature {
                resolution: BaselineResolution::AllHours,
            },
            provenance_explained: false,
        }
    }

    fn input(lo: f64, snr: f64, n: f64, class_entropy: Option<f64>) -> CandidateInput {
        CandidateInput {
            subject: CandidateSubject::Cells {
                scheme: 1,
                lo_cell: (lo / 6250.0) as i64,
                hi_cell: (lo / 6250.0) as i64 + 4,
            },
            freq: FreqRange::new(lo, lo + 25e3),
            snr_db: Some(snr),
            novelty: novelty(n),
            class_entropy,
            decoder_available: false,
            periodicity: None,
            boring: BoringEvidence::default(),
            suspect_fraction: 0.0,
            trust_tested: false,
            expected_interval_s: None,
            min_on_off_s: None,
            next_burst_eta: None,
        }
    }

    #[test]
    fn score_boring_prior_caps_allocation_and_honours_mismatch() {
        let mut e = BoringEvidence {
            allocation_suggested: true,
            ..BoringEvidence::default()
        };
        assert_eq!(boring_prior(&e), 0.0, "only after blind characterisation");
        e.blindly_characterised = true;
        assert_eq!(boring_prior(&e), ALLOCATION_BORING_MAX);
        e.allocation_mismatch = true;
        assert_eq!(
            boring_prior(&e),
            0.0,
            "a mismatch is interesting, not boring"
        );
        e.always_on = true;
        e.stable = true;
        e.well_characterised = true;
        assert!((boring_prior(&e) - 0.7).abs() < 1e-12);
        e.allocation_mismatch = false;
        assert_eq!(boring_prior(&e), 1.0);
        assert!(always_on(&[0.99, 0.97]) && !always_on(&[0.99, 0.9]) && !always_on(&[]));
        assert!(stable(&[0.0; 10]) && !stable(&[0.0; 9]));
        assert!(well_characterised(None, true) && !well_characterised(None, false));
    }

    #[test]
    fn score_weights_change_the_ranking_deterministically() {
        // A: novel, weak, unclassified. B: strong, familiar, classified.
        let inputs = [
            input(100e6, 40.0, 0.0, Some(0.1)),
            input(101e6, 6.0, 0.9, None),
        ];
        let t = Timestamp::from_unix_nanos(1);
        let default = rank(t, SiteKey::Unassigned, ScoreWeights::default(), &inputs);
        default.validate().unwrap();
        assert_eq!(
            default.candidates[0].freq.lo_hz, 101e6,
            "novelty-led defaults"
        );
        let snr_led = ScoreWeights {
            version: 2,
            snr: 10.0,
            novelty: 0.0,
            class_entropy: 0.0,
            ..ScoreWeights::default()
        };
        let ranked = rank(t, SiteKey::Unassigned, snr_led, &inputs);
        ranked.validate().unwrap();
        assert_eq!(ranked.candidates[0].freq.lo_hz, 100e6);
        assert_eq!(ranked, rank(t, SiteKey::Unassigned, snr_led, &inputs));
        // Ties break by frequency, independent of input order.
        let tie = [input(103e6, 10.0, 0.5, None), input(102e6, 10.0, 0.5, None)];
        let mut rev = tie.clone();
        rev.reverse();
        let a = rank(t, SiteKey::Unassigned, snr_led, &tie);
        assert_eq!(a, rank(t, SiteKey::Unassigned, snr_led, &rev));
        assert_eq!(a.candidates[0].freq.lo_hz, 102e6);
    }

    #[test]
    fn score_scorer_rate_limits_and_publishes_only_changes() {
        let p = SharedInterestingness::default();
        let mut s = Scorer::default();
        let w = ScoreWeights::default();
        let at = |sec: i64| Timestamp::from_unix_nanos(sec * 1_000_000_000);
        let mut inputs = vec![input(100e6, 20.0, 0.5, None)];
        inputs[0].suspect_fraction = 0.6;
        assert_eq!(
            s.run(at(0), false, SiteKey::Mobile, w, &inputs, &p),
            Ok(Some(1))
        );
        assert!(p.snapshot().candidates[0].needs_verification);
        assert_eq!(
            s.run(at(5), false, SiteKey::Mobile, w, &inputs, &p),
            Ok(None),
            "10 s"
        );
        assert_eq!(
            s.run(at(10), false, SiteKey::Mobile, w, &inputs, &p),
            Ok(None),
            "unchanged"
        );
        inputs[0].novelty = novelty(0.9);
        assert_eq!(
            s.run(at(70), true, SiteKey::Mobile, w, &inputs, &p),
            Ok(Some(2))
        );
        assert_eq!(
            s.run(at(100), true, SiteKey::Mobile, w, &inputs, &p),
            Ok(None),
            "60 s low power"
        );
        assert_eq!(p.version(), 2);
    }
}
