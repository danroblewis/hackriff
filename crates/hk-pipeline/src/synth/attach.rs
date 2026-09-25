//! **Attach and confirm-by-decode** for a finished region-analyze job (MAUTO M-9, T-860; ADR-0015
//! §5.4–§5.5 as amended by ADR-0021 §11.2 and derived by ADR-0022 §6).
//!
//! A `done` job with results, asked to attach, does three things, in this order:
//!
//! 1. **Finds its emitter.** An emitter target is that emitter. Otherwise the live inventory
//!    emitter nearest the searched channel **in the analysed window** (the
//!    [`crate::refine::emitter_for_channel`] geometry, scoped to the window). If there is none,
//!    the job's stored decodes create a candidate through ordinary ingestion, and that is the
//!    emitter.
//! 2. **Stores decodes** — only for a rank-1 result that **solved** on hold-out, and only the
//!    frames of that hold-out run (never the search window). Each is an ordinary `Decode` row
//!    under the repository's content gate and the stream policy's shape rule. With an emitter
//!    already found it is stored and **linked** to it; with none it goes through the T-111
//!    ingestion path ([`hk_plugins::Ingest::store_decode`]), whose identity sighting creates the
//!    candidate. Every row has
//!    `decoder_id = synth:<template id | open>`, `decoder_version = <engine>+<recipe hash>` and
//!    [`DecodeProvenance::Synthesized`] carrying the numbers the confirm gate read. An open search
//!    names the **structural** identity `other:hk-framing`, which the identity route never
//!    confirms on; a template-bound identity is still marked synthesized, and
//!    `Repository::identity_decode_evidence` does not count `synth:` rows, so **the only way a
//!    synthesized decode confirms anything is [`SynthesizedConfirm`]**.
//! 3. **Appends an `emitter_synthesis` row** (append-only; the emitter's measured values are
//!    never touched) carrying the job, the rank-1 recipe inline with its hash, the hold-out
//!    evidence, `trace_summary`, `replay_key`, the null control and the sealed resolution, then
//!    runs [`SynthesizedConfirm::decide`]. A pass confirms the emitter (author `auto`, actor
//!    [`CONFIRM_SYNTH_RULE`]) — never demotes, and a user delete wins.

use hk_model::repo::LIFECYCLE_TEXT_MAX;
use hk_model::repo::synthesis::{
    EmitterSynthesis, Resolution as RowResolution, ResolutionKind as RowKind,
    ResolutionReason as RowReason, SYNTHESIZED_BY_OUTPUT_ANALYSIS, Stage as RowStage,
    StageEvidence as RowEvidence, SynthPipeline, SynthesisJob, Verdict as RowVerdict,
};
use hk_model::{
    ContentClass, ContentHash, CrcStatus, Decode, DecodeId, DecodeProvenance, DecodedIdentity,
    EmitterId, FreqRange, IdentityScheme, LifecycleAuthor, LifecycleState, Region, RepoError,
    Repository, TimeRange, Timestamp,
};
use hk_model::{EmitterLink, LinkTarget};
use hk_plugins::Ingest;
use hk_stream::policy;
use hk_synth::result::HoldoutFrame;
use hk_synth::trace::{Reason, Resolution, ResolutionKind};
use hk_synth::{PipelineResult, Profile, SearchOutcome, Stage, Verdict};
use serde_json::{Value, json};

use crate::inventory::{
    CONFIRM_SYNTH_RULE, SynthConfirmDecision, SynthConfirmOutcome, SynthesizedConfirm,
    SynthesizedEvidence,
};

/// The structural identity scheme an open search's decodes carry (the blind framer's, T-082).
pub const STRUCTURAL_SCHEME: &str = "hk-framing";

/// Most numeric recipe parameters copied onto the row's `pipeline.params`.
const MAX_PIPELINE_PARAMS: usize = 32;

/// Everything the attach step reads from a finished job.
pub struct AttachInput<'a> {
    /// `a<n>`.
    pub job_id: &'a str,
    /// The job's profile.
    pub profile: Profile,
    /// The emitter target, when the job had one.
    pub target: Option<EmitterId>,
    /// The channel searched.
    pub band: FreqRange,
    /// What was actually read (from the read ledger).
    pub window: TimeRange,
    /// The finished search.
    pub outcome: &'a SearchOutcome,
    /// ADR-0021 §4.1.
    pub trace_summary: Option<Value>,
    /// ADR-0021 §5.
    pub replay_key: Option<Value>,
    /// The sealed resolution, when nothing solved.
    pub resolution: Option<&'a Resolution>,
    /// The acquired IQ's content class (the most restrictive piece).
    pub content_class: ContentClass,
    /// Whether any acquired piece was recorded under an overloaded front end.
    pub overload: Option<bool>,
    /// Whether the job has been cancelled since it finished searching, read under the job
    /// manager's lock. Checked last, inside the attach transaction: a cancelled job never attaches
    /// (docs/api.md), so a cancel that lands mid-attach rolls every write back — decodes, row and
    /// confirmation alike. `None`: nothing can cancel.
    pub cancelled: Option<&'a (dyn Fn() -> bool + Sync)>,
}

