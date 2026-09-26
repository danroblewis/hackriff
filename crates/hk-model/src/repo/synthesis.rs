//! **Why that decoding pipeline was chosen, in terms of what was measured** (T-546; ADR-0015 §5.4,
//! ADR-0021).
//!
//! An emitter's `emitter_synthesis` row is the durable answer to "the system decoded this — how
//! did it know how?". It carries the chosen [`SynthPipeline`], the per-stage [`StageEvidence`] the
//! choice rests on, the [`TraceNode`] record of what else was considered and why each alternative
//! left the search, and — when nothing won — a sealed [`Resolution`] saying what that means.
//!
//! **A black box that happens to decode P25 is not the capability** (T-546's acceptance), and this
//! is the object that makes it not one. `POST /api/analyze {emitter_id}` serves it.
//!
//! # The three rules this file exists to keep
//!
//! 1. **Tried and not-tried are structurally different**, never inferred from prose. Every
//!    [`Outcome`] is one or the other; [`TraceNode::measured`] is `None` for every not-tried kind
//!    and `Some` for every tried one, and [`EmitterSynthesis::validate`] refuses a row that breaks
//!    it. This is the decode-side statement of the canvas rule that grey means *genuinely
//!    unobserved* — "we did not look at PSK" and "PSK measured 4 bits against a 6-bit floor" are
//!    different answers with different next actions (ADR-0021 §2.2).
//! 2. **`not-searched` is not `unknown`** (ADR-0021 §7A.4). An emitter with no row at all is
//!    un-looked-at, and the API says so rather than reporting an empty search as a negative
//!    result.
//! 3. **Append-only.** Re-analysing writes a new row; nothing rewrites what a previous search
//!    concluded, exactly as [`super::refined`] treats a refinement.
//!
//! # What is deliberately not here yet
//!
//! ADR-0021's bound on trace residency (`max_trace_nodes`, the elision priority order and the
//! `elided` counters) and its shuffled-null control (§8.2) belong to the general MAUTO beam
//! search, which does not exist. This build's producer is the trunking chain, whose hypothesis set
//! is a handful of framings rather than a beam, so every node it opens is recorded and
//! [`MAX_TRACE_NODES`] is a safety bound rather than an elision policy. A producer that can exceed
//! it is a producer that needs ADR-0021 §2.3 implemented.

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::{RepoError, Repository, blob};
use crate::detection::DetectionFlags;
use crate::ids::EmitterId;
use crate::region::TimeRange;
use crate::time::Timestamp;

/// Provenance of every synthesis row.
pub const SYNTHESIZED_BY_OUTPUT_ANALYSIS: &str = "synthesized by output analysis";

/// Most rows [`Repository::synthesis_history`] returns.
pub const SYNTHESIS_HISTORY_MAX: usize = 200;

/// Most trace nodes one row may carry (ADR-0021 §2.3's bound, as a refusal rather than an
/// elision policy — see the module docs).
pub const MAX_TRACE_NODES: usize = 512;

const ENSURE_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS emitter_synthesis (
    synthesis_id INTEGER PRIMARY KEY,
    emitter_id   BLOB    NOT NULL REFERENCES emitter (emitter_id),
    t            INTEGER NOT NULL,
    verdict      TEXT    NOT NULL,
    body         TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_emitter_synthesis_emitter
    ON emitter_synthesis (emitter_id, t, synthesis_id);
CREATE TRIGGER IF NOT EXISTS emitter_synthesis_append_only
    BEFORE UPDATE ON emitter_synthesis
    BEGIN SELECT RAISE(ABORT, 'syntheses are append-only'); END;";

/// How deep the search got (ADR-0015 §3.4; the ladder is closed and unchanged by ADR-0021).
///
/// It says **how far**, never what the result means: that is [`Resolution`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// S0 only: energy, nothing structured.
    Energy,
    /// S1: a demodulator locked.
    Demodulated,
    /// S2–S3: a symbol clock and bits.
    Clocked,
    /// S4: framing found.
    Framed,
    /// S5 partial: a check exists and some frames pass.
    Checked,
    /// S5 on hold-out meets the solve rule.
    Solved,
}

/// The stage ladder (ADR-0015 §1.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    /// Channel: centre, bandwidth, in-band SNR.
    S0Channel,
    /// Demodulator family and its parameters.
    S1Demod,
    /// Symbol clock.
    S2Clock,
    /// Bits.
    S3Bits,
    /// Framing.
    S4Framing,
    /// Check.
    S5Check,
    /// Fields.
    S6Fields,
}

/// The pipeline the engine chose.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynthPipeline {
    /// Demodulator id, e.g. `c4fm`.
    pub demod: String,
    /// Framing/decoder id, e.g. `p25-tsbk`; `None` when the search stopped before framing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decode: Option<String>,
    /// The measured parameters the pipeline was bound with, e.g. `symbol_rate_bd`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<(String, f64)>,
    /// Backend-rendered one-line statement of the choice, so no client does wording logic
    /// (ADR-0015 §3.4).
    pub summary: String,
}

/// One stage's measured evidence for the chosen pipeline (ADR-0015 §2.1/§3.4).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StageEvidence {
    /// Stage.
    pub stage: Stage,
    /// Metric name, in the ADR-0015 §2.1 vocabulary where one fits.
    pub metric: String,
    /// The metric in its natural unit.
    pub raw: f64,
    /// Support: symbols, bursts or distinct frames.
    pub n: u64,
    /// Significance against the metric's null, bits.
    pub bits: f64,
    /// Backend-rendered statement of what this measured.
    pub summary: String,
}

/// Why a hypothesis left the search (ADR-0021 §2.2; closed — adding a variant is a contract
/// change).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    // ---- Tried: `measured` is `Some`.
    /// Stayed in the beam and was expanded.
    Survived,
    /// Measured below its stage floor.
    PrunedFloor,
    /// Optimistic bound below the best complete result.
    PrunedBound,
    /// Scored, but outside the beam width or the diversity cap.
    PrunedBeam,
    /// A complete candidate that finished below the winner.
    EvaluatedWorse,
    /// Replaced by its refined child.
    RefinedInto,
    /// Prefix-hash hit: reused another node's stage output.
    Memoised,
    // ---- Not tried: `measured` is `None`.
    /// Posterior below the defer threshold; the budget never reached it.
    DeferredPrior,
    /// Enqueued and ready, but a budget cap hit first.
    DeferredBudget,
    /// No block exists for the structure (ADR-0015 §8).
    Unsupported,
    /// The alternative is inconsistent with a fixed ancestor choice.
    NotApplicable,
    /// The power policy refused the expansion.
    RefusedPower,
}

