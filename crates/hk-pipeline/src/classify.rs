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
//! So [`should_record`] holds the classifier back in the one case where writing would *demote* a
//! better-informed producer: a **tie at rank 3**, where "latest among equals" would hand the family
//! to whoever wrote last. Where nothing else has spoken — an emitter carrying only track shape, or
//! nothing at all, which is the unknown-signal case this milestone is for — the row is written and
//! wins over shape, exactly as ADR-0016 §2 ranks it.
//!
//! **A row that cannot take over is always recorded** (T-247). When the current classification sits
//! at rank 0–2 (a user, a decoder, or a lock-verified chain or verifier) the classifier's rank-3
//! row can never become the emitter's family however late it is appended, so suppressing it would
//! discard a measurement for no arbitration benefit — and would leave the emitter with no
//! posterior, no open-set score and no `unknown`, which is exactly the vacuum T-247 closed. Such a
//! row is appended and served as `latest_classification` (`docs/api.md`), beside the family the
//! better-informed producer keeps. This is evidence *about* an emitter, never a change *to* one —
//! the rule [`crate::characterise`] follows for matches and clusters.
//!
//! That is why [`record_locked_chain_label`] exists. ADR-0016 §2 puts a demodulator chain that
//! **locked** at [`ArbRank::LockVerified`] (rank 2), but a chain writing through the legacy path
//! records no lock, so its row derives rank 3 ([`hk_model::classify::ArbRank::legacy`]) and ties
//! with the classifier. A chain that knows it locked writes its rank explicitly — the follow-up
//! `hk_model::Repository::append_classification_ranked` names this module as its owner — and the
//! tie disappears: the chain keeps `2fsk` at rank 2, and the classifier's `fsk` posterior is
//! recorded at rank 3 without ever displacing it.

use hk_classify::{Classifier, ClassifyRequest, SymbolEstimator};
use hk_dsp::{InputInfo, IqSample};
use hk_estimate::{
    ChannelSnippet, Hints, ParamEstimator, ParameterSet, SnippetExtractor, SnippetRequest,
};
use hk_model::classify::{ArbRank, Classification, Stage, TaxonomyRef, family_of};
use hk_model::cluster::RecordedClassification;
use hk_model::{EmitterId, RepoError, Repository, Timestamp};

