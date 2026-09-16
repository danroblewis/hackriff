//! The single pipeline call site of the C15 classifier (ADR-0016 §4, "Placement").
//!
//! Classification runs **per event, on the CPU, off the ring and DSP threads** — where
//! [`crate::family::explain_emitter`] already runs, at a chain writer. One detection box in, one
//! [`Classification`] appended to the emitter at [`ArbRank::Classifier`] (rank 3).
//!
//! # Wrapping, not replacing (why a row is sometimes not written)
//!
//! An M3 row carries a **family** label (`fsk`, `analog`), while the demodulator and decoder
//! chains write the **class** they demodulated (`2fsk`, `wfm`) through the legacy writer, which
//! derives rank 3 as well ([`hk_model::classify::rank`]). Arbitration is "lowest rank, latest
//! among equals", so a rank-3 row written after a chain's would take over the emitter's family and
//! turn `2fsk` into `fsk` — a regression for every reader that expects the chain's label, and for
//! the M0/M1 acceptance suites that assert it.
//!
//! So [`should_record`] holds the classifier back whenever a **better-informed producer** (a user,
//! a decoder, a lock-verified verifier, or a demodulator chain) has already put a known `hk-mod@1`
//! family on the emitter. The classification is still computed and returned to the caller; it is
//! the *arbitration* that is left alone. Where nothing else has spoken — an emitter carrying only
//! track shape, or nothing at all, which is the unknown-signal case this milestone is for — the
//! row is written and wins over shape, exactly as ADR-0016 §2 ranks it.
//!
//! The lasting fix is for locked chain labels to be written at [`ArbRank::LockVerified`] (rank 2),
//! which ADR-0016 §2 already specifies; then this guard becomes unnecessary. That is a follow-up:
//! it changes what `/api/inventory` reports for every demodulated emitter.

use hk_classify::{Classifier, ClassifyRequest, SymbolEstimator};
use hk_dsp::{InputInfo, IqSample};
use hk_estimate::{Hints, ParamEstimator, SnippetExtractor, SnippetRequest};
use hk_model::classify::{ArbRank, Classification, Stage, TaxonomyRef, family_of};
use hk_model::cluster::RecordedClassification;
use hk_model::{EmitterId, RepoError, Repository, Timestamp};

/// Whether an emitter's existing classification means the feature tree must not take over its
/// family (see the module docs).
pub fn should_record(current: Option<&RecordedClassification>) -> bool {
    let Some(r) = current else {
        return true;
    };
    if r.arb_rank > ArbRank::Classifier {
        // Track shape only: the classifier outranks it (ADR-0016 §2).
        return true;
    }
    match r.stage {
        // Our own earlier rows (or a DL stage's) may be superseded: that is a re-classification,
        // not a demotion.
        Stage::FeatureTree | Stage::Dl => true,
        // A user, a decoder, a verifier or a demodulator chain knows more than the feature tree.
        // Only a label that is not a modulation at all (a service family such as `adsb`) leaves
        // room for it.
        _ => family_of(&r.classification.family, &TaxonomyRef::current()).is_none(),
    }
}

/// Runs the cascade over one detection box, through the same C13 chain the demodulators use:
/// snippet → parameters → C14 symbol estimate → CFO-corrected, power-normalised snippet →
/// classification.
///
/// **C14 runs here** (T-238), once per classification event, on this CPU call site — off the ring
/// and DSP real-time threads, where the feature vector and `family::explain_emitter` already run
/// (ADR-0007; ADR-0016 §4, "Placement"). Its cost is bounded by construction: it analyses this one
/// snippet, cut to the detection extent, at a fixed 6 samples per OBW99, and reports its own
/// `cost_us`. `c14` is threaded in rather than built per call so its FFT plans are cached across
/// events. Without it the six symbol-derived dimensions of `features@1` abstain and the row carries
/// `no_symbol_estimate`; where C14 genuinely cannot estimate, they still abstain
/// ([`hk_classify::symbols`]) rather than being given a fabricated value.
///
/// `None` when the box cannot be extracted or C13 could not measure enough to normalise it (an
/// abstention upstream, not a classification of `unknown`).
pub fn classify_box<T: IqSample>(
    classifier: &Classifier,
    c14: &mut SymbolEstimator,
    info: InputInfo<'_>,
    iq: &[T],
    request: &SnippetRequest,
    t: Timestamp,
) -> Option<Classification> {
    let mut extractor = SnippetExtractor::new(Default::default());
    let snippet = extractor.extract(info, iq, request).ok()?;
    let params = ParamEstimator::new(Default::default()).estimate(&snippet, &Hints::default());
    let window = c14.window_from_snippet(&snippet, &params);
    let normalised =
        hk_estimate::normalise::normalise(&snippet, &params, &Default::default()).ok()?;
    let mut req = ClassifyRequest::new(&normalised.samples, normalised.sample_rate_hz, t);
    req.obw_hz = params.obw99_hz.value();
    req.snr_db = params
        .snr_extent_db
        .value()
        .or_else(|| params.snr_box_db.value());
    req.symbols = window.as_ref().map(|w| &w.params);
    // The T-200 verifier tests its likelihoods on the same window C14 synced on, so "post-sync"
    // means the samples the clock was actually locked to (`hk_classify::verify`).
    req.symbol_samples = window.as_ref().map(|w| w.samples.as_slice());
    req.symbol_sample_rate_hz = window.as_ref().map(|w| w.sample_rate_hz);
    req.suspect.clipped = params.flags.clipped;
    req.suspect.spur = params.flags.overload;
    Some(classifier.classify(&req))
}

