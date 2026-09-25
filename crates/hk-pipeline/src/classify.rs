//! The single pipeline call site of the C15 classifier (ADR-0016 §4, "Placement").
//!
//! Classification runs **per event, on the CPU, off the ring and DSP threads**. One detection box
//! in, one [`Classification`] appended to the emitter at [`ArbRank::Classifier`] (rank 3).
//!
//! # Who calls it (T-878)
//!
//! [`crate::chains::classify`]: a measuring chain on [`crate::chains::spec::Trigger::EveryTrack`],
//! attached **beside** whatever decode chain a confirmed track selected, for every confirmed track
//! whose priors it matches. Until T-878 the only caller sat inside the fsk chain, after a
//! successful framed write, so an emission no decode chain matched (POCSAG and ACARS scenes), one
//! only a non-classifying chain matched (a WFM station) and one a chain matched but could not
//! demodulate (a LoRa burst the fsk chain got too few bursts of) were never classified at all —
//! exactly the unknown signals this milestone is for. The classifier is now a function of the
//! region's own IQ and nothing else a chain decides.
//!
//! # Wrapping, not replacing (T-247, and the rank-3 tie T-878 resolved)
//!
//! An M3 row carries a **family** label (`fsk`, `analog`), while the demodulator and decoder
//! chains write the **class** they demodulated (`2fsk`, `wfm`) through the legacy writer, which
//! derives rank 3 as well ([`hk_model::classify::rank`]). Arbitration is "lowest rank, latest
//! among equals", so a rank-3 row written after a chain's would take over the emitter's family and
//! turn `2fsk` into `fsk` — a regression for every reader that expects the chain's label, and for
//! the M0/M1 acceptance suites that assert it.
//!
//! **Every classification is recorded.** A row that cannot take over the family — the current
//! classification sits at rank 0–2 (a user, a decoder, a lock-verified chain or verifier) — is
//! appended as evidence beside the family its producer keeps (T-247), and served as
//! `latest_classification` (`docs/api.md`). A row that may take over — nothing else has spoken, or
//! only track shape, or the classifier's own earlier row — is appended and wins, exactly as
//! ADR-0016 §2 ranks it.
//!
//! The one remaining case is the **tie at rank 3** against a better-informed producer: a
//! demodulator chain that wrote its label without recording a lock ([`keeps_family`]). Until T-878
//! the classifier's row was simply *not written* there, which discarded a measurement for an
//! arbitration reason: a multipath scene classified three times and persisted nothing. Now the row
//! is written and the producer's own label is then **restated** at the same rank
//! ([`record`]), so "latest among equals" still hands the family to the producer. Nothing is
//! invented — the restated row is the producer's own family, confidence, open-set score, model
//! version and time, re-appended exactly as [`record_locked_chain_label`] re-appends it at rank 2
//! — and the arbitration ladder in `hk_model` is untouched. The cost is one extra history row per
//! classification of such an emitter, which is bounded by the classification bound below.
//!
//! That is also why [`record_locked_chain_label`] exists. ADR-0016 §2 puts a demodulator chain that
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