/// Whether an emitter's existing classification means the feature tree must not take over its
/// family (see the module docs).
pub fn should_record(current: Option<&RecordedClassification>) -> bool {
    let Some(r) = current else {
        return true;
    };
    if r.arb_rank != ArbRank::Classifier {
        // Arbitration is already settled without us, in one direction or the other: a row at rank
        // 0-2 (user, decoder, lock-verified) keeps the family however late this one is appended,
        // and track shape at rank 4 loses to it (T-183, ADR-0016 §2). Either way this row cannot
        // demote anyone, so the measurement is recorded rather than thrown away.
        return true;
    }
    // A tie at rank 3, where "latest among equals" decides: only here could writing take the
    // family off a better-informed producer.
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

/// The C13 half of [`classify_box`]: the box's channel snippet and its parameter set — among
/// them the burst **extent** the normalised snippet is cut to, so every C15 envelope feature is
/// measured over it (T-876). `None` when the box cannot be extracted.
///
/// Public so a test can hold the extent this call site measures against a scene's hidden truth
/// (`tests/device_fsk_classify.rs`) without a second copy of the steps drifting from this one.
pub fn measure_box<T: IqSample>(
    info: InputInfo<'_>,
    iq: &[T],
    request: &SnippetRequest,
) -> Option<(ChannelSnippet, ParameterSet)> {
    let mut extractor = SnippetExtractor::new(Default::default());
    let snippet = extractor.extract(info, iq, request).ok()?;
    let params = ParamEstimator::new(Default::default()).estimate(&snippet, &Hints::default());
    Some((snippet, params))
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
/// # The receiver's own cyclic lines (T-399)
///
/// `survey` is the run's receiver-line survey ([`crate::survey`]), and this is where it is
/// **used** — never where it is measured. It is handed to C14 before the estimate, so the cyclic
/// dimensions of `features@1` stop reading the receiver's own contribution as the emission's
/// structure. The survey wants a second of the raw tuned span and this call site holds one burst,
/// so the measurement happens once per capture state on the `hk-survey` reader and is only read
/// here; and it is read through [`crate::survey::ReceiverSurvey::apply`], which gates it on **this
/// window's own provenance**, so a survey never crosses a device, a retune or a gain step.
///
/// Before the first survey lands (its first second of capture), and for any window whose receiver
/// state has no survey, C14 falls back to the recorded per-capture
/// [`hk_model::Provenance::capture_artefacts`] of T-373 and T-382 alone — the status quo. Nothing
/// here waits for a survey.
///
/// The exclusion is never silent whichever source it came from: the excluded frequencies are
/// reported on the estimate (`excluded_receiver_hz`, `excluded_cyclic_hz`), a line that lost to
/// one is marked `artefact_suppressed`, and the estimate carries `BlindReason::CaptureArtefact`.
/// It removes lines from the **argmax only**, never from the whitening floor: that is real power.
///
/// `None` when the box cannot be extracted or C13 could not measure enough to normalise it (an
/// abstention upstream, not a classification of `unknown`).
pub fn classify_box<T: IqSample>(
    classifier: &Classifier,
    c14: &mut SymbolEstimator,
    survey: &crate::survey::ReceiverSurvey,
    info: InputInfo<'_>,
    iq: &[T],
    request: &SnippetRequest,
    t: Timestamp,
) -> Option<Classification> {
    survey.apply(c14, info.provenance.get());
    let (snippet, params) = measure_box(info, iq, request)?;
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

/// **The pipeline's single C15 call site** (T-247): classifies one detection box and appends the
/// result to `emitter`. Returns the classification and whether a row was written (`None` when the
/// cascade abstained upstream — see [`classify_box`]).
///
/// # Where this runs, and what bounds its cost
///
/// On a **chain writer thread** (`crate::chains::fsk`), off the ring, the DSP readers and the audio
/// chains, where `crate::family::explain_emitter` and [`crate::characterise`] already run
/// (ADR-0007; ADR-0016 §4, "Placement"). The chain already owns its own copy of the samples, so
/// nothing here touches the ring; only the [`record`] call takes the repository lock the caller
/// already holds.
///
/// Its cost is bounded by construction, as [`crate::characterise`] states its own:
///
/// - **At most one classification per emitter per chain write.** A chain classifies the single
///   most-evidence burst it demodulated, once, at detach — not one per burst and not one per
///   flush.
/// - **Per call:** one snippet extraction over that one burst, one C13 parameter estimate, one C14
///   symbol window capped at `hk_classify::symbols`' own sample ceiling, one `features@1` vector
///   (≤ 24 dimensions), `O(families)` density evaluations, and one row insert. No FFT beyond that
///   single snippet, no ring or sample-buffer access, and no I/O beyond the repository the caller
///   already holds.
/// - **The receiver-line survey is not part of that bound, and is not run here** (T-399). It costs
///   a filter bank and a whitened periodogram per reference channel over a second or more of the
///   raw span — 785 ms of one core on a 2 s window at 2.4 Msps, measured — which is why it is
///   measured once per capture state on the `hk-survey` reader and only *read* at this call site,
///   as a clone of a small line list ([`classify_box`]).
#[allow(clippy::too_many_arguments)]
pub fn classify_and_record<T: IqSample>(
    repo: &mut Repository,
    emitter: EmitterId,
    classifier: &Classifier,
    c14: &mut SymbolEstimator,
    survey: &crate::survey::ReceiverSurvey,
    info: InputInfo<'_>,
    iq: &[T],
    request: &SnippetRequest,
    t: Timestamp,
) -> Result<Option<(Classification, bool)>, RepoError> {
    let Some(classification) = classify_box(classifier, c14, survey, info, iq, request, t) else {
        return Ok(None);
    };
    let written = record(repo, emitter, &classification)?;
    Ok(Some((classification, written)))
}

/// Re-appends a demodulator chain's own label at [`ArbRank::LockVerified`] (rank 2), the rank
/// ADR-0016 §2 gives a chain that **locked** — a recovered clock plus CRC-valid framing, not a
/// pre-sync guess. Returns whether a row was written.
///
/// A chain writing through the legacy path records no lock, so its row derives rank 3
/// ([`hk_model::classify::ArbRank::legacy`]) and **ties** with the classifier, where "latest among
/// equals" would let a later rank-3 row take the family off it. Stating the rank removes the tie:
/// the chain keeps `2fsk` as the emitter's family, and [`classify_and_record`] may then record the
/// C15 posterior at rank 3 without ever displacing it (see the module docs).
///
/// The row is the chain's **own** classification, re-appended at its proper rank — nothing is
/// invented here and no label changes. Does nothing (`false`) when the emitter's current
/// classification is not a rank-3 chain row, so a second call after promotion is a no-op.
pub fn record_locked_chain_label(
    repo: &mut Repository,
    emitter: EmitterId,
) -> Result<bool, RepoError> {
    let id = repo.live_emitter_id(emitter)?;
    let Some(r) = repo.current_classification(id)? else {
        return Ok(false);
    };
    if r.stage != Stage::Chain || r.arb_rank != ArbRank::Classifier {
        return Ok(false);
    }
    repo.append_classification_ranked(id, &r.classification, Stage::Chain, ArbRank::LockVerified)?;
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
        // The one case writing would demote someone: an unlocked chain label at the classifier's
        // own rank, where "latest among equals" decides.
        assert!(
            !should_record(Some(&recorded("wfm", Stage::Chain, ArbRank::Classifier))),
            "a rank-3 chain label must keep the emitter's family"
        );
        // T-247: a producer the classifier cannot outrank keeps the family whatever is appended
        // after it, so the measurement is recorded as evidence rather than discarded. These rows
        // become `latest_classification` beside the family their producer keeps.
        for (family, stage, rank) in [
            ("2fsk", Stage::Chain, ArbRank::LockVerified),
            ("fsk", Stage::Verifier, ArbRank::LockVerified),
            ("bpsk", Stage::User, ArbRank::User),
            // A decoder's service label is not a modulation at all.
            ("adsb", Stage::Decoder, ArbRank::Decoder),
        ] {
            assert!(
                should_record(Some(&recorded(family, stage, rank))),
                "{family} from {stage:?} outranks the classifier, so recording cannot demote it"
            );
        }
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