/// Appends `classification` to `emitter` at [`ArbRank::Classifier`], unless a better-informed
/// producer already owns its family ([`should_record`]). Returns whether a row was written.
pub fn record(
    repo: &mut Repository,
    emitter: EmitterId,
    classification: &Classification,
) -> Result<bool, RepoError> {
    let id = repo.live_emitter_id(emitter)?;
    if !should_record(repo.current_classification(id)?.as_ref()) {
        return Ok(false);
    }
    repo.record_classification(id, classification, ArbRank::Classifier)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_model::classify::{
        CLASSIFICATION_SCHEMA, ClassProvenance, Coarse, HK_MOD_V1, LabelP, SuspectFlags,
        entropy_norm,
    };
    use hk_model::emitter::Classification as LegacyClassification;

    fn recorded(family: &str, stage: Stage, rank: ArbRank) -> RecordedClassification {
        RecordedClassification {
            classification: LegacyClassification {
                t: Timestamp::UNIX_EPOCH,
                family: family.to_owned(),
                confidence: 0.9,
                open_set_score: 0.1,
                model_version: "test".into(),
            },
            input: None,
            feature_set_version: None,
            taxonomy: None,
            stage,
            arb_rank: rank,
            detail: None,
        }
    }

    #[test]
    fn the_classifier_never_demotes_a_better_informed_producer() {
        // Nothing yet, or track shape only: the classifier writes.
        assert!(should_record(None));
        assert!(should_record(Some(&recorded(
            "fm-broadcast",
            Stage::TrackShape,
            ArbRank::TrackShape
        ))));
        // A demodulator chain, a decoder, a verifier or a user owns the family: it does not.
        for (family, stage, rank) in [
            ("wfm", Stage::Chain, ArbRank::Classifier),
            ("2fsk", Stage::Chain, ArbRank::LockVerified),
            ("fsk", Stage::Verifier, ArbRank::LockVerified),
            ("bpsk", Stage::User, ArbRank::User),
        ] {
            assert!(
                !should_record(Some(&recorded(family, stage, rank))),
                "{family} from {stage:?} must keep the emitter's family"
            );
        }
        // A decoder's service label is not a modulation, so the tree may still say what the
        // modulation is.
        assert!(should_record(Some(&recorded(
            "adsb",
            Stage::Decoder,
            ArbRank::Decoder
        ))));
        // Its own earlier row is superseded, not protected.
        assert!(should_record(Some(&recorded(
            "fsk",
            Stage::FeatureTree,
            ArbRank::Classifier
        ))));
    }

    #[test]
    fn a_recorded_row_keeps_the_contract_and_the_classifier_rank() {
        let posterior = vec![
            LabelP {
                label: "fsk".into(),
                p: 0.8,
            },
            LabelP {
                label: "unknown".into(),
                p: 0.2,
            },
        ];
        let c = Classification {
            schema: CLASSIFICATION_SCHEMA,
            t: Timestamp::UNIX_EPOCH,
            taxonomy: TaxonomyRef::current(),
            input: None,
            coarse: Coarse::Digital,
            entropy_norm: entropy_norm(&posterior, HK_MOD_V1.families.len() + 1),
            likelihood: posterior.clone(),
            posterior,
            prior: None,
            family: "fsk".into(),
            confidence: 0.8,
            class: None,
            open_set_score: 0.2,
            stage: Stage::FeatureTree,
            provenance: ClassProvenance {
                rules: hk_classify::RULES_VERSION.into(),
                features_version: hk_classify::FEATURES_VERSION,
                features_ref: None,
                ml: None,
                snr_db: Some(24.0),
                snr_gate_db: 20.0,
                gated: false,
                thresholds: hk_classify::THRESHOLDS_VERSION.into(),
                suspect: SuspectFlags::default(),
                power_mode: None,
            },
            flags: Vec::new(),
            reasons: Vec::new(),
        };
        c.validate().expect("the call site writes valid rows");
        assert!(ArbRank::Classifier.allows(c.stage));
    }
}