impl Outcome {
    /// Whether the hypothesis was actually evaluated.
    ///
    /// **The load-bearing distinction of the whole trace.** It is derived here and *served*, so no
    /// client ever infers it (ADR-0021 §2.1).
    pub fn tried(self) -> bool {
        !matches!(
            self,
            Outcome::DeferredPrior
                | Outcome::DeferredBudget
                | Outcome::Unsupported
                | Outcome::NotApplicable
                | Outcome::RefusedPower
        )
    }
}

/// What a tried node measured.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Measured {
    /// Metric name.
    pub metric: String,
    /// The metric in its natural unit.
    pub raw: f64,
    /// Support.
    pub n: u64,
    /// Significance, bits.
    pub bits: f64,
}

/// One decision the engine made (ADR-0021 §2.1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TraceNode {
    /// Node id, unique within the row.
    pub id: String,
    /// Parent node id; `None` at the root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Stage the decision was taken at.
    pub stage: Stage,
    /// The structural alternative this node fixes, e.g. `four-level-fm` or `p25-phase-1`.
    pub choice: String,
    /// The hypothesis family, for filtering ("why not DMR").
    pub family: String,
    /// Where the hypothesis came from: `estimate | template | classification | signature |
    /// proposal | open`.
    pub seed_source: String,
    /// The measurement, `None` for every not-tried outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured: Option<Measured>,
    /// Outcome.
    pub outcome: Outcome,
    /// Whether it was evaluated — derived from [`Outcome::tried`] and served, never inferred.
    pub tried: bool,
    /// Evaluations charged to this node.
    pub evaluations: u32,
    /// Backend-rendered statement of the decision.
    pub summary: String,
}

impl TraceNode {
    /// Starts a node. It is finished by exactly one of [`Node::tried`] or [`Node::not_tried`],
    /// **which is the point**: the two halves of ADR-0021 §2.2's distinction are different calls
    /// taking different arguments, so a node cannot be built that claims a measurement it does
    /// not have or omits one it does. [`EmitterSynthesis::validate`] is the backstop for a row
    /// assembled some other way (deserialised, hand-edited).
    pub fn at(
        id: impl Into<String>,
        stage: Stage,
        family: impl Into<String>,
        choice: impl Into<String>,
    ) -> Node {
        Node {
            id: id.into(),
            parent: None,
            stage,
            family: family.into(),
            choice: choice.into(),
            seed_source: "open".into(),
            evaluations: 0,
        }
    }

    /// Sets the parent.
    pub fn child_of(mut self, parent: impl Into<String>) -> Self {
        self.parent = Some(parent.into());
        self
    }
}

/// A [`TraceNode`] under construction ([`TraceNode::at`]).
#[derive(Clone, Debug)]
pub struct Node {
    id: String,
    parent: Option<String>,
    stage: Stage,
    family: String,
    choice: String,
    seed_source: String,
    evaluations: u32,
}

impl Node {
    /// Where the hypothesis came from: `estimate | template | classification | signature |
    /// proposal | open` (default `open`).
    pub fn seed(mut self, source: impl Into<String>) -> Self {
        self.seed_source = source.into();
        self
    }

    /// Sets the parent node.
    pub fn child_of(mut self, parent: impl Into<String>) -> Self {
        self.parent = Some(parent.into());
        self
    }

    /// Evaluations charged to this node.
    pub fn evaluations(mut self, n: u32) -> Self {
        self.evaluations = n;
        self
    }

    /// Finishes a hypothesis that **was evaluated**: it carries what it measured.
    pub fn tried(
        self,
        outcome: Outcome,
        measured: Measured,
        summary: impl Into<String>,
    ) -> TraceNode {
        self.finish(outcome, Some(measured), summary)
    }

    /// Finishes a hypothesis that was **never looked at** — deferred, unsupported, inapplicable or
    /// refused. It carries no measurement, because there is none: "we did not look" is a different
    /// answer from "we looked and it scored badly", and this is where that stays true.
    pub fn not_tried(self, outcome: Outcome, summary: impl Into<String>) -> TraceNode {
        self.finish(outcome, None, summary)
    }

    fn finish(
        self,
        outcome: Outcome,
        measured: Option<Measured>,
        summary: impl Into<String>,
    ) -> TraceNode {
        TraceNode {
            id: self.id,
            parent: self.parent,
            stage: self.stage,
            choice: self.choice,
            family: self.family,
            seed_source: self.seed_source,
            measured,
            outcome,
            tried: outcome.tried(),
            evaluations: self.evaluations,
            summary: summary.into(),
        }
    }
}

/// What it means that nothing was solved (ADR-0021 §7A.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResolutionKind {
    /// A finished search found nothing.
    Unknown,
    /// Framed, check-valid and unidentified — a result, not a failure (ADR-0021 §7A.5).
    StructuredUnidentified,
    /// The highest-posterior suspicion has no block in this build.
    UnsupportedStructure,
    /// **No finished search exists.** Never rendered as `unknown`.
    NotSearched,
}

/// Why nothing won (ADR-0021 §7A.3; closed).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResolutionReason {
    /// There was not energy to analyse.
    NoSignal,
    /// Energy existed; everything tried measured below its floor.
    NothingScored,
    /// Two or more complete candidates within the supersession margin.
    Tied,
    /// The queue was non-empty when the budget ran out.
    BudgetExhausted,
    /// The highest-posterior suspicion has no block.
    UnsupportedStructure,
}

/// Where a suggestion came from (ADR-0021 §9.1; closed).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExplanationSource {
    /// A spectrum-allocation row.
    Alloc,
    /// A band plan (channel raster, service convention).
    BandPlan,
    /// A licence or assignment record.
    Licence,
    /// A catalogue of known emitters (satellites, beacons).
    Catalogue,
    /// This device's own earlier observations.
    History,
}

/// How the measurement sits against the reference (ADR-0021 §9.1; closed).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExplanationStatus {
    /// The measurement is where the reference says this service should be.
    Expected,
    /// The measurement disagrees with the reference — **the interesting case** (CLAUDE.md). It is
    /// a flag, never a correction: the emitter keeps its measured centre.
    Unexpected,
    /// Nothing in the reference data covers this measurement. **Not** "this is fine".
    NoReferenceData,
}