/// What attaching did (served on the job: `emitter_id`, `decodes`, `confirm`).
#[derive(Clone, Debug, PartialEq)]
pub struct Attached {
    /// The emitter the results attached to, if any.
    pub emitter: Option<EmitterId>,
    /// Decode rows stored.
    pub decodes_stored: u64,
    /// Of those, CRC-valid without correction.
    pub decodes_valid: u64,
    /// Whether an `emitter_synthesis` row was appended.
    pub synthesis_written: bool,
    /// The confirm decision.
    pub confirm: SynthConfirmDecision,
}

/// `sha256:<hex>` of a recipe's canonical JSON.
pub fn recipe_hash(recipe: &hk_recipe::Recipe) -> String {
    ContentHash::of(recipe).map_or_else(
        |_| "sha256:unhashable".to_owned(),
        |h| format!("sha256:{}", h.to_hex()),
    )
}

/// The live inventory emitter nearest `band`'s centre among those seen in `window` whose centre
/// lies within half the band and which is at most twice as wide (ADR-0015 §5.4).
pub fn emitter_in_window(
    repo: &Repository,
    band: FreqRange,
    window: TimeRange,
) -> Result<Option<EmitterId>, RepoError> {
    let center = (band.lo_hz + band.hi_hz) / 2.0;
    let width = band.hi_hz - band.lo_hz;
    let mut rows: Vec<_> = repo
        .emitters_in_region(&Region::new(band, window))?
        .into_iter()
        .filter(|e| (e.f_center_hz - center).abs() <= 0.5 * width && e.bandwidth_hz <= 2.0 * width)
        .collect();
    rows.sort_by(|a, b| {
        (a.f_center_hz - center)
            .abs()
            .total_cmp(&(b.f_center_hz - center).abs())
    });
    // A user-deleted row is out of the inventory: never attach to it.
    for e in rows {
        if repo.emitter_lifecycle_state(e.id)? != LifecycleState::Deleted {
            return Ok(Some(e.id));
        }
    }
    Ok(None)
}