/// Whether an emitter's current classification belongs to a **better-informed producer that ties
/// with the classifier at rank 3** — the one case where appending a C15 row would take the family
/// off someone who knows more (see the module docs). [`record`] still writes the row, and then
/// restates that producer's label so it stays "latest among equals".
pub fn keeps_family(current: Option<&RecordedClassification>) -> bool {
    let Some(r) = current else {
        return false;
    };
    if r.arb_rank != ArbRank::Classifier {
        // Arbitration is already settled without us, in one direction or the other: a row at rank
        // 0-2 (user, decoder, lock-verified) keeps the family however late this one is appended,
        // and track shape at rank 4 loses to it (T-183, ADR-0016 §2).
        return false;
    }
    match r.stage {
        // Our own earlier rows (or a DL stage's) may be superseded: that is a re-classification,
        // not a demotion.
        Stage::FeatureTree | Stage::Dl => false,
        // A demodulator chain knows more than the feature tree. Only a label that is not a
        // modulation at all (a service family such as `adsb`) leaves room for it.
        _ => family_of(&r.classification.family, &TaxonomyRef::current()).is_some(),
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
    classify_box_observed(classifier, c14, survey, info, iq, request, t, None).map(|(c, _)| c)
}

/// [`classify_box`], plus the learned stage's input (`hk_classify::dl_input`) of the **same**
/// normalised snippet when `ml` has a model in a non-`off` mode for the family that came out
/// (T-844). The vector is computed only then, so an idle shadow stage costs nothing here; `None`
/// in the second slot means no model wanted it, or the snippet was unmeasurable.
#[allow(clippy::too_many_arguments)]
pub fn classify_box_observed<T: IqSample>(
    classifier: &Classifier,
    c14: &mut SymbolEstimator,
    survey: &crate::survey::ReceiverSurvey,
    info: InputInfo<'_>,
    iq: &[T],
    request: &SnippetRequest,
    t: Timestamp,
    ml: Option<&crate::ml::MlStage>,
) -> Option<(Classification, Option<Vec<f32>>)> {
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
    let classification = classifier.classify(&req);
    let input = ml
        .filter(|m| m.wants(&classification.family))
        .and_then(|_| hk_classify::dl_input(req.samples, req.sample_rate_hz, req.obw_hz));
    Some((classification, input))
}

/// Appends `classification` to `emitter` at [`ArbRank::Classifier`]. **Always writes** (T-878):
/// where a better-informed producer ties at rank 3 ([`keeps_family`]), its label is restated
/// after the row so it keeps the emitter's family. Returns whether the producer's label was
/// restated.
///
/// Both appends happen under the one repository borrow the caller holds, so no other writer can
/// land between them and read the classifier's row as the family.
pub fn record(
    repo: &mut Repository,
    emitter: EmitterId,
    classification: &Classification,
) -> Result<bool, RepoError> {
    let id = repo.live_emitter_id(emitter)?;
    let current = repo.current_classification(id)?;
    repo.record_classification(id, classification, ArbRank::Classifier)?;
    let Some(r) = current.filter(|r| keeps_family(Some(r))) else {
        return Ok(false);
    };
    repo.append_classification_ranked(id, &r.classification, r.stage, r.arb_rank)?;
    Ok(true)
}

/// **The pipeline's single C15 call site** (T-247, T-878): classifies one detection box and
/// appends the result to `emitter` ([`record`], which always writes). Returns the classification
/// and whether a tied producer's label was restated after it (`None` when the cascade abstained
/// upstream — see [`classify_box`]).
///
/// # Where this runs, and what bounds its cost
///
/// On a **classifying chain's own thread** ([`crate::chains::classify`]), off the ring, the DSP
/// readers and the audio chains, where [`crate::characterise`] already runs for the sweep chain
/// (ADR-0007; ADR-0016 §4, "Placement"). The chain owns its own copy of the samples, so nothing
/// here touches the ring; only the [`record`] call takes the repository lock.
///
/// Its cost is bounded by construction, as [`crate::characterise`] states its own:
///
/// - **At most one classification per confirmed track.** The chain classifies the single
///   most-evidence box its track produced (the longest, capped at the node's `window_s`), once —
///   not one per burst and not one per flush — and at most `max_chains` such chains run at once.
/// - **Per call:** one snippet extraction over that one box, one C13 parameter estimate, one C14
///   symbol window capped at `hk_classify::symbols`' own sample ceiling, one `features@1` vector
///   (≤ 24 dimensions), `O(families)` density evaluations, and one or two row inserts. No FFT
///   beyond that single snippet, no ring or sample-buffer access, and no I/O beyond the
///   repository.
/// - **The receiver-line survey is not part of that bound, and is not run here** (T-399). It costs
///   a filter bank and a whitened periodogram per reference channel over a second or more of the
///   raw span — 785 ms of one core on a 2 s window at 2.4 Msps, measured — which is why it is
///   measured once per capture state on the `hk-survey` reader and only *read* at this call site,
///   as a clone of a small line list ([`classify_box`]).
///
/// [`crate::chains::classify`] calls [`classify_box`] and [`record`] separately rather than
/// through this function, because it may classify a continuous emission before the inventory has
/// an entry to write it against; the two halves and their bound are the same.
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
    let restated = record(repo, emitter, &classification)?;
    Ok(Some((classification, restated)))
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
        // Nothing yet, or track shape only: the classifier's row may take the family.
        assert!(!keeps_family(None));
        assert!(!keeps_family(Some(&recorded(
            "fm-broadcast",
            Stage::TrackShape,
            ArbRank::TrackShape
        ))));
        // The one tie: an unlocked chain label at the classifier's own rank, where "latest among
        // equals" would decide. The chain keeps the family (its label is restated after the row).
        assert!(
            keeps_family(Some(&recorded("wfm", Stage::Chain, ArbRank::Classifier))),
            "a rank-3 chain label must keep the emitter's family"
        );
        // T-247: a producer the classifier cannot outrank keeps the family whatever is appended
        // after it, so nothing needs restating.
        for (family, stage, rank) in [
            ("2fsk", Stage::Chain, ArbRank::LockVerified),
            ("fsk", Stage::Verifier, ArbRank::LockVerified),
            ("bpsk", Stage::User, ArbRank::User),
            ("adsb", Stage::Decoder, ArbRank::Decoder),
            // A chain's service label is not a modulation at all.
            ("adsb", Stage::Chain, ArbRank::Classifier),
        ] {
            assert!(
                !keeps_family(Some(&recorded(family, stage, rank))),
                "{family} from {stage:?} at {rank:?} needs no restating"
            );
        }
        // Its own earlier row is superseded, not protected.
        assert!(!keeps_family(Some(&recorded(
            "fsk",
            Stage::FeatureTree,
            ArbRank::Classifier
        ))));
    }

    /// T-878: the rank-3 tie **records** the classifier's row and leaves the chain's family —
    /// before T-878 the row was dropped, so an emitter an unlocked chain labelled was never
    /// classified at all (the multipath scene persisted nothing from three classifications).
    #[test]
    fn a_rank_three_tie_records_the_row_and_the_chain_keeps_the_family() {
        use hk_model::emitter::Classification as Legacy;
        let mut repo = Repository::open_in_memory().unwrap();
        let t = Timestamp::UNIX_EPOCH;
        let e = emitter(&mut repo, 433.92e6);
        // An unlocked chain label, through the legacy writer: stage chain, derived rank 3.
        repo.append_classification(
            e,
            &Legacy {
                t,
                family: "2fsk".into(),
                confidence: 0.9,
                open_set_score: 0.1,
                model_version: "hk-demod/fsk@1".into(),
            },
        )
        .unwrap();
        let before = repo.classification_history(e).unwrap().len();

        let restated = record(&mut repo, e, &posterior("fsk")).unwrap();
        assert!(restated, "the tie restates the chain's label");
        let history = repo.classification_history(e).unwrap();
        assert_eq!(history.len(), before + 2, "the row and the restatement");
        assert!(
            history
                .iter()
                .any(|r| r.stage == Stage::FeatureTree && r.detail.is_some()),
            "the classifier's row was recorded: {history:?}"
        );
        let current = repo.current_classification(e).unwrap().unwrap();
        assert_eq!(current.classification.family, "2fsk");
        assert_eq!(current.stage, Stage::Chain);
        // The restated row is still the chain's unlocked label, so a chain that later proves its
        // lock can promote it exactly as before.
        assert!(record_locked_chain_label(&mut repo, e).unwrap());

        // Without a producer to tie with, the row is written and takes the family.
        let e2 = emitter(&mut repo, 915.0e6);
        assert!(!record(&mut repo, e2, &posterior("unknown")).unwrap());
        let c2 = repo.current_classification(e2).unwrap().unwrap();
        assert_eq!(c2.stage, Stage::FeatureTree);
        assert_eq!(c2.classification.family, "unknown");
    }

    /// An emitter with no classification at all, from one track sighting.
    fn emitter(repo: &mut Repository, f: f64) -> EmitterId {
        use hk_model::{Fingerprint, LinkTarget, Sighting, TimeRange, TrackId};
        let t = Timestamp::UNIX_EPOCH;
        let s = Sighting {
            source: LinkTarget::Track(TrackId::new()),
            seen: TimeRange::new(t, t.saturating_add_nanos(1_000_000_000)),
            count: 1,
            f_center_hz: f,
            bandwidth_hz: 20e3,
            fingerprint: Some(Fingerprint::new(f, 20e3)),
            identity: None,
            context: None,
            classification: None,
            tags: Vec::new(),
        };
        repo.record_sighting(&s, None).unwrap().emitter_id
    }

    /// A valid C15 row naming `family` (or `unknown`).
    fn posterior(family: &str) -> Classification {
        let posterior = if family == "unknown" {
            vec![
                LabelP {
                    label: "fsk".into(),
                    p: 0.2,
                },
                LabelP {
                    label: "unknown".into(),
                    p: 0.8,
                },
            ]
        } else {
            vec![
                LabelP {
                    label: family.into(),
                    p: 0.8,
                },
                LabelP {
                    label: "unknown".into(),
                    p: 0.2,
                },
            ]
        };
        Classification {
            schema: CLASSIFICATION_SCHEMA,
            t: Timestamp::UNIX_EPOCH,
            taxonomy: TaxonomyRef::current(),
            input: None,
            coarse: Coarse::Digital,
            entropy_norm: entropy_norm(&posterior, HK_MOD_V1.families.len() + 1),
            likelihood: posterior.clone(),
            posterior,
            prior: None,
            family: family.into(),
            confidence: 0.8,
            class: None,
            open_set_score: if family == "unknown" { 0.8 } else { 0.2 },
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
        }
    }

    #[test]
    fn a_recorded_row_keeps_the_contract_and_the_classifier_rank() {
        let c = posterior("fsk");
        c.validate().expect("the call site writes valid rows");
        assert!(ArbRank::Classifier.allows(c.stage));
    }
}