/// A ranked suggestion about what a **sealed** [`Resolution`] might be (ADR-0021 §9.1).
///
/// **A suggestion explains a result; it never becomes one.** It is computed by `hk-context` after
/// the resolution is final, from measured parameters only, and modifies nothing on the row: an
/// `unknown` with three high-scoring explanations is still `unknown` (ADR-0021 §9.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Explanation {
    /// Which reference said so.
    pub source: ExplanationSource,
    /// What it names, e.g. `fm-broadcast`.
    pub identity: String,
    /// How well the measurement fits, 0..=1. Ordering only: it never confirms anything.
    pub score: f64,
    /// Measured centre minus the reference's nearest expected frequency, Hz; `None` when the
    /// reference names a band rather than a channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distance_hz: Option<f64>,
    /// Expected / unexpected / no reference data.
    pub status: ExplanationStatus,
    /// Age of the reference data, days; `None` when the source does not date itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_age_days: Option<u32>,
    /// Why this suggestion, in words the user can check against the measurement.
    pub reasoning: String,
}

/// What an [`ResolutionKind::UnsupportedStructure`] row suspected (ADR-0021 §7A.6), summarised
/// onto the row so ADR-0021 §9.4's backlog — *"3 emitters are waiting on `psk_demod`"* — is a
/// group-by over `emitter_synthesis` and not a search through summary prose. The full sealed
/// object (with the posterior and who suspected it) stays in `job.sealed_resolution`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuspectedStructure {
    /// The structure (`css`, `ofdm`).
    pub structure: String,
    /// The missing block's stable id (`css_dechirp`).
    pub missing_block: String,
}

/// One row of [`Repository::missing_block_backlog`] (ADR-0021 §9.4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingBlockCount {
    /// The block this build does not have, e.g. `psk_demod`.
    pub missing_block: String,
    /// The structure it would decode, e.g. `psk`.
    pub structure: String,
    /// Emitters whose latest analysis is waiting on it.
    pub emitters: u64,
}

/// The sealed negative result (ADR-0021 §7A.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Resolution {
    /// What the absence means.
    pub kind: ResolutionKind,
    /// How deep the search got; `None` for [`ResolutionKind::NotSearched`], which ruled nothing
    /// out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deepest_verdict: Option<Verdict>,
    /// Why nothing won; `None` for [`ResolutionKind::NotSearched`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<ResolutionReason>,
    /// The structure with no block, on an `unsupported-structure` row and nowhere else: without
    /// it the absence of a LoRa decode reads exactly like a LoRa signal decoded as noise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspected: Option<SuspectedStructure>,
    /// Backend-rendered statement.
    pub summary: String,
    /// Ranked known-signal suggestions, attached **after** everything above was sealed and
    /// modifying none of it (ADR-0021 §9.2). Written only by
    /// [`Resolution::attach_explanations`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub explanations: Vec<Explanation>,
}

impl Resolution {
    /// The answer for an emitter nothing has analysed: **un-looked-at, not found-nothing.**
    pub fn not_searched() -> Self {
        Self {
            kind: ResolutionKind::NotSearched,
            deepest_verdict: None,
            reason: None,
            suspected: None,
            summary: "no analysis has run on this emitter: not searched, which is not the same \
                      as searched and unidentified"
                .into(),
            explanations: Vec::new(),
        }
    }

    /// **The only way a suggestion reaches a resolution** (ADR-0021 §9.2/§9.3).
    ///
    /// Replaces `explanations` and touches nothing else: `kind`, `deepest_verdict`, `reason`,
    /// `suspected` and `summary` are what the search sealed, whatever the suggestions say. An
    /// `unknown` explained by three high-scoring allocations is still `unknown`.
    pub fn attach_explanations(&mut self, explanations: Vec<Explanation>) {
        self.explanations = explanations;
    }
}

/// What the receiver itself contributed, measured rather than assumed (docs/19 §7.6a).
///
/// **A clock error is a property of the receiver, not of any signal**, so it is recorded once per
/// analysis and applies to every emission in the same capture — which is both how it is recognised
/// and what makes it cheap.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReceiverFit {
    /// Channel grid the offset was fitted against, Hz.
    pub grid_hz: f64,
    /// Fitted offset of the received emissions from the assumed grid, Hz.
    pub offset_hz: f64,
    /// Concentration of the fit, 0–1.
    pub concentration: f64,
    /// The same offset expressed against the tuned centre, parts per million.
    pub ppm: f64,
    /// Which **absolute** offset the modulo-grid fit above really is, and the evidence (T-628).
    ///
    /// `offset_hz` names a grid, not a receiver: +4300 Hz and −8200 Hz are the same 12.5 kHz grid.
    /// Reaching an absolute frequency — a trunking grant — needs the alias settled. Absent on rows
    /// written before T-628, which is the same answer as [`AliasState::NotTried`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<ReceiverAlias>,
}

/// Whether the receiver's grid alias was tried, and what came of it (T-628).
///
/// ADR-0021's rule: **not tried is a different answer from tried and unsettled**, and a client
/// must not have to guess which.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AliasState {
    /// Nothing asked for an absolute frequency, or nothing could be measured against.
    NotTried,
    /// One alias chosen; [`ReceiverAlias::offset_hz`] carries it.
    Resolved,
    /// Tried, and the evidence did not single one out. Nothing absolute was measured.
    Unresolved,
}

/// What decided — or failed to decide — the alias.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AliasEvidence {
    /// The receiver's clock bound admits a single alias.
    ClockBound,
    /// Exactly one alias found energy on more granted channels than any other.
    GrantedChannelEnergy,
    /// No granted channel inside the window: nothing needed an absolute frequency.
    NoGrantedChannel,
    /// No quiet channel to set a threshold against, so energy could not be measured.
    NoReference,
    /// The fitted offset is beyond the clock bound: no alias is admissible.
    NoAliasInBound,
    /// No alias found energy on any granted channel.
    NothingOccupied,
    /// Two or more aliases found energy on equally many granted channels.
    Tied,
}