fn name_of<T: serde::Serialize>(v: T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn row_stage(s: Stage) -> RowStage {
    match s {
        Stage::S0 => RowStage::S0Channel,
        Stage::S1 => RowStage::S1Demod,
        Stage::S2 => RowStage::S2Clock,
        Stage::S3 => RowStage::S3Bits,
        Stage::S4 => RowStage::S4Framing,
        Stage::S5 => RowStage::S5Check,
        Stage::S6 => RowStage::S6Fields,
    }
}

fn row_verdict(v: Verdict) -> RowVerdict {
    match v {
        Verdict::Energy => RowVerdict::Energy,
        Verdict::Demodulated => RowVerdict::Demodulated,
        Verdict::Clocked => RowVerdict::Clocked,
        Verdict::Framed => RowVerdict::Framed,
        Verdict::Checked => RowVerdict::Checked,
        Verdict::Solved => RowVerdict::Solved,
    }
}

fn row_resolution(r: &Resolution) -> RowResolution {
    RowResolution {
        kind: match r.kind {
            ResolutionKind::Unknown => RowKind::Unknown,
            ResolutionKind::StructuredUnidentified => RowKind::StructuredUnidentified,
            ResolutionKind::UnsupportedStructure => RowKind::UnsupportedStructure,
            ResolutionKind::NotSearched => RowKind::NotSearched,
        },
        deepest_verdict: r.deepest_verdict.map(row_verdict),
        reason: r.reason.map(|x| match x {
            Reason::NoSignal => RowReason::NoSignal,
            Reason::NothingScored => RowReason::NothingScored,
            Reason::Tied => RowReason::Tied,
            Reason::BudgetExhausted => RowReason::BudgetExhausted,
            Reason::UnsupportedStructure => RowReason::UnsupportedStructure,
        }),
        summary: r.summary.clone(),
    }
}

/// The recipe block that produced `stage`'s evidence on the ladder.
fn block_at(r: &PipelineResult, stage: Stage) -> Option<String> {
    let node = r.stages.iter().find(|e| e.stage == stage)?.node.clone();
    r.recipe
        .nodes
        .iter()
        .find(|n| n.id == node)
        .map(|n| n.block.clone())
}

fn pipeline_of(r: &PipelineResult) -> SynthPipeline {
    let params = r
        .recipe
        .nodes
        .iter()
        .flat_map(|n| {
            n.params
                .iter()
                .filter_map(move |(k, v)| v.as_f64().map(|x| (format!("{}.{k}", n.id), x)))
        })
        .filter(|(_, v)| v.is_finite())
        .take(MAX_PIPELINE_PARAMS)
        .collect();
    SynthPipeline {
        demod: block_at(r, Stage::S1).unwrap_or_else(|| "none".to_owned()),
        decode: block_at(r, Stage::S5).or_else(|| block_at(r, Stage::S4)),
        params,
        summary: r.summary.clone(),
    }
}

/// The name the reason text and `decoder_id` use: the template id, else the recipe id.
fn pipeline_name(r: &PipelineResult) -> String {
    r.template
        .as_ref()
        .map_or_else(|| r.recipe.id.clone(), |t| t.id.clone())
}

fn decode_row(
    f: &HoldoutFrame,
    r: &PipelineResult,
    job_id: &str,
    hash: &str,
    hypotheses: u64,
    class: ContentClass,
) -> Decode {
    let h = r.holdout.as_ref();
    let structural = || DecodedIdentity {
        scheme: IdentityScheme::Other(STRUCTURAL_SCHEME.into()),
        value: format!(
            "synth;{}",
            hash.trim_start_matches("sha256:")
                .get(..16)
                .unwrap_or_default()
        ),
    };
    // A template-bound identity is kept (and still marked synthesized); an open search, or an
    // identity whose scheme does not parse, gets the structural one.
    let identity = match (&r.template, &f.identity) {
        (Some(_), Some((scheme, value))) => scheme
            .parse::<IdentityScheme>()
            .ok()
            .map(|scheme| DecodedIdentity {
                scheme,
                value: value.clone(),
            })
            .unwrap_or_else(structural),
        _ => structural(),
    };
    let crc_status = if f.corrected {
        CrcStatus::Corrected
    } else if f.check_valid {
        CrcStatus::Valid
    } else {
        CrcStatus::Invalid
    };
    Decode {
        id: DecodeId::new(),
        demodulation_ref: None,
        recording_ref: None,
        decoder_id: format!(
            "synth:{}",
            r.template.as_ref().map_or("open", |t| t.id.as_str())
        ),
        decoder_version: format!("{}+{hash}", hk_synth::ENGINE),
        frame_model: f.frame_model.clone(),
        metadata: if f.metadata.is_null() {
            json!({})
        } else {
            f.metadata.clone()
        },
        content: f.content.clone().filter(|_| class.permits_content()),
        crc_status,
        identity: Some(identity),
        content_class: class,
        t: Timestamp::from_unix_nanos(f.t_ns),
        provenance: Some(DecodeProvenance::Synthesized {
            job_id: job_id.to_owned(),
            holdout: true,
            evidence_bits: h.map_or(f64::from(r.evidence_bits), |h| f64::from(h.evidence_bits)),
            hypotheses,
            analytic_holdout_bits: h.map_or(0.0, |h| f64::from(h.analytic_bits)),
            check_bits: h.and_then(|h| h.check_bits).map(f64::from),
            l_check: h.and_then(|h| h.l_check).map(f64::from),
            check_searched: h.is_none_or(|h| h.check_origin.searched()),
            template_provenance: r.template.as_ref().map(|_| {
                match h.map(|h| h.check_origin) {
                    Some(hk_synth::result::CheckOrigin::TemplateFixed) => "builtin-or-user",
                    Some(hk_synth::result::CheckOrigin::Discovered { .. }) => "discovered",
                    _ => "template",
                }
                .to_owned()
            }),
        }),
    }
}

/// Attaches a finished job's results (module docs). `Ok(None)` when there is nothing to attach:
/// the job did not finish `done`, it has no results, or it was cancelled before the attach
/// committed.
///
/// **One transaction.** Every write — the stored decodes, the new candidate they may create, the
/// `emitter_synthesis` row and the confirmation — lands together or not at all, so a failed write
/// can never leave a Confirmed emitter behind a job that says `not-attached`, and a cancel that
/// lands mid-attach undoes the lot.
pub fn attach(
    ingest: &mut Ingest,
    policy: &SynthesizedConfirm,
    input: &AttachInput<'_>,
) -> Result<Option<Attached>, RepoError> {
    let o = input.outcome;
    if o.state != hk_synth::JobState::Done || o.results.is_empty() {
        return Ok(None);
    }
    ingest.repo_mut().begin_write_batch()?;
    let out = attach_in(ingest, policy, input);
    let cancelled = input.cancelled.is_some_and(|c| c());
    match out {
        Ok(a) if !cancelled => {
            ingest.repo_mut().commit_write_batch()?;
            Ok(a)
        }
        Ok(_) => {
            ingest.repo_mut().rollback_write_batch()?;
            Ok(None)
        }
        Err(e) => {
            let _ = ingest.repo_mut().rollback_write_batch();
            Err(e)
        }
    }
}

/// [`attach`]'s writes, inside its transaction.
fn attach_in(
    ingest: &mut Ingest,
    policy: &SynthesizedConfirm,
    input: &AttachInput<'_>,
) -> Result<Option<Attached>, RepoError> {
    let o = input.outcome;
    let Some(r) = o.results.first() else {
        return Ok(None);
    };
    let hash = recipe_hash(&r.recipe);
    let center = (input.band.lo_hz + input.band.hi_hz) / 2.0;
    let width = input.band.hi_hz - input.band.lo_hz;
    let evidence_bits = r.analytic_holdout_bits.map(f64::from);
    let not_attached = |stored, valid, reason: &str| Attached {
        emitter: None,
        decodes_stored: stored,
        decodes_valid: valid,
        synthesis_written: false,
        confirm: SynthConfirmDecision {
            rule: CONFIRM_SYNTH_RULE,
            outcome: SynthConfirmOutcome::NotAttached,
            evidence_bits,
            reason: reason.to_owned(),
            decision_rate: None,
        },
    };

    // 1. The emitter: the target, else the nearest in the window. A user-deleted target is out of
    //    the inventory and a user delete wins: nothing is stored, linked or appended for it (the
    //    route refuses such a target at admission; this is the delete-during-search race).
    let mut emitter = match input.target {
        Some(e) => {
            let e = ingest.repo().live_emitter_id(e)?;
            if ingest.repo().emitter_lifecycle_state(e)? == LifecycleState::Deleted {
                return Ok(Some(not_attached(
                    0,
                    0,
                    "the target was deleted by the user; a user delete wins",
                )));
            }
            Some(e)
        }
        None => emitter_in_window(ingest.repo(), input.band, input.window)?,
    };

    // 2. Stored decodes: the solved rank-1's hold-out frames only.
    let (mut stored, mut valid) = (0u64, 0u64);
    if r.verdict == Verdict::Solved {
        let _ = ingest.take_new_emitters();
        for f in &o.holdout_frames {
            let d = decode_row(
                f,
                r,
                input.job_id,
                &hash,
                o.used.hypotheses,
                input.content_class,
            );
            let is_valid = d.crc_status == CrcStatus::Valid;
            match emitter {
                // The emitter is known: store the row and link it. A sighting here would let a
                // structural or shared-channel identity mint a second entry for this emission.
                Some(e) => {
                    let mut d = d;
                    if !policy::decode_is_allowlist_shaped(&d) {
                        policy::sanitize_decode(None, STRUCTURAL_SCHEME, &mut d);
                    }
                    let id = d.id;
                    let t = d.t;
                    ingest.repo_mut().insert_decode(&d)?;
                    ingest.repo_mut().link_emitter(&EmitterLink {
                        emitter_id: e,
                        target: LinkTarget::Decode(id),
                        linked_at: t,
                    })?;
                }
                // No emitter yet: ordinary ingestion, whose identity sighting creates the
                // candidate — for this frame only. Every later frame is stored and linked to the
                // emitter it created (the arm above): entity resolution mints a new entry per
                // sighting of a structural identity, so ingesting every frame would stack one
                // ghost candidate per frame on one emission.
                None => {
                    ingest.store_decode(d, None, None, Some(center), Some(width))?;
                    emitter = ingest.take_new_emitters().into_iter().next();
                }
            }
            stored += 1;
            valid += u64::from(is_valid);
        }
    }

    let Some(emitter) = emitter else {
        return Ok(Some(not_attached(
            stored,
            valid,
            "no inventory emitter in the analysed window, and no decode created one",
        )));
    };

    // 3. The decision, then the row, then the lifecycle — all in this transaction, and the state
    //    read here is the state the confirmation changes (the batch holds the write lock).
    let trust = ingest.repo().window_trust(emitter, input.window)?;
    let decision = policy.decide(&SynthesizedEvidence {
        profile: input.profile,
        result: r,
        pipeline: pipeline_name(r),
        suspect_fraction: (trust.detections > 0).then(|| trust.suspect_fraction()),
        overload: input.overload,
    });
    // ADR-0022 §8: every evaluation counts toward the rolling decision rate, in this transaction
    // (a rolled-back attach decided nothing), on the device's clock — decisions per device-week
    // is the budget's unit, whatever capture time the analysed window has.
    let rate = policy.decision_rate(
        ingest
            .repo_mut()
            .record_confirm_decision(CONFIRM_SYNTH_RULE, Timestamp::now())?,
    );
    let decision = match (decision, rate.void_text()) {
        (Ok(reason), Some(void)) => Ok(format!("{reason}; {void}")),
        (Err(why), Some(void)) => Err(format!("{why}; {void}")),
        (d, None) => d,
    };
    let state = ingest.repo().emitter_lifecycle_state(emitter)?;
    if state == LifecycleState::Deleted {
        // Deleted since it was chosen: a user delete wins, and the rule never resurrects.
        return Ok(Some(not_attached(
            stored,
            valid,
            "the emitter was deleted by the user; a user delete wins",
        )));
    }
    let confirm = match &decision {
        Ok(reason) if state == LifecycleState::Candidate => SynthConfirmDecision {
            rule: CONFIRM_SYNTH_RULE,
            outcome: SynthConfirmOutcome::Confirmed,
            evidence_bits,
            reason: reason.clone(),
            decision_rate: Some(rate),
        },
        Ok(reason) => SynthConfirmDecision {
            rule: CONFIRM_SYNTH_RULE,
            outcome: SynthConfirmOutcome::Already,
            evidence_bits,
            reason: format!("already confirmed; this analysis would have: {reason}"),
            decision_rate: Some(rate),
        },
        Err(why) => SynthConfirmDecision {
            rule: CONFIRM_SYNTH_RULE,
            outcome: SynthConfirmOutcome::Insufficient,
            evidence_bits,
            reason: why.clone(),
            decision_rate: Some(rate),
        },
    };

    let holdout = r.holdout.as_ref();
    let row = EmitterSynthesis {
        emitter_id: emitter,
        provenance: SYNTHESIZED_BY_OUTPUT_ANALYSIS.into(),
        engine: hk_synth::ENGINE.into(),
        t: input.window.start,
        verdict: row_verdict(r.verdict),
        stage_reached: row_stage(r.stage_reached),
        pipeline: Some(pipeline_of(r)),
        evidence: holdout
            .map_or(r.stages.as_slice(), |h| h.stages.as_slice())
            .iter()
            .map(|e| RowEvidence {
                stage: row_stage(e.stage),
                metric: name_of(e.metric),
                raw: f64::from(e.raw),
                n: u64::from(e.n),
                bits: f64::from(e.bits),
                summary: format!(
                    "{} {} = {:.3} over {} ({:.1} bits){}",
                    e.node,
                    name_of(e.metric),
                    e.raw,
                    e.n,
                    e.bits,
                    if holdout.is_some() { ", hold-out" } else { "" }
                ),
            })
            .filter(|e| e.raw.is_finite() && e.bits.is_finite())
            .collect(),
        // ADR-0021 §4.4: the node list lives for the job only; the summary persists.
        trace: Vec::new(),
        resolution: if r.verdict == Verdict::Solved {
            None
        } else {
            Some(
                input
                    .resolution
                    .map(row_resolution)
                    .unwrap_or_else(|| RowResolution {
                        kind: RowKind::Unknown,
                        deepest_verdict: Some(row_verdict(r.verdict)),
                        reason: None,
                        summary: "Searched and not identified.".into(),
                    }),
            )
        },
        receiver: None,
        job: Some(SynthesisJob {
            job_id: input.job_id.to_owned(),
            profile: name_of(input.profile),
            evidence_bits: f64::from(r.evidence_bits),
            prior_bits: f64::from(r.prior_bits),
            analytic_holdout_bits: evidence_bits,
            template: r
                .template
                .as_ref()
                .map(|t| json!({ "id": t.id, "version": t.version })),
            recipe: serde_json::to_value(&r.recipe).unwrap_or(Value::Null),
            recipe_hash: hash,
            check: r.check.as_ref().and_then(|c| serde_json::to_value(c).ok()),
            holdout: holdout.and_then(|h| serde_json::to_value(h).ok()),
            trace_summary: input.trace_summary.clone(),
            replay_key: input.replay_key.clone(),
            null_control: holdout
                .and_then(|h| h.null_control)
                .and_then(|n| serde_json::to_value(n).ok()),
            sealed_resolution: input.resolution.and_then(|r| serde_json::to_value(r).ok()),
            decodes_stored: stored,
            decodes_valid: valid,
            confirm: serde_json::to_value(&confirm).ok(),
        }),
    };
    // The row first, then the irreversible transition; the transaction makes them one.
    ingest.repo_mut().insert_synthesis(&row)?;
    if confirm.outcome == SynthConfirmOutcome::Confirmed {
        ingest.repo_mut().change_emitter_lifecycle(
            emitter,
            LifecycleState::Confirmed,
            LifecycleAuthor::Auto,
            CONFIRM_SYNTH_RULE,
            lifecycle_text(&confirm.reason),
            input.window.end,
        )?;
    }
    Ok(Some(Attached {
        emitter: Some(emitter),
        decodes_stored: stored,
        decodes_valid: valid,
        synthesis_written: true,
        confirm,
    }))
}

/// The lifecycle store refuses a reason over [`LIFECYCLE_TEXT_MAX`] bytes, and a long template
/// id plus the arithmetic plus a void budget note can reach it: the event keeps the head of the
/// sentence (cut on a character boundary), and the `emitter_synthesis` row's `confirm` keeps it
/// whole.
fn lifecycle_text(reason: &str) -> &str {
    if reason.len() <= LIFECYCLE_TEXT_MAX {
        return reason;
    }
    let mut n = LIFECYCLE_TEXT_MAX;
    while !reason.is_char_boundary(n) {
        n -= 1;
    }
    &reason[..n]
}

#[cfg(test)]
mod tests {
    //! `ConfirmPolicy.synthesized` clause by clause (ADR-0022 §4, §6). Each refusal names the
    //! clause that refused, so a reader can tell which number was close.
    use hk_synth::result::{CheckOrigin, CheckSummary, HoldoutEvidence};
    use hk_synth::trace::NullControl;

    use super::*;

    fn recipe() -> hk_recipe::Recipe {
        serde_json::from_str(include_str!("../../../../recipes/adsb.recipe.json")).unwrap()
    }

    /// A searched CRC-16 solved on hold-out: 3 differences, `L_check` 21 (ADR-0022 §4.2's row),
    /// 48 − 21 = 27 check bits, 30 analytic bits, the null control passed with 11.4 bits.
    fn solved() -> PipelineResult {
        PipelineResult {
            rank: 1,
            verdict: Verdict::Solved,
            summary: "s".into(),
            recipe: recipe(),
            template: None,
            stage_reached: Stage::S5,
            stages: Vec::new(),
            evidence_bits: 40.0,
            prior_bits: 0.0,
            analytic_holdout_bits: Some(30.0),
            check: Some(CheckSummary {
                kind: "crc".into(),
                model: "CRC-16".into(),
                width: 16,
                pass_rate: 1.0,
                distinct_valid: 3,
                corrected_excluded: 0,
                tested: 3,
                holdout: true,
            }),
            frames_preview: Vec::new(),
            characterisation: None,
            holdout: Some(HoldoutEvidence {
                evidence_bits: 40.0,
                analytic_bits: 30.0,
                check_bits: Some(27.0),
                l_check: Some(21.0),
                check_width: Some(16),
                differences: 3,
                check_origin: CheckOrigin::Searched,
                stages: Vec::new(),
                null_control: Some(NullControl {
                    k: 2,
                    ran: true,
                    best_null_bits: 28.6,
                    margin_bits: 11.4,
                    capped: false,
                }),
            }),
        }
    }

    fn decide(
        r: &PipelineResult,
        profile: Profile,
        suspect: Option<f64>,
        overload: Option<bool>,
    ) -> Result<String, String> {
        SynthesizedConfirm::default().decide(&SynthesizedEvidence {
            profile,
            result: r,
            pipeline: "generic-fsk-framed".into(),
            suspect_fraction: suspect,
            overload,
        })
    }

    fn ok(r: &PipelineResult) -> Result<String, String> {
        decide(r, Profile::Standard, Some(0.0), Some(false))
    }

    fn with(f: impl FnOnce(&mut HoldoutEvidence)) -> PipelineResult {
        let mut r = solved();
        f(r.holdout.as_mut().unwrap());
        r
    }

    /// ADR-0022 §4.2's worked table: the frame count is a formula, not a constant.
    #[test]
    fn min_differences_is_adr_0022_s4_2s_table() {
        let p = SynthesizedConfirm::default();
        for (width, l_check, want) in [
            (24, 0.0, 1), // CRC-24 template-fixed: one squitter
            (16, 0.0, 2),
            (8, 0.0, 3),
            (16, 21.0, 3),
            (8, 13.0, 5),
            (32, 37.0, 2),
        ] {
            assert_eq!(
                p.min_differences(width, l_check),
                want,
                "width {width}, L {l_check}"
            );
        }
        assert_eq!(p.min_differences(0, 0.0), u64::MAX);
    }

    #[test]
    fn a_searched_check_passing_every_clause_confirms_with_the_arithmetic_in_the_reason() {
        let reason = ok(&solved()).unwrap();
        assert_eq!(
            reason,
            "decoded by synthesized pipeline `generic-fsk-framed`: CRC-16 (searched), 3 differing \
             frame(s) valid on hold-out without correction, 48.0 − 21.0 = 27.0 check bits, 30.0 \
             analytic bits against a 24-bit threshold; null control passed with a 11.4-bit margin"
        );
    }

    #[test]
    fn a_partial_verdict_and_quick_never_confirm() {
        let mut r = solved();
        r.verdict = Verdict::Checked;
        assert!(ok(&r).unwrap_err().contains("not solved"));
        let e = decide(&solved(), Profile::Quick, Some(0.0), Some(false)).unwrap_err();
        assert!(e.contains("`quick` never confirms"), "{e}");
    }

    #[test]
    fn width_floor_hard_check_floor_and_analytic_threshold_each_refuse() {
        let e = ok(&with(|h| h.check_width = Some(7))).unwrap_err();
        assert!(e.contains("under the 8-bit floor"), "{e}");
        let e = ok(&with(|h| h.check_width = None)).unwrap_err();
        assert!(e.contains("always carries a check"), "{e}");
        // 24 analytic bits of sync excess and a thin check: not a decode.
        let e = ok(&with(|h| h.check_bits = Some(15.9))).unwrap_err();
        assert!(e.contains("16-bit hard check floor"), "{e}");
        let e = ok(&with(|h| h.check_bits = None)).unwrap_err();
        assert!(e.contains("hard check floor"), "{e}");
        let e = ok(&with(|h| h.analytic_bits = 23.9)).unwrap_err();
        assert!(e.contains("against a 24-bit threshold"), "{e}");
        let e = ok(&with(|h| h.analytic_bits = f32::NAN)).unwrap_err();
        assert!(e.contains("analytic"), "{e}");
    }

    /// ADR-0022 §5.1: an unknown inherited charge is not a zero one.
    #[test]
    fn a_discovered_template_without_its_charge_never_confirms() {
        let e = ok(&with(|h| {
            h.check_origin = CheckOrigin::Discovered {
                look_elsewhere_bits: None,
            };
            h.l_check = None;
        }))
        .unwrap_err();
        assert!(e.contains("not recorded"), "{e}");
    }

    /// ADR-0022 §5.3 / ADR-0021 §8.2: a searched check needs the null control to have run and
    /// passed; a template-fixed one does not run it.
    #[test]
    fn the_null_control_gates_a_searched_check_only() {
        let e = ok(&with(|h| h.null_control = None)).unwrap_err();
        assert!(e.contains("needs the null control"), "{e}");
        let e = ok(&with(|h| {
            h.null_control = Some(NullControl {
                k: 2,
                ran: false,
                best_null_bits: 0.0,
                margin_bits: 0.0,
                capped: false,
            })
        }))
        .unwrap_err();
        assert!(e.contains("could not run"), "{e}");
        let e = ok(&with(|h| {
            h.null_control = Some(NullControl {
                k: 2,
                ran: true,
                best_null_bits: 35.0,
                margin_bits: 5.0,
                capped: true,
            })
        }))
        .unwrap_err();
        assert!(e.contains("capped"), "{e}");
        // A discovered template counts as searched.
        let e = ok(&with(|h| {
            h.check_origin = CheckOrigin::Discovered {
                look_elsewhere_bits: Some(4.0),
            };
            h.null_control = None;
        }))
        .unwrap_err();
        assert!(e.contains("needs the null control"), "{e}");
        // Template-fixed: no control, and it confirms.
        let reason = ok(&with(|h| {
            h.check_origin = CheckOrigin::TemplateFixed;
            h.null_control = None;
        }))
        .unwrap();
        assert!(reason.contains("(template-fixed)"), "{reason}");
        assert!(!reason.contains("null control"), "{reason}");
    }

    /// ADR-0022 §4.2 (review M1): the check is worth at most `width × differences − L_check`,
    /// whatever the evaluator reports. A searched CRC-8 on a beacon repeating one payload five
    /// times has one difference: 8 − 13 = −5 bits, so it refuses even when told 40.
    #[test]
    fn check_bits_are_clamped_to_width_times_differences_less_l_check() {
        let e = ok(&with(|h| {
            h.check_width = Some(8);
            h.differences = 1;
            h.l_check = Some(13.0);
            h.check_bits = Some(40.0);
            h.analytic_bits = 45.0;
        }))
        .unwrap_err();
        assert!(e.contains("hard check floor"), "{e}");
        assert!(e.contains("-5.0 bits"), "{e}");
        // Clamped check bits come off the analytic total too: 3 differences of CRC-16, reported
        // 40 check bits but worth 27, with 30 analytic → 17 after the 13-bit excess is removed.
        let e = ok(&with(|h| h.check_bits = Some(40.0))).unwrap_err();
        assert!(e.contains("17.0 analytic hold-out bits"), "{e}");
    }

    /// ADR-0015 §5.5 condition 4, failing closed on what was not measured.
    #[test]
    fn front_end_trust_refuses_suspect_overloaded_or_unmeasured_windows() {
        let r = solved();
        let e = decide(&r, Profile::Standard, Some(0.51), Some(false)).unwrap_err();
        assert!(
            e.contains("51 % of the window's detections are suspect"),
            "{e}"
        );
        assert!(decide(&r, Profile::Standard, Some(0.5), Some(false)).is_ok());
        let e = decide(&r, Profile::Standard, None, Some(false)).unwrap_err();
        assert!(e.contains("no detection of this emitter"), "{e}");
        let e = decide(&r, Profile::Standard, Some(0.0), Some(true)).unwrap_err();
        assert!(e.contains("overloaded"), "{e}");
        let e = decide(&r, Profile::Standard, Some(0.0), None).unwrap_err();
        assert!(e.contains("unknown"), "{e}");
    }

    #[test]
    fn a_disabled_rule_confirms_nothing() {
        let p = SynthesizedConfirm {
            enabled: false,
            ..SynthesizedConfirm::default()
        };
        let r = solved();
        let e = p
            .decide(&SynthesizedEvidence {
                profile: Profile::Deep,
                result: &r,
                pipeline: "p".into(),
                suspect_fraction: Some(0.0),
                overload: Some(false),
            })
            .unwrap_err();
        assert!(e.contains("disabled"), "{e}");
    }

    // -----------------------------------------------------------------------------------------
    // T-575: the ticket's named cases, ADR-0022 §2, §4.2, §5.1, §8
    // -----------------------------------------------------------------------------------------

    /// One ADS-B-like squitter: a CRC-24 frame and an 8-pulse preamble's sync excess.
    fn crc24_single_frame(origin: CheckOrigin, l_check: f32) -> PipelineResult {
        let mut r = solved();
        let c = r.check.as_mut().unwrap();
        c.model = "CRC-24".into();
        c.width = 24;
        c.distinct_valid = 1;
        c.tested = 1;
        let h = r.holdout.as_mut().unwrap();
        h.check_width = Some(24);
        h.differences = 1;
        h.l_check = Some(l_check);
        h.check_bits = Some(24.0 - l_check);
        h.analytic_bits = 24.0 - l_check + 6.0;
        h.check_origin = origin;
        if !origin.searched() {
            h.null_control = None;
        }
        r
    }

    /// ADR-0022 §4.2's first row: a template-fixed CRC-24 needs one difference, so one clean
    /// squitter confirms; the same frame with its generator searched (L_check ≈ 24 + 5) does not.
    #[test]
    fn t575_a_template_fixed_crc24_single_frame_confirms_and_the_same_frame_searched_does_not() {
        let reason = ok(&crc24_single_frame(CheckOrigin::TemplateFixed, 0.0)).unwrap();
        assert!(
            reason.contains("CRC-24 (template-fixed), 1 differing frame"),
            "{reason}"
        );
        assert!(reason.contains("24.0 − 0.0 = 24.0 check bits"), "{reason}");
        let e = ok(&crc24_single_frame(CheckOrigin::Searched, 29.0)).unwrap_err();
        assert!(e.contains("hard check floor"), "{e}");
        assert!(e.contains("3 needed"), "{e}");
    }

    /// ADR-0022 §4.2: `differences`, not `distinct_valid`. A template-fixed CRC-8 beacon repeating
    /// one payload eight times has eight valid frames and one difference; whatever its check
    /// reports, it is worth one frame's 8 bits, and the repeat count confirms nothing.
    #[test]
    fn t575_a_repeated_payload_beacon_does_not_confirm_on_its_repeat_count() {
        let mut r = crc24_single_frame(CheckOrigin::TemplateFixed, 0.0);
        let c = r.check.as_mut().unwrap();
        c.model = "CRC-8".into();
        c.width = 8;
        c.distinct_valid = 8;
        c.tested = 8;
        let h = r.holdout.as_mut().unwrap();
        h.check_width = Some(8);
        h.differences = 1;
        h.check_bits = Some(64.0);
        h.analytic_bits = 70.0;
        let e = ok(&r).unwrap_err();
        assert!(e.contains("8.0 bits after L_check 0.0"), "{e}");
        assert!(e.contains("1 differing frames of 8 bits; 3 needed"), "{e}");
        // With eight *different* payloads the same frames carry 64 check bits, and confirm.
        let h = r.holdout.as_mut().unwrap();
        h.differences = 8;
        assert!(ok(&r).is_ok());
    }

    /// ADR-0022 §4.1: the rank currency does not pay. 70 `evidence_bits`, 22 of them analytic.
    #[test]
    fn t575_seventy_evidence_bits_of_which_22_are_analytic_do_not_confirm() {
        let mut r = solved();
        r.evidence_bits = 70.0;
        r.analytic_holdout_bits = Some(22.0);
        let h = r.holdout.as_mut().unwrap();
        h.evidence_bits = 70.0;
        h.analytic_bits = 22.0;
        h.check_bits = Some(22.0);
        let e = ok(&r).unwrap_err();
        assert!(
            e.contains("22.0 analytic hold-out bits against a 24-bit threshold"),
            "{e}"
        );
    }

    /// ADR-0022 §2.3: the gate's whole input is the hold-out evidence of the hypothesis being
    /// confirmed plus the window's trust. The job-total `coverage.look_elsewhere_bits` is not a
    /// field of [`SynthesizedEvidence`] or of [`PipelineResult`], so it cannot reach the gate;
    /// what can move the decision is only this hypothesis's own numbers. (The engine side — a
    /// job total larger than the analytic ladder, and the result still solving — is
    /// `hk-synth`'s `t575_the_job_total_look_elsewhere_never_reaches_the_solve_or_confirm_key`.)
    #[test]
    fn t575_the_job_total_look_elsewhere_never_reaches_the_gate() {
        let r = solved();
        let ev = SynthesizedEvidence {
            profile: Profile::Standard,
            result: &r,
            pipeline: "generic-fsk-framed".into(),
            suspect_fraction: Some(0.0),
            overload: Some(false),
        };
        // Exhaustive destructuring: a field added here must be argued for.
        let SynthesizedEvidence {
            profile: _,
            result: _,
            pipeline: _,
            suspect_fraction: _,
            overload: _,
        } = &ev;
        assert!(SynthesizedConfirm::default().decide(&ev).is_ok());
    }

    /// ADR-0022 §5.1: a discovered template's check confirms only when its `L_check` includes
    /// the inherited discovery charge. A row claiming less than it inherited is refused.
    #[test]
    fn t575_a_discovered_template_confirms_only_with_its_inherited_l_charged() {
        let discovered = CheckOrigin::Discovered {
            look_elsewhere_bits: Some(20.0),
        };
        // S5's own count 1 bit + 20 inherited = 21: 48 − 21 = 27 check bits, and it confirms.
        let reason = ok(&with(|h| h.check_origin = discovered)).unwrap();
        assert!(reason.contains("48.0 − 21.0 = 27.0 check bits"), "{reason}");
        // The same row with the inherited charge left out of L_check.
        let e = ok(&with(|h| {
            h.check_origin = discovered;
            h.l_check = Some(1.0);
            h.check_bits = Some(47.0);
        }))
        .unwrap_err();
        assert!(e.contains("inherited"), "{e}");
        // And an unknown inherited charge with an L_check that pretends to know it.
        let e = ok(&with(|h| {
            h.check_origin = CheckOrigin::Discovered {
                look_elsewhere_bits: None,
            };
        }))
        .unwrap_err();
        assert!(e.contains("not recorded"), "{e}");
    }

    /// ADR-0022 §8: the budget claim holds up to the assumed rate and is void strictly above it;
    /// the thresholds do not move either way.
    #[test]
    fn t575_the_decision_rate_voids_the_budget_claim_above_the_assumed_rate() {
        let p = SynthesizedConfirm::default();
        let at = p.decision_rate(20_000);
        assert_eq!(at.budget_claim, crate::inventory::BudgetClaim::Holds);
        assert_eq!(at.void_text(), None);
        let over = p.decision_rate(20_001);
        assert_eq!(over.budget_claim, crate::inventory::BudgetClaim::Void);
        assert_eq!(over.assumed_per_week, 20_000);
        let text = over.void_text().unwrap();
        assert!(
            text.contains("20001 decisions in 7 days > 20000 assumed"),
            "{text}"
        );
        assert_eq!(
            serde_json::to_value(over).unwrap(),
            json!({"decisions_7d": 20001, "assumed_per_week": 20000, "budget_claim": "void"})
        );
        // The same evidence decides the same way at any count: no uptime-dependent charge.
        assert!(ok(&solved()).is_ok());
    }

    #[test]
    fn t575_a_long_reason_is_cut_to_the_lifecycle_limit_on_a_char_boundary() {
        let long = "−".repeat(LIFECYCLE_TEXT_MAX);
        let cut = lifecycle_text(&long);
        assert!(cut.len() <= LIFECYCLE_TEXT_MAX && !cut.is_empty());
        assert!(long.starts_with(cut));
        assert_eq!(lifecycle_text("short"), "short");
    }
}
