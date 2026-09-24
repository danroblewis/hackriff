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
        .filter(|e| {
            (e.f_center_hz - center).abs() <= 0.5 * width && e.bandwidth_hz <= 2.0 * width
        })
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
/// the job did not finish `done`, or it has no results.
pub fn attach(
    ingest: &mut Ingest,
    policy: &SynthesizedConfirm,
    input: &AttachInput<'_>,
) -> Result<Option<Attached>, RepoError> {
    let o = input.outcome;
    if o.state != hk_synth::JobState::Done {
        return Ok(None);
    }
    let Some(r) = o.results.first() else {
        return Ok(None);
    };
    let hash = recipe_hash(&r.recipe);
    let center = (input.band.lo_hz + input.band.hi_hz) / 2.0;
    let width = input.band.hi_hz - input.band.lo_hz;

    // 1. The emitter: the target, else the nearest in the window.
    let mut emitter = match input.target {
        Some(e) => Some(e),
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
                // No emitter: ordinary ingestion, whose identity sighting creates the candidate.
                None => {
                    ingest.store_decode(d, None, None, Some(center), Some(width))?;
                }
            }
            stored += 1;
            valid += u64::from(is_valid);
        }
        if emitter.is_none() {
            emitter = ingest.take_new_emitters().into_iter().next();
        }
    }

    let evidence_bits = r.analytic_holdout_bits.map(f64::from);
    let Some(emitter) = emitter else {
        return Ok(Some(Attached {
            emitter: None,
            decodes_stored: stored,
            decodes_valid: valid,
            synthesis_written: false,
            confirm: SynthConfirmDecision {
                rule: CONFIRM_SYNTH_RULE,
                outcome: SynthConfirmOutcome::NotAttached,
                evidence_bits,
                reason: "no inventory emitter in the analysed window, and no decode created one"
                    .into(),
            },
        }));
    };

    // 3. The decision, then the row, then the lifecycle.
    let trust = ingest.repo().window_trust(emitter, input.window)?;
    let decision = policy.decide(&SynthesizedEvidence {
        profile: input.profile,
        result: r,
        pipeline: pipeline_name(r),
        suspect_fraction: trust.suspect_fraction(),
        overload: input.overload,
    });
    let mut confirm = match &decision {
        Ok(reason) => SynthConfirmDecision {
            rule: CONFIRM_SYNTH_RULE,
            outcome: SynthConfirmOutcome::Confirmed,
            evidence_bits,
            reason: reason.clone(),
        },
        Err(why) => SynthConfirmDecision {
            rule: CONFIRM_SYNTH_RULE,
            outcome: SynthConfirmOutcome::Insufficient,
            evidence_bits,
            reason: why.clone(),
        },
    };
    if let Ok(reason) = &decision {
        match ingest.repo_mut().change_emitter_lifecycle(
            emitter,
            LifecycleState::Confirmed,
            LifecycleAuthor::Auto,
            CONFIRM_SYNTH_RULE,
            reason,
            input.window.end,
        ) {
            Ok(Some(_)) => {}
            Ok(None) => {
                confirm.outcome = SynthConfirmOutcome::Already;
                confirm.reason = format!("already confirmed; this analysis would have: {reason}");
            }
            // Deleted by the user: a user delete wins, and the rule never resurrects.
            Err(RepoError::NotFound { .. }) => {
                confirm.outcome = SynthConfirmOutcome::Already;
                confirm.reason = "deleted by the user; a user delete wins".into();
            }
            Err(e) => return Err(e),
        }
    }

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
            Some(input.resolution.map(row_resolution).unwrap_or_else(|| {
                RowResolution {
                    kind: RowKind::Unknown,
                    deepest_verdict: Some(row_verdict(r.verdict)),
                    reason: None,
                    summary: "Searched and not identified.".into(),
                }
            }))
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
    let written = match ingest.repo_mut().insert_synthesis(&row) {
        Ok(_) => true,
        // The emitter vanished (a user delete between search and attach): nothing to append to.
        Err(RepoError::NotFound { .. }) => false,
        Err(e) => return Err(e),
    };
    Ok(Some(Attached {
        emitter: Some(emitter),
        decodes_stored: stored,
        decodes_valid: valid,
        synthesis_written: written,
        confirm,
    }))
}