/// The settled (or unsettled) absolute receiver offset (T-628).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReceiverAlias {
    /// Tried or not, and what came of it.
    pub state: AliasState,
    /// Why.
    pub evidence: AliasEvidence,
    /// The absolute offset, Hz, only when [`AliasState::Resolved`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset_hz: Option<f64>,
    /// The same, ppm of the tuned centre.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ppm: Option<f64>,
    /// The receiver clock bound the aliases were drawn from, ppm.
    pub bound_ppm: f64,
    /// Aliases the bound admitted (and so were tried, when measured).
    pub candidates: u32,
    /// Granted channels measured under each alias.
    pub targets: u32,
    /// Granted channels the best alias found carrying a transmission.
    pub occupied: u32,
    /// The best any other alias managed.
    pub runner_up: u32,
}

/// Most bytes one stored recipe document may take (a recipe is a handful of nodes; this bounds a
/// row, not a design).
pub const MAX_RECIPE_BYTES: usize = 64 * 1024;

/// A region-analyze job's contribution to its row (ADR-0015 §5.4 as amended by ADR-0021 §11.2 and
/// ADR-0022 §11.3; MAUTO M-9). Absent on rows a chain's own analysis wrote (the trunking chain).
///
/// The types it carries live in `hk-synth`, which depends on this crate; they are stored as their
/// served JSON, exactly as the job serves them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynthesisJob {
    /// The analyze job, `a<n>`.
    pub job_id: String,
    /// Profile (`quick`, `standard`, `deep`).
    pub profile: String,
    /// Rank-1 `evidence_bits` (search rank key). Reported; never the confirm key.
    pub evidence_bits: f64,
    /// Rank-1 `prior_bits`. Reported only; never ranks.
    pub prior_bits: f64,
    /// Rank-1 hold-out analytic bits (ADR-0022 §2), the confirm key; `None` when not validated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analytic_holdout_bits: Option<f64>,
    /// The template rank 1 came from (`{id, version}`), or `None` for open search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<serde_json::Value>,
    /// The rank-1 recipe document, inline. Startable as-is (`POST /api/pipelines`).
    pub recipe: serde_json::Value,
    /// `sha256:<hex>` of the recipe's canonical JSON — the `decoder_version` suffix of its stored
    /// decodes.
    pub recipe_hash: String,
    /// The check summary, when S5 was reached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<serde_json::Value>,
    /// The rank-1 hold-out evidence (ADR-0022 §6's inputs), when validated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holdout: Option<serde_json::Value>,
    /// ADR-0021 §4.1, persisted so a second look reads what earlier looks covered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_summary: Option<serde_json::Value>,
    /// ADR-0021 §5: what makes this search reproducible, and so comparable with a later one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_key: Option<serde_json::Value>,
    /// ADR-0021 §8.2, whether or not it fired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub null_control: Option<serde_json::Value>,
    /// The `hk-synth`-sealed resolution (ADR-0021 §7A.2), when nothing solved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sealed_resolution: Option<serde_json::Value>,
    /// Decode rows stored from the hold-out run, and how many were valid without correction.
    pub decodes_stored: u64,
    /// Of those, CRC-valid.
    pub decodes_valid: u64,
    /// The `ConfirmPolicy.synthesized` decision (`{rule, outcome, evidence_bits, reason}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<serde_json::Value>,
}

/// One analysis of one emitter.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EmitterSynthesis {
    /// Emitter (the live id when written).
    pub emitter_id: EmitterId,
    /// Always [`SYNTHESIZED_BY_OUTPUT_ANALYSIS`].
    pub provenance: String,
    /// Engine and version, e.g. `hk-pipeline/trunk-synth@0.1.0`.
    pub engine: String,
    /// Time of the IQ this analysed.
    #[serde(rename = "t_ns", alias = "t")]
    pub t: Timestamp,
    /// How deep it got.
    pub verdict: Verdict,
    /// Deepest stage reached.
    pub stage_reached: Stage,
    /// The pipeline chosen; `None` when nothing was chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline: Option<SynthPipeline>,
    /// Per-stage evidence for the choice.
    #[serde(default)]
    pub evidence: Vec<StageEvidence>,
    /// What was considered and why each alternative left.
    #[serde(default)]
    pub trace: Vec<TraceNode>,
    /// The sealed negative result; `None` only when [`Verdict::Solved`] was reached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<Resolution>,
    /// What the receiver contributed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver: Option<ReceiverFit>,
    /// The region-analyze job that wrote this row (M-9); `None` for a chain's own analysis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<SynthesisJob>,
}

impl EmitterSynthesis {
    /// Checks the rules in the module docs.
    pub fn validate(&self) -> Result<(), RepoError> {
        let bad = |why: String| Err(RepoError::Invalid(format!("synthesis: {why}")));
        if self.provenance != SYNTHESIZED_BY_OUTPUT_ANALYSIS {
            return bad(format!(
                "provenance must be {SYNTHESIZED_BY_OUTPUT_ANALYSIS:?}"
            ));
        }
        if self.engine.trim().is_empty() {
            return bad(
                "an engine name is required: an answer nobody authored is not evidence".into(),
            );
        }
        if self.trace.len() > MAX_TRACE_NODES {
            return bad(format!(
                "{} trace nodes exceeds MAX_TRACE_NODES ({MAX_TRACE_NODES}); a producer that can \
                 reach this needs ADR-0021 §2.3's elision policy, not a bigger bound",
                self.trace.len(),
            ));
        }
        for n in &self.trace {
            if n.tried != n.outcome.tried() {
                return bad(format!(
                    "trace node {:?} says tried={} for outcome {:?}: tried and not-tried are \
                     structural, never editorial (ADR-0021 §2.2)",
                    n.id, n.tried, n.outcome,
                ));
            }
            if n.outcome.tried() != n.measured.is_some() {
                return bad(format!(
                    "trace node {:?} has outcome {:?} and measured={}: a tried node carries a \
                     measurement and a not-tried one carries none",
                    n.id,
                    n.outcome,
                    if n.measured.is_some() { "some" } else { "none" },
                ));
            }
        }
        if self.verdict != Verdict::Solved && self.resolution.is_none() {
            return bad(
                "an unsolved analysis must carry a Resolution: an absence with no statement of \
                 what it means is exactly what ADR-0021 forbids"
                    .into(),
            );
        }
        if self
            .resolution
            .as_ref()
            .is_some_and(|r| r.kind == ResolutionKind::NotSearched)
        {
            return bad(
                "a stored row IS a finished search, so it can never resolve `not-searched` \
                 (ADR-0021 §7A.4)"
                    .into(),
            );
        }
        if let Some(r) = &self.resolution {
            let names = r.suspected.is_some();
            if (r.kind == ResolutionKind::UnsupportedStructure) != names {
                return bad(
                    "`unsupported-structure` names the structure and the missing block, and no \
                     other kind claims one: without the name, the absence of a decode reads \
                     exactly like the signal decoded as noise (ADR-0021 §7A.6)"
                        .into(),
                );
            }
        }
        let finite = self
            .evidence
            .iter()
            .flat_map(|e| [e.raw, e.bits])
            .chain(self.trace.iter().flat_map(|n| {
                n.measured
                    .as_ref()
                    .map(|m| [m.raw, m.bits])
                    .unwrap_or([0.0, 0.0])
            }))
            .chain(
                self.pipeline
                    .iter()
                    .flat_map(|p| p.params.iter().map(|(_, v)| *v)),
            );
        if finite.into_iter().any(|v| !v.is_finite()) {
            return bad("non-finite number".into());
        }
        if let Some(j) = &self.job {
            if j.job_id.trim().is_empty() {
                return bad("a job row names its job".into());
            }
            if !j.recipe_hash.starts_with("sha256:") {
                return bad("recipe_hash must be `sha256:<hex>`".into());
            }
            if !j.recipe.is_object() {
                return bad("the recipe is stored inline as a document".into());
            }
            if serde_json::to_string(&j.recipe).map_or(true, |t| t.len() > MAX_RECIPE_BYTES) {
                return bad(format!("the recipe exceeds {MAX_RECIPE_BYTES} bytes"));
            }
            let nums = [
                Some(j.evidence_bits),
                Some(j.prior_bits),
                j.analytic_holdout_bits,
            ];
            if nums.into_iter().flatten().any(|v| !v.is_finite()) {
                return bad("non-finite job number".into());
            }
            if j.decodes_valid > j.decodes_stored {
                return bad("more valid decodes than stored ones".into());
            }
        }
        Ok(())
    }
}

/// Most detections [`Repository::window_trust`] reads.
pub const MAX_TRUST_DETECTIONS: usize = 4096;

/// An emitter's detections over a window, read for ADR-0015 §5.5 condition 4 (front-end trust).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowTrust {
    /// Linked detections overlapping the window (capped at [`MAX_TRUST_DETECTIONS`]).
    pub detections: u64,
    /// Of those, suspect: clipped, spur, image, IMD or compressed — the measurement's own flags
    /// OR'd with a standing retune verdict's (the same reading as the relation evidence).
    pub suspect: u64,
}

impl WindowTrust {
    /// Suspect share, `0` when nothing was detected (no detection is no suspicion).
    pub fn suspect_fraction(&self) -> f64 {
        if self.detections == 0 {
            0.0
        } else {
            self.suspect as f64 / self.detections as f64
        }
    }
}

/// The flag bits of an emitter's linked detections overlapping `[?2, ?3)`, with the standing
/// retune verdict's bits; reached through current track links or direct links, like the relation
/// evidence. `?4` caps the rows.
const WINDOW_TRUST_SQL: &str = "\
     SELECT flags, retune_bits FROM ( \
       SELECT d.flags AS flags, d.t_start AS t_start, \
              coalesce((SELECT dr.flag_bits FROM detection_retune dr \
                        WHERE dr.detection_id = d.detection_id AND dr.active = 1 \
                          AND dr.verdict_id = (SELECT max(verdict_id) FROM detection_retune \
                                               WHERE detection_id = d.detection_id)), 0) \
                AS retune_bits \
       FROM emitter_link el \
       JOIN track_detection td ON td.track_id = el.target_id \
       JOIN detection d ON d.detection_id = td.detection_id \
       WHERE el.emitter_id = ?1 AND el.target_kind = 'track' AND el.superseded_by IS NULL \
         AND d.t_end > ?2 AND d.t_start < ?3 \
       UNION ALL \
       SELECT d.flags AS flags, d.t_start AS t_start, \
              coalesce((SELECT dr.flag_bits FROM detection_retune dr \
                        WHERE dr.detection_id = d.detection_id AND dr.active = 1 \
                          AND dr.verdict_id = (SELECT max(verdict_id) FROM detection_retune \
                                               WHERE detection_id = d.detection_id)), 0) \
                AS retune_bits \
       FROM emitter_link el \
       JOIN detection d ON d.detection_id = el.target_id \
       WHERE el.emitter_id = ?1 AND el.target_kind = 'detection' AND el.superseded_by IS NULL \
         AND d.t_end > ?2 AND d.t_start < ?3 \
     ) ORDER BY t_start DESC LIMIT ?4";

impl Repository {
    /// Front-end trust of `emitter`'s detections over `window` (ADR-0015 §5.5 condition 4:
    /// "≤ 50 % suspect detections"). Follows merges to the live id.
    pub fn window_trust(
        &self,
        emitter: EmitterId,
        window: TimeRange,
    ) -> Result<WindowTrust, RepoError> {
        let id = self.live_emitter_id(emitter)?;
        let mut stmt = self.conn.prepare_cached(WINDOW_TRUST_SQL)?;
        let rows = stmt
            .query_map(
                params![
                    blob(id),
                    window.start.as_unix_nanos(),
                    window.end.as_unix_nanos(),
                    MAX_TRUST_DETECTIONS as i64
                ],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let mut t = WindowTrust::default();
        for (flags, retune) in rows {
            let f = DetectionFlags::from_bits(u32::try_from(flags | retune).unwrap_or(0));
            t.detections += 1;
            if f.clipped || f.spur_candidate || f.image_candidate || f.suspect_imd || f.compressed {
                t.suspect += 1;
            }
        }
        Ok(t)
    }

    pub(super) fn ensure_synthesis_table(&self) -> Result<(), RepoError> {
        self.conn.execute_batch(ENSURE_TABLE)?;
        Ok(())
    }

    /// Stores an analysis under the live emitter; returns the stored row.
    pub fn insert_synthesis(
        &mut self,
        synthesis: &EmitterSynthesis,
    ) -> Result<EmitterSynthesis, RepoError> {
        synthesis.validate()?;
        self.ensure_synthesis_table()?;
        let id = self.live_emitter_id(synthesis.emitter_id)?;
        // Existence, and the same refusal `insert_refined_tuning` makes: a row about an emitter
        // that is not there is not a finding.
        self.emitter(id)?;
        let row = EmitterSynthesis {
            emitter_id: id,
            ..synthesis.clone()
        };
        let tx = self.write_tx()?;
        tx.execute(
            "INSERT INTO emitter_synthesis (emitter_id, t, verdict, body) VALUES (?1, ?2, ?3, ?4)",
            params![
                blob(id),
                row.t.as_unix_nanos(),
                serde_json::to_string(&row.verdict)?,
                serde_json::to_string(&row)?
            ],
        )?;
        tx.commit()?;
        Ok(row)
    }

    /// Every analysis of `emitter` (its live id and emitters merged into it), newest first.
    pub fn synthesis_history(
        &self,
        emitter: EmitterId,
    ) -> Result<Vec<EmitterSynthesis>, RepoError> {
        self.synthesis_rows(emitter, SYNTHESIS_HISTORY_MAX)
    }

    /// At most `limit` analyses of `emitter`, newest first.
    fn synthesis_rows(
        &self,
        emitter: EmitterId,
        limit: usize,
    ) -> Result<Vec<EmitterSynthesis>, RepoError> {
        self.ensure_synthesis_table()?;
        let id = self.live_emitter_id(emitter)?;
        let mut stmt = self.conn.prepare_cached(
            "WITH RECURSIVE absorbed(id) AS ( \
                 SELECT ?1 \
                 UNION SELECT e.emitter_id FROM emitter e JOIN absorbed a ON e.merged_into = a.id \
             ) \
             SELECT body FROM emitter_synthesis \
             WHERE emitter_id IN (SELECT id FROM absorbed) \
             ORDER BY t DESC, synthesis_id DESC LIMIT ?2",
        )?;
        let texts = stmt
            .query_map(params![blob(id), limit as i64], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        texts
            .iter()
            .map(|t| serde_json::from_str(t).map_err(RepoError::from))
            .collect()
    }

    /// The current (latest) analysis of `emitter`, if any.
    ///
    /// `None` means **not searched** ([`Resolution::not_searched`]), never "searched and found
    /// nothing" — the caller must not collapse the two (ADR-0021 §7A.4).
    pub fn synthesis(&self, emitter: EmitterId) -> Result<Option<EmitterSynthesis>, RepoError> {
        // One row, not the history: `/api/inventory` reads this for every row it serves.
        Ok(self.synthesis_rows(emitter, 1)?.into_iter().next())
    }

    /// **The `missing_block` backlog** (ADR-0021 §9.4): for each block this build does not have,
    /// how many emitters are waiting on it — *"3 emitters are waiting on `psk_demod`"*.
    ///
    /// A group-by over `emitter_synthesis`, counted from each emitter's **latest** row only (an
    /// emitter re-analysed four times is one emitter waiting, not four), and only where that row
    /// resolved `unsupported-structure`. It is the device's own argument for which block to build
    /// next — made from what it met, not from a guess about what users will meet.
    pub fn missing_block_backlog(&self) -> Result<Vec<MissingBlockCount>, RepoError> {
        self.ensure_synthesis_table()?;
        let mut stmt = self.conn.prepare_cached(
            "SELECT body FROM emitter_synthesis s \
             WHERE s.synthesis_id = ( \
                 SELECT synthesis_id FROM emitter_synthesis x \
                 WHERE x.emitter_id = s.emitter_id ORDER BY x.t DESC, x.synthesis_id DESC LIMIT 1 \
             )",
        )?;
        let bodies = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<String>, _>>()?;
        let mut counts: std::collections::BTreeMap<(String, String), u64> =
            std::collections::BTreeMap::new();
        for body in &bodies {
            let row: EmitterSynthesis = serde_json::from_str(body)?;
            let Some(res) = row.resolution else { continue };
            if res.kind != ResolutionKind::UnsupportedStructure {
                continue;
            }
            let Some(sus) = res.suspected else { continue };
            *counts
                .entry((sus.missing_block, sus.structure))
                .or_insert(0) += 1;
        }
        let mut out: Vec<MissingBlockCount> = counts
            .into_iter()
            .map(|((missing_block, structure), emitters)| MissingBlockCount {
                missing_block,
                structure,
                emitters,
            })
            .collect();
        // Most-wanted first; ties by block id, so the answer is stable.
        out.sort_by(|a, b| {
            b.emitters
                .cmp(&a.emitters)
                .then_with(|| a.missing_block.cmp(&b.missing_block))
        });
        Ok(out)
    }

    /// Whether `emitter`'s decoded identity rests **only** on synthesized decodes (decoder id
    /// `synth:…`, ADR-0015 §5.5 trust rules): `None` without an identity; `Some(true)` when at
    /// least one synthesized decode carries it and no ordinary decoder's does. `/api/inventory`
    /// shows it beside the identity, so a synthesized identity — even a real one such as
    /// `adsb-icao` from a template-bound pipeline — is strong evidence, never an unexplained fact.
    /// Metadata only: the value never leaves this call.
    pub fn identity_synthesized(&self, emitter: EmitterId) -> Result<Option<bool>, RepoError> {
        let id = self.live_emitter_id(emitter)?;
        let tx = self.read_tx()?;
        let row: Option<(Option<String>, Option<String>)> = tx
            .prepare_cached(
                "SELECT identity_scheme, identity_value FROM emitter WHERE emitter_id = ?1",
            )?
            .query_row([blob(id)], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let Some((Some(scheme), Some(value))) = row else {
            return Ok(None);
        };
        let (synth, other): (i64, i64) = tx
            .prepare_cached(
                "SELECT coalesce(sum(decoder_id LIKE 'synth:%'), 0), \
                        coalesce(sum(decoder_id NOT LIKE 'synth:%'), 0) \
                 FROM decode WHERE identity_scheme = ?1 AND identity_value = ?2",
            )?
            .query_row(params![scheme, value], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(Some(synth > 0 && other == 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(outcome: Outcome, measured: Option<Measured>) -> TraceNode {
        let n = TraceNode::at("n1", Stage::S1Demod, "fsk", "four-level-fm")
            .seed("estimate")
            .evaluations(1);
        match measured {
            Some(m) => n.tried(outcome, m, "…"),
            None => n.not_tried(outcome, "…"),
        }
    }

    fn measured() -> Measured {
        Measured {
            metric: "bimodality".into(),
            raw: 0.71,
            n: 4096,
            bits: 9.1,
        }
    }

    fn row(trace: Vec<TraceNode>, verdict: Verdict, res: Option<Resolution>) -> EmitterSynthesis {
        EmitterSynthesis {
            emitter_id: EmitterId::new(),
            provenance: SYNTHESIZED_BY_OUTPUT_ANALYSIS.into(),
            engine: "test@1".into(),
            t: Timestamp::UNIX_EPOCH,
            verdict,
            stage_reached: Stage::S5Check,
            pipeline: None,
            evidence: Vec::new(),
            trace,
            resolution: res,
            receiver: None,
            job: None,
        }
    }

    /// **The rule the whole trace is for.** A node that says it was tried must carry what it
    /// measured, and a node that was not tried must carry nothing — because "we did not look" and
    /// "we looked and it scored 4 bits" are different answers, and a reader must never have to
    /// guess which one a row is.
    #[test]
    fn tried_and_not_tried_are_structural_and_a_row_that_confuses_them_is_refused() {
        assert!(Outcome::PrunedFloor.tried());
        assert!(!Outcome::Unsupported.tried());
        assert!(!Outcome::DeferredBudget.tried());

        // Consistent rows pass.
        for (o, m) in [
            (Outcome::PrunedFloor, Some(measured())),
            (Outcome::Survived, Some(measured())),
            (Outcome::Unsupported, None),
            (Outcome::DeferredPrior, None),
        ] {
            row(vec![node(o, m)], Verdict::Solved, None)
                .validate()
                .unwrap_or_else(|e| panic!("{o:?} should validate: {e}"));
        }
        // A not-tried node claiming a measurement is refused …
        let mut n = node(Outcome::Unsupported, Some(measured()));
        n.tried = false;
        assert!(row(vec![n], Verdict::Solved, None).validate().is_err());
        // … as is a tried node with none …
        let mut n = node(Outcome::PrunedFloor, None);
        n.tried = true;
        assert!(row(vec![n], Verdict::Solved, None).validate().is_err());
        // … and so is a hand-edited `tried` flag that disagrees with its own outcome.
        let mut n = node(Outcome::Unsupported, None);
        n.tried = true;
        assert!(row(vec![n], Verdict::Solved, None).validate().is_err());
    }

    /// An unsolved analysis with no statement of what the absence means is the defect ADR-0021
    /// exists to prevent.
    #[test]
    fn an_unsolved_analysis_must_say_what_the_absence_means() {
        assert!(row(Vec::new(), Verdict::Framed, None).validate().is_err());
        assert!(
            row(
                Vec::new(),
                Verdict::Framed,
                Some(Resolution {
                    kind: ResolutionKind::StructuredUnidentified,
                    deepest_verdict: Some(Verdict::Framed),
                    reason: Some(ResolutionReason::NothingScored),
                    suspected: None,
                    summary: "framed, unidentified".into(),
                    explanations: Vec::new(),
                }),
            )
            .validate()
            .is_ok()
        );
    }

    /// **ADR-0021 §9.4's backlog**: *"3 emitters are waiting on `psk_demod`"*, counted from the
    /// device's own data. One emitter waiting is one emitter, however many times it was
    /// re-analysed, and only its **latest** analysis counts — a row superseded by one that found
    /// the structure after all is not a vote for building the block.
    #[test]
    fn the_missing_block_backlog_counts_each_waiting_emitter_once_from_its_latest_row() {
        use crate::LinkTarget;
        use crate::cluster::{Fingerprint, Sighting};
        use crate::ids::TrackId;

        let mut repo = Repository::open_in_memory().unwrap();
        let t = |s: i64| Timestamp::from_unix_nanos(s * 1_000_000_000);
        let emitter_at = |repo: &mut Repository, f: f64| {
            repo.record_sighting(
                &Sighting {
                    source: LinkTarget::Track(TrackId::new()),
                    seen: TimeRange::new(t(0), t(5)),
                    count: 3,
                    f_center_hz: f,
                    bandwidth_hz: 12e3,
                    fingerprint: Some(Fingerprint::new(f, 12e3)),
                    identity: None,
                    context: None,
                    classification: None,
                    tags: Vec::new(),
                },
                None,
            )
            .unwrap()
            .emitter_id
        };
        let waiting = |block: &str, structure: &str| Resolution {
            kind: ResolutionKind::UnsupportedStructure,
            deepest_verdict: Some(Verdict::Demodulated),
            reason: Some(ResolutionReason::UnsupportedStructure),
            suspected: Some(SuspectedStructure {
                structure: structure.to_owned(),
                missing_block: block.to_owned(),
            }),
            summary: format!("no {block} in this build"),
            explanations: Vec::new(),
        };
        let write = |repo: &mut Repository, id: EmitterId, at: i64, res: Resolution| {
            let mut row = row(Vec::new(), Verdict::Demodulated, Some(res));
            row.emitter_id = id;
            row.t = t(at);
            repo.insert_synthesis(&row).unwrap();
        };

        // Three emitters wait on psk_demod; one of them was analysed twice and still waits.
        for (i, f) in [915e6, 916e6, 917e6].into_iter().enumerate() {
            let id = emitter_at(&mut repo, f);
            if i == 0 {
                write(&mut repo, id, 1, waiting("psk_demod", "psk"));
            }
            write(&mut repo, id, 2, waiting("psk_demod", "psk"));
        }
        // One waits on css_dechirp.
        let lora = emitter_at(&mut repo, 868e6);
        write(&mut repo, lora, 2, waiting("css_dechirp", "css"));
        // One waited, then a later analysis solved it: it is no longer waiting on anything.
        let solved = emitter_at(&mut repo, 869e6);
        write(&mut repo, solved, 1, waiting("ofdm_sync", "ofdm"));
        let mut done = row(Vec::new(), Verdict::Solved, None);
        done.emitter_id = solved;
        done.t = t(3);
        repo.insert_synthesis(&done).unwrap();
        // And one emitter nothing analysed at all: not-searched is not a vote (§7A.4).
        let _unlooked = emitter_at(&mut repo, 870e6);

        let backlog = repo.missing_block_backlog().unwrap();
        assert_eq!(
            backlog,
            vec![
                MissingBlockCount {
                    missing_block: "psk_demod".into(),
                    structure: "psk".into(),
                    emitters: 3,
                },
                MissingBlockCount {
                    missing_block: "css_dechirp".into(),
                    structure: "css".into(),
                    emitters: 1,
                },
            ],
            "most-wanted first, each emitter counted once, and nothing solved or un-looked-at \
             counted at all"
        );
    }

    /// ADR-0021 §7A.6 (T-567): `unsupported-structure` is the one kind that must name what it
    /// suspected and which block is missing, and no other kind may claim one. Without the name,
    /// the absence of a LoRa decode is indistinguishable from a LoRa signal decoded as noise —
    /// and ADR-0021 §9.4's "3 emitters are waiting on `psk_demod`" backlog cannot be counted.
    #[test]
    fn an_unsupported_structure_row_must_name_the_block_it_is_waiting_on() {
        let res = |kind, suspected| {
            Some(Resolution {
                kind,
                deepest_verdict: Some(Verdict::Demodulated),
                reason: Some(ResolutionReason::UnsupportedStructure),
                suspected,
                summary: "no block for it".into(),
                explanations: Vec::new(),
            })
        };
        let css = || {
            Some(SuspectedStructure {
                structure: "css".into(),
                missing_block: "css_dechirp".into(),
            })
        };
        assert!(
            row(
                Vec::new(),
                Verdict::Demodulated,
                res(ResolutionKind::UnsupportedStructure, None)
            )
            .validate()
            .is_err(),
            "unsupported-structure with nothing named is the defect §7A.6 exists for"
        );
        assert!(
            row(
                Vec::new(),
                Verdict::Demodulated,
                res(ResolutionKind::UnsupportedStructure, css())
            )
            .validate()
            .is_ok()
        );
        assert!(
            row(
                Vec::new(),
                Verdict::Demodulated,
                res(ResolutionKind::Unknown, css())
            )
            .validate()
            .is_err(),
            "an `unknown` may not borrow a missing block it never suspected"
        );
        // It is a field, not prose: a client reads it without parsing the summary.
        let v = serde_json::to_value(res(ResolutionKind::UnsupportedStructure, css())).unwrap();
        assert_eq!(v["kind"], "unsupported-structure");
        assert_eq!(v["suspected"]["missing_block"], "css_dechirp");
        // …and it is absent, not null, on every other kind.
        let v = serde_json::to_value(Resolution::not_searched()).unwrap();
        assert!(v.get("suspected").is_none());
        assert_eq!(v["kind"], "not-searched");
    }

    /// A stored row is a finished search by definition, so it can never mean "not searched".
    #[test]
    fn a_stored_row_can_never_resolve_not_searched() {
        assert!(
            row(
                Vec::new(),
                Verdict::Energy,
                Some(Resolution::not_searched())
            )
            .validate()
            .is_err()
        );
        assert_eq!(Resolution::not_searched().deepest_verdict, None);
        assert_eq!(Resolution::not_searched().reason, None);
    }

    /// The verdict ladder orders by depth, which is what lets `deepest_verdict` be a `max`.
    #[test]
    fn the_verdict_ladder_orders_by_depth() {
        assert!(Verdict::Solved > Verdict::Checked);
        assert!(Verdict::Checked > Verdict::Framed);
        assert!(Verdict::Framed > Verdict::Clocked);
        assert!(Verdict::Clocked > Verdict::Demodulated);
        assert!(Verdict::Demodulated > Verdict::Energy);
    }

    /// The wire vocabulary is kebab-case and stable: clients match on these strings.
    #[test]
    fn the_wire_vocabulary_is_stable() {
        assert_eq!(
            serde_json::to_string(&Verdict::Solved).unwrap(),
            "\"solved\""
        );
        assert_eq!(
            serde_json::to_string(&Stage::S4Framing).unwrap(),
            "\"s4-framing\""
        );
        assert_eq!(
            serde_json::to_string(&Outcome::PrunedFloor).unwrap(),
            "\"pruned-floor\""
        );
        assert_eq!(
            serde_json::to_string(&ResolutionKind::NotSearched).unwrap(),
            "\"not-searched\""
        );
        assert_eq!(
            serde_json::to_string(&ResolutionReason::BudgetExhausted).unwrap(),
            "\"budget-exhausted\""
        );
    }

    #[test]
    fn a_non_finite_number_is_refused() {
        let mut r = row(Vec::new(), Verdict::Solved, None);
        r.evidence.push(StageEvidence {
            stage: Stage::S0Channel,
            metric: "snr".into(),
            raw: f64::NAN,
            n: 1,
            bits: 1.0,
            summary: String::new(),
        });
        assert!(r.validate().is_err());
    }

    /// T-628: a receiver fit written before alias resolution existed still reads, as "not
    /// tried"; a resolved one states its absolute offset and evidence in kebab-case words; and an
    /// unresolved one carries NO offset, so nothing downstream can mistake it for a measurement.
    #[test]
    fn a_receiver_alias_round_trips_and_its_absence_reads_as_not_tried() {
        let old: ReceiverFit = serde_json::from_value(serde_json::json!({
            "grid_hz": 12500.0, "offset_hz": 4300.0, "concentration": 0.7, "ppm": 5.05,
        }))
        .unwrap();
        assert_eq!(old.alias, None);

        let resolved = ReceiverAlias {
            state: AliasState::Resolved,
            evidence: AliasEvidence::GrantedChannelEnergy,
            offset_hz: Some(-8200.0),
            ppm: Some(-9.6),
            bound_ppm: 20.0,
            candidates: 3,
            targets: 3,
            occupied: 3,
            runner_up: 2,
        };
        let fit = ReceiverFit {
            alias: Some(resolved),
            ..old
        };
        let v = serde_json::to_value(fit).unwrap();
        assert_eq!(v["alias"]["state"], "resolved");
        assert_eq!(v["alias"]["evidence"], "granted-channel-energy");
        assert_eq!(v["alias"]["offset_hz"], -8200.0);
        assert_eq!(serde_json::from_value::<ReceiverFit>(v).unwrap(), fit);

        let unresolved = ReceiverAlias {
            state: AliasState::Unresolved,
            evidence: AliasEvidence::Tied,
            offset_hz: None,
            ppm: None,
            ..resolved
        };
        let v = serde_json::to_value(unresolved).unwrap();
        assert_eq!(v["state"], "unresolved");
        assert_eq!(v["evidence"], "tied");
        assert!(v.get("offset_hz").is_none(), "{v}");
    }
}
