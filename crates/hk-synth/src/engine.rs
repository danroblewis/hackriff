//! The search engine (ADR-0015 §3.1–§3.4, MAUTO M-3, T-854): staged beam search with prefix
//! memoisation, floor and optimistic-bound pruning, count budgets with wall/CPU backstops, the
//! stop rules, power and throttle — and, per ADR-0021 §3, the search trace, written at every
//! site that removes a node from the frontier.
//!
//! # The seam: [`Evaluator`]
//!
//! The engine never touches IQ, blocks or `hk_estimate::assist` directly. An [`Evaluator`] runs
//! one stage of a candidate prefix over the search or hold-out window (M-2's batch `run_window`
//! over `Block::evidence` implements it) and serves proposal operators (M-4's adapters). The
//! engine owns everything above that: which hypotheses exist, which run, in what order, what
//! they cost, what is kept, and why each one left the frontier. Tests drive it with a synthetic
//! evaluator whose hidden truth the engine never sees.
//!
//! # The loop (ADR-0015 §3.1)
//!
//! 1. **Seed**: [`SearchSpec::roots`] — skeletons or templates with their priors (§4.2 / M-5).
//!    Roots whose family ADR-0016's likelihood rule deferred wait in a side queue and run only if
//!    the main pass ends with budget left.
//! 2. **Expand by stage**, k = 0…6. For each beam node whose skeleton has a slot at k, every slot
//!    alternative becomes children: discrete parameters (`enum`, `hex`, `proposal`) make one child
//!    per combination (by prior; beyond [`MAX_DISCRETE_CHILDREN`] they are recorded as not
//!    tried, never dropped quietly); continuous parameters
//!    (`float`, `int`) are **swept, not branched** — a coordinate grid around the seed (§3.1
//!    step 3: log scale ×{½, 1, 2} ± 3 steps of 1 %; linear ± 3 steps over the range) collapsed
//!    onto the child as `swept` descriptors (ADR-0021 §1 rule 2). A skeleton without a slot at k
//!    passes through.
//! 3. **Evaluate** on the search window. Stage outputs are cached by node in an LRU bounded by
//!    `max_cache_bytes`, so a child runs only its new stage; an evicted parent output is
//!    recomputed from its deepest cached ancestor and charged. A child whose hypothesis (parent
//!    prefix, alternative, discrete binding, sweep) was already evaluated is **memoised**: it
//!    reuses the earlier node's result for no new work (ADR-0021 §1 rule 3). It competes in the
//!    beam like any child and its children name it as their parent, but the trace records it as
//!    `memoised {reused}` rather than its beam fate: that fate is the reused node's measurement.
//! 4. **Score and prune** (§1.3): `b_j` below `floor_j` → `pruned_floor`; an optimistic rank
//!    below the best complete result → `pruned_bound`; outside beam width W (8 at S0–S2, 4 at
//!    S3–S6) or over 2 per (family, skeleton) → `pruned_beam`. A child with no further slot is
//!    **complete** and leaves the beam for the ranked results.
//! 5. **Validate** on the hold-out window: early, for any node at S5+ whose search evidence
//!    already meets the solve rule (that is how `solved` can be first to hit), and at the end for
//!    the top 3 complete candidates. Only hold-out evidence solves.
//!
//! Local refinement (§3.1 step 6, `refined_into`) runs M-7's [`crate::EvidenceObjective`] over
//! IQ, which this engine never touches: it needs a refine call on the [`Evaluator`] seam, which
//! arrives with the IQ-backed evaluator (M-6/M-8). Until then this engine never emits
//! `refined_into`.
//!
//! # Determinism (ADR-0021 §5)
//!
//! Given the same spec, evaluator and a job bounded by counts, every decision is identical run to
//! run, **including across thread counts**: plans are made sequentially, evaluated in parallel,
//! merged by index, and the evaluation cap is reserved per plan before dispatch. A stop caused by
//! wall or CPU time, a plateau measured against wall time, a throttle, or a thermal pause marks
//! the outcome [`SearchOutcome::nondeterministic`].
//!
//! # Scores
//!
//! `evidence_bits = Σ_{j in chain} min(b_j, cap_j) − L_j`, with `L_j = log₂(hypotheses tried at
//! stage j in this job)`, and `b_j` combined per block by [`crate::evidence::combine_stage_bits`]
//! (§13.1). Beam decisions read `L_j` as it stands when the level is scored; results are
//! re-scored with the job's final counts, so every result pays the same look-elsewhere cost.
//! `prior_bits` orders the beam and never ranks (§1.3).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hk_estimate::assist::{Budget as AssistBudget, DEFAULT_MAX_OPS};
use hk_recipe::{
    Catalogue, FieldMap, InputSpec, NodeSpec, OutputKind, OutputPolicy, OutputSpec, Recipe,
    RecipeError,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::admission::{Control, PowerPolicy, halve};
use crate::candidate::{Candidate, CandidateError, Domain, FreeParam, Scale, SeedSource};
use crate::evidence::{
    Evidence, EvidenceSet, MetricId, combine_stage_bits, look_elsewhere_bits, quality_from_bits,
};
use crate::proposal::ProposalOp;
use crate::result::{
    CheckOrigin, CheckSummary, HoldoutEvidence, HoldoutFrame, MAX_HOLDOUT_FRAMES, PipelineResult,
    StageEvidence, TemplateRef, Verdict,
};
use crate::search::{JobState, NULL_CONTROL_SHARE, Profile, StopReason, SynthBudget};
use crate::skeleton::Skeleton;
use crate::stage::{Stage, default_cap_bits, default_floor_bits};
use crate::trace::{
    BeamCause, BlockVersion, BudgetCoverage, Coverage, FamilyCoverage, FamilyState,
    MIN_NULL_MARGIN_BITS, Measured, NullControl, Outcome, OutcomeKind, Reason, ReplayBudget,
    ReplayKey, SkeletonCoverage, Spent, Swept, TraceBounds, TraceHypothesis, TraceNode,
};
use crate::trace_sink::{Trace, TraceSink};

/// Most children one slot alternative makes from its discrete parameters. Combinations beyond
/// it (lowest prior first) are **not dropped quietly**: they are recorded as one not-tried
/// `deferred_budget` node on that alternative, whose summary says how many.
pub const MAX_DISCRETE_CHILDREN: usize = 256;
/// Plans evaluated between control checks. Fixed, so check points do not depend on threads.
pub const CHUNK: usize = 8;
/// Most ranked results (ADR-0015 §5.2: `results ≤ 10`).
pub const MAX_RESULTS: usize = 10;
/// Complete candidates validated on hold-out at the end (§3.1 step 7).
pub const VALIDATE_TOP: usize = 3;
/// Beam slots per identical (family, skeleton) (§3.1 step 5).
pub const DIVERSITY_CAP: usize = 2;
/// Plateau: no best-score gain above this many bits…
pub const PLATEAU_GAIN_BITS: f32 = 1.0;
/// …over this fraction of the budget (§3.3).
pub const PLATEAU_FRACTION: f64 = 0.25;
/// Evaluations held back from the search so end-of-search hold-out validation of
/// [`VALIDATE_TOP`] seven-stage chains always fits (a quarter of the cap at most).
pub const VALIDATION_RESERVE: u64 = (VALIDATE_TOP as u64) * 7;

/// Beam width W at `stage` (§3.1 step 5).
pub const fn beam_width(stage: Stage) -> usize {
    match stage {
        Stage::S0 | Stage::S1 | Stage::S2 => 8,
        _ => 4,
    }
}

// ---------------------------------------------------------------------------------------------
// The seam
// ---------------------------------------------------------------------------------------------

/// Which window an evaluation reads (ADR-0015 §3.1 step 1; `EvalDepth` maps onto it in M-7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalWindow {
    /// The first 60 % (or the odd bursts).
    Search,
    /// The rest. Only hold-out evidence solves or confirms.
    Holdout,
    /// ADR-0021 §8.2's `k`-th **null** window, derived by the evaluator from the hold-out IQ at
    /// the same gain state: `0` the time-reversed copy, `1…` phase-randomised surrogates (same
    /// magnitude spectrum, randomised phase — PSD and SNR kept, symbol timing and framing
    /// destroyed). An evaluator that cannot make one answers an error, and the control then did
    /// not run, which is never a pass.
    Null(u32),
}

/// One stage evaluation asked of an [`Evaluator`].
pub struct EvalRequest<'a, O> {
    /// Search or hold-out.
    pub window: EvalWindow,
    /// The stage being run.
    pub stage: Stage,
    /// The whole prefix, every parameter of the new stage bound. A valid recipe (§1.2).
    pub candidate: &'a Candidate,
    /// The indices in `candidate.recipe.nodes` this stage adds; everything before them is the
    /// parent's prefix, whose output is `parent`.
    pub new_nodes: Range<usize>,
    /// The parent prefix's stage output on the same window; `None` for the first slot (read the
    /// window itself).
    pub parent: Option<&'a O>,
}

/// One block's evidence from an evaluation (§13.1 combines per block, then sums).
#[derive(Clone, Debug, PartialEq)]
pub struct NodeEvidence {
    /// Recipe node id.
    pub node: String,
    /// What its `Block::evidence` emitted.
    pub evidence: EvidenceSet,
}

/// An evaluation's answer.
pub struct Evaluated<O> {
    /// Per block. Entries for other stages are ignored.
    pub evidence: Vec<NodeEvidence>,
    /// The stage output children read.
    pub output: O,
    /// Its resident size, for the memo cache.
    pub output_bytes: u64,
    /// The check summary, when this stage is a check.
    pub check: Option<CheckSummary>,
    /// Decoded frames, when this stage produces them. Read only from the rank-1 prefix's
    /// hold-out run (the attach step's stored decodes, ADR-0015 §5.5); ignored elsewhere.
    pub frames: Vec<HoldoutFrame>,
}

/// Why an evaluation could not run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvalError {
    /// The source ended (stop `source_ended`).
    SourceEnded,
    /// The window left the ring (stop `evicted`).
    Evicted,
    /// Anything else: the job fails, keeping what it had.
    Failed(String),
}

/// A proposal-operator call (ADR-0015 §3.2).
pub struct ProposeRequest<'a, O> {
    /// Which operator.
    pub op: ProposalOp,
    /// The free parameter whose domain names it.
    pub path: &'a str,
    /// The stage the parameter belongs to.
    pub stage: Stage,
    /// The prefix with this stage's nodes appended, the proposed parameter still unbound (not
    /// necessarily a valid recipe yet).
    pub candidate: &'a Candidate,
    /// The parent prefix's output on the search window — the "node's own bits or frames".
    pub parent: Option<&'a O>,
    /// This call's share of the job's assist-op pool.
    pub budget: AssistBudget,
}

/// One suggestion: parameter bindings and the prior they earn. The score feeds `prior_bits`,
/// never `evidence_bits` (§3.2).
#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    /// `nodes[<id>].params.<name>` → value.
    pub bind: BTreeMap<String, Value>,
    /// Prior bits in [−8, 0].
    pub prior_bits: f32,
}

/// A proposal call's answer and what it cost.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProposalReply {
    /// Suggestions, best first.
    pub suggestions: Vec<Suggestion>,
    /// Assist word operations spent.
    pub ops: u64,
    /// Hypotheses the operator tested — charged to the stage's look-elsewhere `L_j` (§1.3).
    pub hypotheses: u64,
}

/// What the engine runs stages and proposals through (module docs).
pub trait Evaluator: Sync {
    /// A stage output (samples, soft symbols, bits, frames…), opaque to the engine.
    type Output: Send + Sync;

    /// Runs `req.new_nodes` over `req.window`, reading `req.parent`.
    fn evaluate(
        &self,
        req: &EvalRequest<'_, Self::Output>,
    ) -> Result<Evaluated<Self::Output>, EvalError>;

    /// Runs a proposal operator. The default proposes nothing (no operators wired: M-4).
    fn propose(&self, req: &ProposeRequest<'_, Self::Output>) -> Result<ProposalReply, EvalError> {
        let _ = req;
        Ok(ProposalReply::default())
    }
}

// ---------------------------------------------------------------------------------------------
// The spec
// ---------------------------------------------------------------------------------------------

/// The recipe header every candidate shares (the target channel and output policy).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecipeHead {
    /// What the prefixes consume.
    pub input: InputSpec,
    /// The content ceiling of every candidate.
    pub output_policy: OutputPolicy,
    /// Field maps `fields` nodes may reference.
    #[serde(default)]
    pub field_maps: BTreeMap<String, FieldMap>,
}

/// One seeded hypothesis (ADR-0015 §4.2; M-5 turns templates and the ADR-0016 seed into these).
#[derive(Clone, Debug, PartialEq)]
pub struct Root {
    /// The structure space.
    pub skeleton: Skeleton,
    /// The template it came from, if any.
    pub template: Option<TemplateRef>,
    /// Free parameters, with domains and seeds (§1.2).
    pub free: Vec<FreeParam>,
    /// `log₂ π(h)`, clipped. Orders; never ranks.
    pub prior_bits: f32,
    /// Where it came from.
    pub seed_source: SeedSource,
    /// The family it commits to, when seeding committed one.
    pub family: Option<String>,
    /// `Some(posterior)`: deferred to the side queue by ADR-0016's likelihood rule.
    pub deferred: Option<f64>,
    /// Where the check this root reaches comes from (ADR-0022 §5.1). An open skeleton is
    /// [`CheckOrigin::Searched`]; seeding sets `TemplateFixed` only for a `builtin` or `user`
    /// template that fixes the whole check.
    pub check_origin: CheckOrigin,
}

/// A suspected structure with no block (ADR-0021 §7A.6), recorded as `unsupported` up front.
#[derive(Clone, Debug, PartialEq)]
pub struct UnsupportedStructure {
    /// `psk`, `css`, `ofdm`.
    pub structure: String,
    /// The missing block's stable id.
    pub missing_block: String,
    /// Reference (`ADR-0011 §1.5`).
    pub reference: String,
    /// The `hk-mod@1` family.
    pub family: Option<String>,
    /// The slot the block would fill.
    pub slot: Stage,
    /// The posterior of the suspicion, if a classification made it.
    pub posterior: Option<f64>,
}

/// The solve rule, **ADR-0022's inequality** (§4, §6 steps 1–3) on hold-out evidence, replacing
/// ADR-0015 §5.5's 64 bits / 3 frames / width 16.
///
/// A result is `solved` only when all hold, on hold-out, at S5 or deeper:
/// 1. the check's width ≥ [`Self::min_check_width`] (a check summary is required);
/// 2. `check_bits = width × differences − L_check` ≥ [`Self::hard_check_floor_bits`] — a solve
///    always carries a check, never sync excess alone;
/// 3. `analytic_holdout_bits` ≥ [`Self::min_analytic_holdout_bits`], paid only in analytic-null
///    bits each net of its own stage's look-elsewhere (ADR-0022 §2.1);
/// 4. for a **searched** check at a profile with `K > 0`, ADR-0021 §8.2's null control ran and did
///    not cap.
///
/// The frame count is not a constant: it is what the inequality implies,
/// `max(1, ⌈(24 + L_check) / width⌉)` differences (ADR-0022 §4.2). `ConfirmPolicy.synthesized`
/// re-checks the same numbers from the stored row, plus front-end trust and the profile.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SolveRule {
    /// ADR-0022 §4.1: 24.
    pub min_analytic_holdout_bits: f32,
    /// ADR-0022 §4.3: 16 of those bits from a check stage. Derived.
    pub hard_check_floor_bits: f32,
    /// ADR-0022 §4.3 / §4.3.1: 16, **measured by T-577** (8 only once §4.3.1's count is
    /// re-measured by T-577's harness; never below 8).
    pub min_check_width: u32,
}

impl Default for SolveRule {
    fn default() -> Self {
        Self {
            min_analytic_holdout_bits: 24.0,
            hard_check_floor_bits: 16.0,
            min_check_width: 16,
        }
    }
}

impl SolveRule {
    /// Whether `h` (at `stage`) meets steps 1–3. The null control (step 4) is the engine's.
    pub fn met(&self, stage: Stage, h: &HoldoutEvidence) -> bool {
        stage >= Stage::S5
            && h.check_width.is_some_and(|w| w >= self.min_check_width)
            && h.check_bits
                .is_some_and(|b| b >= self.hard_check_floor_bits)
            && h.l_check.is_some()
            && h.analytic_bits >= self.min_analytic_holdout_bits
    }
}

/// Everything a search needs besides the evaluator.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchSpec {
    /// The shared recipe header.
    pub head: RecipeHead,
    /// Seeded hypotheses, in seed order.
    pub roots: Vec<Root>,
    /// Structures suspected but unsupported.
    pub unsupported: Vec<UnsupportedStructure>,
    /// The profile (trace bounds, power admission).
    pub profile: Profile,
    /// The admitted budget ([`crate::admission::admit`]).
    pub budget: SynthBudget,
    /// The solve rule.
    pub solve: SolveRule,
    /// When the job started (coverage `t`), supplied by the caller.
    pub started_at: String,
    /// Trace residency bounds; [`Profile::trace_bounds`] unless a caller (a test, a
    /// memory-constrained host) narrows them. Applied on insert (ADR-0021 §2.3).
    pub trace_bounds: TraceBounds,
}

impl SearchSpec {
    /// A spec at `profile`'s default budget and the default solve rule.
    pub fn new(head: RecipeHead, roots: Vec<Root>, profile: Profile) -> Self {
        Self {
            head,
            roots,
            unsupported: Vec::new(),
            profile,
            budget: profile.budget(),
            solve: SolveRule::default(),
            started_at: String::new(),
            trace_bounds: profile.trace_bounds(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Progress and outcome
// ---------------------------------------------------------------------------------------------

/// The live progress snapshot (ADR-0015 §5.2's four counters plus ADR-0021 §3's split).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    /// Job state (`searching`, `validating`, `throttled`).
    pub state: Option<JobState>,
    /// Deepest stage any node reached.
    pub stage_max: Option<Stage>,
    /// Nodes in the beam.
    pub beam: u32,
    /// Tried nodes pruned (floor, bound, beam).
    pub pruned: u64,
    /// Not-tried nodes deferred (prior or budget).
    pub deferred: u64,
    /// Trace nodes that were evaluated.
    pub tried: u64,
    /// Trace nodes that were not.
    pub not_tried: u64,
    /// Trace nodes dropped by the bound (counted in `elided`).
    pub elided: u64,
    /// Evaluations so far.
    pub evaluations: u64,
    /// Proposal calls so far.
    pub proposal_calls: u64,
}

/// A progress listener (the job manager streams it, ≤ 1/s).
pub trait Observer {
    /// Called at state changes and after each evaluated chunk.
    fn progress(&mut self, p: &Progress);
}

impl Observer for () {
    fn progress(&mut self, _: &Progress) {}
}

impl<F: FnMut(&Progress)> Observer for F {
    fn progress(&mut self, p: &Progress) {
        self(p)
    }
}

/// What the job used (`used` on the job object, plus the count units).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Used {
    /// Wall, s.
    pub wall_s: f64,
    /// Busy time summed over search threads, s.
    pub cpu_s: f64,
    /// Evaluations.
    pub evaluations: u64,
    /// Hypotheses charged to look-elsewhere, all stages.
    pub hypotheses: u64,
    /// Proposal calls.
    pub proposal_calls: u64,
    /// Assist word operations.
    pub assist_ops: u64,
    /// Peak memo-cache residency, bytes.
    pub cache_bytes: u64,
}

/// The trace's measured cost (ADR-0021 §3: "a claim to be checked, not asserted").
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TraceCost {
    /// Wall spent building and retaining trace nodes, s.
    pub wall_s: f64,
    /// The part of `wall_s` spent inside the sink (serialising for the byte bound, retention).
    pub retention_s: f64,
    /// Nodes that left the frontier (recorded + elided): what `wall_s` was spent on.
    pub decisions: u64,
    /// Retained trace bytes at the end.
    pub bytes: u64,
    /// `wall_s` over the job's wall.
    pub fraction: f64,
}

/// A finished search.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchOutcome {
    /// `done`, `cancelled` or `failed`.
    pub state: JobState,
    /// Why it stopped; `None` only when it failed.
    pub stop: Option<StopReason>,
    /// The failure, if any.
    pub error: Option<String>,
    /// Ranked results (≤ [`MAX_RESULTS`]); there are always some once anything was evaluated.
    pub results: Vec<PipelineResult>,
    /// The search trace.
    pub trace: Trace,
    /// What was searched.
    pub coverage: Coverage,
    /// The engine's reading of why nothing won, when nothing solved and the job finished
    /// (ADR-0021 §7A.3). M-9 seals the `Resolution` from it.
    pub reason: Option<Reason>,
    /// Resources used.
    pub used: Used,
    /// The trace's cost.
    pub trace_cost: TraceCost,
    /// Some decision depended on wall/CPU time, a cancel, a throttle, a thermal pause or a power
    /// change (ADR-0021 §5). Every [`TraceNode::nondeterministic`] node implies it.
    pub nondeterministic: bool,
    /// What the trace can be reproduced from (ADR-0021 §5). The engine fills everything but
    /// `window` and `calibration_hash`, which the job layer knows.
    pub replay_key: ReplayKey,
    /// How often capture losses throttled the job.
    pub throttle_events: u32,
    /// The power policy that refused further expansion, if one did.
    pub refused_power: Option<PowerPolicy>,
    /// The rank-1 result's decoded hold-out frames (≤ [`MAX_HOLDOUT_FRAMES`]), for the attach
    /// step's stored decodes (ADR-0015 §5.5). Empty unless rank 1 is `solved`. Never served.
    pub holdout_frames: Vec<HoldoutFrame>,
}

impl SearchOutcome {
    /// The null control a result recorded, preferring one that capped (the reason nothing
    /// solved) over one that passed.
    pub fn null_control(&self) -> Option<NullControl> {
        let all = || {
            self.results
                .iter()
                .filter_map(|r| r.holdout.as_ref()?.null_control)
        };
        all().find(|n| n.capped).or_else(|| all().next())
    }

    /// **Seals** the finished search's [`crate::trace::Resolution`] (ADR-0021 §7A.2, §9.3): built
    /// here, in `hk-synth`, before any context lookup can run, so nothing downstream can shape it.
    /// `None` when a result solved. Only a `done` search may say `unknown`; a cancelled or failed
    /// one ruled nothing out and is `not-searched` with `last_attempt = ended`.
    ///
    /// `trace_summary` and `replay_key` are the job's (ADR-0021 §4.1, §5), carried so the
    /// persisted row can be compared with a later look.
    pub fn seal(
        &self,
        trace_summary: Option<Value>,
        replay_key: Option<Value>,
        ended: &str,
    ) -> Option<crate::trace::Resolution> {
        use crate::trace::{Resolution, ResolutionKind};
        if self.state != JobState::Done {
            return Some(Resolution::not_searched(Some(ended.to_owned())));
        }
        if self.results.iter().any(|r| r.verdict == Verdict::Solved) {
            return None;
        }
        let deepest = self.results.iter().map(|r| r.verdict).max();
        let null_control = self.null_control();
        let (kind, text) = if self.reason == Some(Reason::UnsupportedStructure) {
            (
                ResolutionKind::UnsupportedStructure,
                "The structure this looks like has no block to decode it yet.".to_owned(),
            )
        } else if self.results.iter().any(|r| r.characterisation.is_some()) {
            (
                ResolutionKind::StructuredUnidentified,
                "Framed and check-valid, but no known format matches.".to_owned(),
            )
        } else {
            let why = match self.reason {
                Some(Reason::NoSignal) => "no signal measured above the floor".to_owned(),
                Some(Reason::NothingScored) => "every hypothesis measured below its floor".into(),
                Some(Reason::Tied) => match null_control.filter(|n| n.capped) {
                    Some(n) => format!(
                        "the best candidate met the solve rule on hold-out, but the null windows \
                         came within {:.1} bits of it, under the {MIN_NULL_MARGIN_BITS}-bit margin",
                        n.margin_bits
                    ),
                    None => "the best candidates tied within the margin".to_owned(),
                },
                Some(Reason::BudgetExhausted) => {
                    "the budget ran out before the space was covered".to_owned()
                }
                _ => "nothing reached the solve rule".to_owned(),
            };
            (
                ResolutionKind::Unknown,
                format!("Searched and not identified: {why}."),
            )
        };
        Some(Resolution {
            kind,
            deepest_verdict: deepest,
            reason: self.reason,
            coverage: Some(self.coverage.clone()),
            suspected: None,
            null_control,
            ruled_out: Vec::new(),
            retry: None,
            trace_summary,
            replay_key,
            explanations: Vec::new(),
            last_attempt: None,
            summary: text,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum At {
    Root(u32),
    Node(u32),
}

#[derive(Clone, Copy, Debug)]
struct Live {
    at: At,
    next: Stage,
}

#[derive(Clone, Debug)]
struct Holdout {
    ev: HoldoutEvidence,
    solved: bool,
    /// ADR-0021 §8.2 capped the verdict at `framed`.
    capped: bool,
    frames: Vec<HoldoutFrame>,
}

/// One chain run over one window (hold-out or a null).
struct ChainRun {
    /// `Σ min(b_j, cap_j)` over the chain's stages.
    capped_sum: f32,
    mask: u8,
    /// Per stage: capped analytic bits, only for stages that emitted analytic evidence.
    analytic: Vec<(Stage, f32)>,
    ladder: Vec<StageEvidence>,
    differences: u32,
    check: Option<CheckSummary>,
    frames: Vec<HoldoutFrame>,
}

struct Node {
    parent: Option<u32>,
    root: u32,
    stage: Stage,
    /// Alternative index at `stage`; `None` for a not-tried root-level node.
    alt: Option<usize>,
    bind: BTreeMap<String, Value>,
    swept: Vec<Swept>,
    key: String,
    reuse: Option<u32>,
    prior_bits: f32,
    seed_source: SeedSource,
    family: Option<String>,
    family_node: Option<u32>,
    evaluated: bool,
    b: f32,
    capped_sum: f32,
    chain_mask: u8,
    ladder: Vec<StageEvidence>,
    measured: Option<Measured>,
    check: Option<CheckSummary>,
    frames: u32,
    evaluations: u64,
    cpu: Duration,
    complete: bool,
    holdout: Option<Holdout>,
    finalised: bool,
    /// For a structure with no skeleton (an unsupported suspicion): what the trace names.
    label: Option<(String, String)>,
    /// Appended to the rendered summary.
    note: Option<String>,
    /// The [`MAX_DISCRETE_CHILDREN`] overflow row: deferred by a count cap at planning time,
    /// never by the stop.
    overflow: bool,
}

struct Plan {
    parent: Option<u32>,
    root: u32,
    stage: Stage,
    alt: usize,
    base: Candidate,
    new_nodes: Range<usize>,
    bind: BTreeMap<String, Value>,
    sweeps: Vec<(String, Vec<Value>, Scale)>,
    prior_bits: f32,
    seed_source: SeedSource,
    family: Option<String>,
    family_node: Option<u32>,
    commits: bool,
    key: String,
    parent_output: Option<usize>,
    max_evals: u64,
}

/// Where a not-tried hypothesis would have gone: (parent, root, stage, slot alternative).
type Slot = (Option<u32>, u32, Stage, usize);

/// Counts `slot` into `groups`, keeping first-seen order (so node ids stay deterministic).
fn group_into(groups: &mut Vec<(Slot, usize)>, slot: Slot) {
    match groups.iter_mut().find(|(s, _)| *s == slot) {
        Some((_, n)) => *n += 1,
        None => groups.push((slot, 1)),
    }
}

/// One discrete option: bindings, the prior bits they add, and whether a proposal made them.
type DiscreteOption = (BTreeMap<String, Value>, f32, bool);

/// A hypothesis identical to one already evaluated (or planned) in this job.
struct Dup {
    parent: Option<u32>,
    root: u32,
    stage: Stage,
    alt: usize,
    key: String,
    prior_bits: f32,
    seed_source: SeedSource,
    family: Option<String>,
    family_node: Option<u32>,
    commits: bool,
}

struct PlanResult<O> {
    bind: BTreeMap<String, Value>,
    best: Option<(f32, Evaluated<O>)>,
    evaluations: u64,
    cpu: Duration,
    swept: Vec<Swept>,
    error: Option<EvalError>,
}

struct Cache<O> {
    map: HashMap<u32, (Arc<O>, u64, u64)>,
    bytes: u64,
    peak: u64,
    tick: u64,
    cap: u64,
}

impl<O> Cache<O> {
    fn new(cap: u64) -> Self {
        Self {
            map: HashMap::new(),
            bytes: 0,
            peak: 0,
            tick: 0,
            cap,
        }
    }

    fn get(&mut self, id: u32) -> Option<Arc<O>> {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(&id).map(|e| {
            e.2 = tick;
            Arc::clone(&e.0)
        })
    }

    fn put(&mut self, id: u32, out: Arc<O>, bytes: u64) {
        if bytes > self.cap {
            return;
        }
        self.tick += 1;
        while self.bytes + bytes > self.cap {
            let Some((&victim, _)) = self.map.iter().min_by_key(|(k, e)| (e.2, **k)) else {
                break;
            };
            if let Some((_, b, _)) = self.map.remove(&victim) {
                self.bytes -= b;
            }
        }
        self.bytes += bytes;
        self.peak = self.peak.max(self.bytes);
        self.map.insert(id, (out, bytes, self.tick));
    }
}

/// Parses `nodes[<id>].params.<name>`.
fn parse_path(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("nodes[")?;
    let (id, rest) = rest.split_once(']')?;
    let name = rest.strip_prefix(".params.")?;
    (!id.is_empty() && !name.is_empty() && !name.contains('.')).then_some((id, name))
}

fn set_param(nodes: &mut [NodeSpec], path: &str, value: &Value) -> bool {
    let Some((id, name)) = parse_path(path) else {
        return false;
    };
    match nodes.iter_mut().find(|n| n.id == id) {
        Some(n) => {
            n.params.insert(name.to_owned(), value.clone());
            true
        }
        None => false,
    }
}

fn stage_bit(s: Stage) -> u8 {
    1 << s.index()
}

fn default_metric(stage: Stage) -> MetricId {
    match stage {
        Stage::S0 => MetricId::Snr,
        Stage::S1 => MetricId::Bimodality,
        Stage::S2 => MetricId::EyeOpen,
        Stage::S3 => MetricId::BitStructure,
        Stage::S4 => MetricId::SyncExcess,
        Stage::S5 => MetricId::CheckDistinctValid,
        Stage::S6 => MetricId::FieldFit,
    }
}

/// `b_k` for one evaluation: per block, §13.1's combination; summed across blocks.
fn stage_bits(ev: &[NodeEvidence], stage: Stage) -> f32 {
    ev.iter()
        .map(|n| combine_stage_bits(n.evidence.iter().filter(|e| e.stage == stage)))
        .sum()
}

/// The confirm-paying part of `b_k` (ADR-0022 §2.1): §13.1's combination over only the metrics
/// **§2.1's table lists as contributing** ([`MetricId::pays_for_confirm`]) — not every metric with
/// a closed-form null. `None` when the stage emitted none of them.
fn analytic_stage_bits(ev: &[NodeEvidence], stage: Stage) -> Option<f32> {
    let mut any = false;
    let b = ev
        .iter()
        .map(|n| {
            let it = n
                .evidence
                .iter()
                .filter(|e| e.stage == stage && e.metric.pays_for_confirm());
            let v: Vec<&Evidence> = it.collect();
            any |= !v.is_empty();
            combine_stage_bits(v)
        })
        .sum();
    any.then_some(b)
}

/// ADR-0022 §4.2's `differences` of one `check_distinct_valid` record: its **`raw`** — the
/// chance-corrected count a check block reports (frames valid *without* FEC correction, each
/// payload counted once however often it repeats, short-period payloads not counted; T-210) —
/// never its `n`, which is the support: every unit **tested**, valid or not, repeated or not.
/// Reading `n` credited a beacon repeating one payload twelve times with twelve differences.
/// Clamped to `n` (a block cannot have more differences than units it tested) and to 0 for a
/// non-finite or negative report: an unreadable count is no count.
fn differences_of(e: &Evidence) -> u32 {
    if !(e.raw.is_finite() && e.raw >= 0.0) {
        return 0;
    }
    (e.raw.floor() as u32).min(e.n)
}

/// The differences of one evaluation, read off the check block `node` whose summary supplies
/// the width — never another block's count against this block's width. With no node named, the
/// **smallest** count any check block reported (the pairing is unknown, so the weakest).
fn differences(ev: &[NodeEvidence], node: Option<&str>) -> u32 {
    ev.iter()
        .filter(|n| node.is_none_or(|id| n.node == id))
        .flat_map(|n| n.evidence.iter())
        .filter(|e| e.metric == MetricId::CheckDistinctValid)
        .map(differences_of)
        .min()
        .unwrap_or(0)
}

fn capped(stage: Stage, b: f32) -> f32 {
    default_cap_bits(stage).map_or(b, |c| b.min(c))
}

fn f64_value(v: f64) -> Value {
    serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number)
}

/// The continuous grid for a free parameter (§3.1 step 3): the seed first, then its
/// neighbours, within the domain, deduplicated.
pub(crate) fn continuous_grid(
    domain: &Domain,
    seed: Option<&Value>,
) -> Option<(Vec<Value>, Scale)> {
    match domain {
        Domain::Float(d) => {
            let (lo, hi) = (d.lo.min(d.hi), d.lo.max(d.hi));
            let mid = if d.scale == Scale::Log && lo > 0.0 {
                (lo * hi).sqrt()
            } else {
                (lo + hi) / 2.0
            };
            let s = seed
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite())
                .map_or(mid, |v| v.clamp(lo, hi));
            let mut pts = vec![s];
            match d.scale {
                Scale::Log => {
                    for m in [1.0, 0.5, 2.0] {
                        for k in -3i32..=3 {
                            pts.push(s * m * (1.0 + 0.01 * f64::from(k)));
                        }
                    }
                }
                Scale::Linear => {
                    let step = ((hi - lo) / 6.0).max(d.resolution.unwrap_or(0.0));
                    for k in -3i32..=3 {
                        pts.push(s + step * f64::from(k));
                    }
                }
            }
            let mut out: Vec<f64> = Vec::new();
            for p in pts {
                let tol = d.resolution.unwrap_or(0.0).max(1e-9 * p.abs().max(1.0));
                if p >= lo - 1e-12 && p <= hi + 1e-12 && !out.iter().any(|q| (q - p).abs() < tol) {
                    out.push(p);
                }
            }
            Some((out.into_iter().map(f64_value).collect(), d.scale))
        }
        Domain::Int(d) => {
            let (lo, hi) = (d.lo.min(d.hi), d.lo.max(d.hi));
            let s = seed
                .and_then(Value::as_i64)
                .map_or(lo + (hi - lo) / 2, |v| v.clamp(lo, hi));
            let step = ((hi - lo) / 6).max(d.resolution.unwrap_or(1)).max(1);
            let mut out = vec![s];
            for k in -3i64..=3 {
                let p = s.saturating_add(step.saturating_mul(k));
                if (lo..=hi).contains(&p) && !out.contains(&p) {
                    out.push(p);
                }
            }
            Some((out.into_iter().map(Value::from).collect(), Scale::Linear))
        }
        _ => None,
    }
}

/// The discrete options for a free parameter, with prior bits each (weights order, never score).
fn discrete_options(domain: &Domain) -> Option<Vec<(Value, f32)>> {
    match domain {
        Domain::Enum(e) => {
            let n = e.values.len();
            let w: Vec<f64> = match &e.weights {
                Some(w) if w.len() == n => w.clone(),
                _ => vec![1.0; n],
            };
            let max = w.iter().copied().fold(0.0f64, f64::max);
            let mut v: Vec<(usize, Value, f32)> = e
                .values
                .iter()
                .cloned()
                .enumerate()
                .map(|(i, val)| {
                    let p = if max > 0.0 { w[i] / max } else { 1.0 };
                    (i, val, crate::evidence::prior_bits(p))
                })
                .collect();
            v.sort_by(|a, b| b.2.total_cmp(&a.2).then(a.0.cmp(&b.0)));
            Some(v.into_iter().map(|(_, val, p)| (val, p)).collect())
        }
        Domain::Hex(h) => Some(
            h.candidates
                .iter()
                .map(|c| (Value::String(c.clone()), 0.0))
                .collect(),
        ),
        _ => None,
    }
}

fn swept_descriptor(path: &str, values: &[Value], scale: Scale, points: u32) -> Swept {
    let nums: Vec<f64> = values.iter().filter_map(Value::as_f64).collect();
    Swept {
        path: path.to_owned(),
        lo: nums.iter().copied().fold(f64::INFINITY, f64::min),
        hi: nums.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        points,
        scale,
    }
}

/// Runs one plan: the seed point, then a coordinate sweep of each continuous parameter.
fn run_plan<E: Evaluator>(
    ev: &E,
    catalogue: &(dyn Catalogue + Sync),
    plan: &Plan,
    parent: Option<&E::Output>,
) -> PlanResult<E::Output> {
    let t = Instant::now();
    let mut current = plan.bind.clone();
    for (path, grid, _) in &plan.sweeps {
        current.insert(path.clone(), grid[0].clone());
    }
    let mut res = PlanResult {
        bind: current.clone(),
        best: None,
        evaluations: 0,
        cpu: Duration::ZERO,
        swept: Vec::new(),
        error: None,
    };
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut eval_at =
        |bind: &BTreeMap<String, Value>, res: &mut PlanResult<E::Output>| -> Option<f32> {
            let key = serde_json::to_string(bind).unwrap_or_default();
            if !seen.insert(key) {
                return None;
            }
            let mut cand = plan.base.clone();
            for (p, v) in bind {
                set_param(&mut cand.recipe.nodes, p, v);
            }
            if cand.check_prefix(catalogue).is_err() {
                return None;
            }
            res.evaluations += 1;
            let req = EvalRequest {
                window: EvalWindow::Search,
                stage: plan.stage,
                candidate: &cand,
                new_nodes: plan.new_nodes.clone(),
                parent,
            };
            match ev.evaluate(&req) {
                Ok(out) => {
                    let b = stage_bits(&out.evidence, plan.stage);
                    let better = res
                        .best
                        .as_ref()
                        .is_none_or(|(best, _)| capped(plan.stage, b) > capped(plan.stage, *best));
                    if better {
                        res.best = Some((b, out));
                        res.bind = bind.clone();
                    }
                    Some(b)
                }
                Err(e) => {
                    res.error = Some(e);
                    None
                }
            }
        };
    eval_at(&current, &mut res);
    for (path, grid, scale) in &plan.sweeps {
        if res.error.is_some() {
            break;
        }
        let mut points = 1u32;
        for v in grid.iter().skip(1) {
            if res.error.is_some() {
                break;
            }
            let mut b = res.bind.clone();
            b.insert(path.clone(), v.clone());
            if eval_at(&b, &mut res).is_some() {
                points += 1;
            }
        }
        res.swept.push(swept_descriptor(path, grid, *scale, points));
    }
    res.cpu = t.elapsed();
    res
}

struct Engine<'a, E: Evaluator> {
    spec: &'a SearchSpec,
    catalogue: &'a (dyn Catalogue + Sync),
    ev: &'a E,
    control: &'a Control,
    observer: &'a mut dyn Observer,
    nodes: Vec<Node>,
    memo: HashMap<String, u32>,
    cache: Cache<E::Output>,
    sink: TraceSink,
    counts: [u64; 7],
    used: Used,
    cpu: Duration,
    start: Instant,
    threads: u32,
    battery_applied: bool,
    stop: Option<StopReason>,
    refused_power: Option<PowerPolicy>,
    error: Option<String>,
    nondeterministic: bool,
    /// The stop was caused by time or an external event (a wall/CPU backstop, a wall-measured
    /// plateau, a cancel), so the not-tried nodes it leaves behind are flagged.
    time_stop: bool,
    /// Evaluations spent on ADR-0021 §8.2's null control (bounded by [`NULL_CONTROL_SHARE`]).
    null_evals: u64,
    /// A candidate that met the solve rule was capped by the null control.
    null_capped: bool,
    complete: Vec<u32>,
    best: Option<(Stage, f32)>,
    last_gain_progress: f64,
    last_lost: u64,
    throttle_events: u32,
    solved: Option<u32>,
    trace_wall: Duration,
    progress: Progress,
    beam: u32,
    families: BTreeMap<String, FamilyCoverage>,
    root_tried: Vec<bool>,
    queue_position: u32,
    pending_keys: std::collections::HashSet<String>,
    later: Vec<Dup>,
}

/// Runs a search to its stop and returns the ranked results, the trace and the coverage.
pub fn search<E: Evaluator>(
    spec: &SearchSpec,
    catalogue: &(dyn Catalogue + Sync),
    evaluator: &E,
    control: &Control,
    observer: &mut dyn Observer,
) -> SearchOutcome {
    let e = Engine {
        spec,
        catalogue,
        ev: evaluator,
        control,
        observer,
        nodes: Vec::new(),
        memo: HashMap::new(),
        cache: Cache::new(spec.budget.max_cache_bytes),
        sink: TraceSink::new(spec.trace_bounds),
        counts: [0; 7],
        used: Used::default(),
        cpu: Duration::ZERO,
        start: Instant::now(),
        threads: spec.budget.threads.max(1),
        battery_applied: false,
        stop: None,
        refused_power: None,
        error: None,
        nondeterministic: false,
        time_stop: false,
        null_evals: 0,
        null_capped: false,
        complete: Vec::new(),
        best: None,
        last_gain_progress: 0.0,
        last_lost: control.lost_samples(),
        throttle_events: 0,
        solved: None,
        trace_wall: Duration::ZERO,
        progress: Progress::default(),
        beam: 0,
        families: BTreeMap::new(),
        root_tried: vec![false; spec.roots.len()],
        queue_position: 0,
        pending_keys: std::collections::HashSet::new(),
        later: Vec::new(),
    };
    e.run()
}

impl<'a, E: Evaluator> Engine<'a, E> {
    fn run(mut self) -> SearchOutcome {
        self.set_state(JobState::Searching);
        let spec = self.spec;
        for u in &spec.unsupported {
            self.not_tried_unsupported(u);
        }
        let (main, side): (Vec<u32>, Vec<u32>) = (0..self.spec.roots.len() as u32)
            .partition(|&r| self.spec.roots[r as usize].deferred.is_none());
        self.run_pass(&main);
        if self.stop.is_none() && !side.is_empty() {
            self.run_pass(&side);
        } else {
            for (pos, &r) in side.iter().enumerate() {
                let posterior = self.spec.roots[r as usize].deferred.unwrap_or(0.0);
                self.not_tried_root(
                    r,
                    Outcome::DeferredPrior {
                        posterior,
                        queue_position: pos as u32 + 1,
                    },
                );
            }
        }
        if self.error.is_none() && self.stop != Some(StopReason::Cancelled) && self.solved.is_none()
        {
            self.validate_top();
        }
        self.finalise_complete();
        self.finish()
    }

    // ---- budget and control ----

    fn progress_fraction(&self) -> (f64, bool) {
        let b = &self.spec.budget;
        let mut p = 0.0f64;
        let mut wall_max = false;
        if let Some(m) = b.max_evaluations.filter(|m| *m > 0) {
            p = p.max(self.used.evaluations as f64 / m as f64);
        }
        if let Some(m) = b.max_proposal_calls.filter(|m| *m > 0) {
            p = p.max(self.used.proposal_calls as f64 / m as f64);
        }
        if b.wall_s > 0.0 {
            let w = self.start.elapsed().as_secs_f64() / b.wall_s;
            if w > p {
                p = w;
                wall_max = true;
            }
        }
        (p, wall_max)
    }

    fn reserve(&self) -> u64 {
        self.spec.budget.max_evaluations.map_or(0, |m| {
            (VALIDATION_RESERVE + self.null_reserve(m)).min(m / 4)
        })
    }

    /// Evaluations the null control may spend: `K` seven-stage chains, within
    /// [`NULL_CONTROL_SHARE`] of the cap (ADR-0021 §8.2). Held back from the search like the
    /// validation reserve, so a control can run on the first candidate that meets the rule.
    fn null_reserve(&self, max_evaluations: u64) -> u64 {
        let want = u64::from(self.spec.profile.null_windows()) * 7;
        want.min(self.null_share(max_evaluations))
    }

    fn null_share(&self, max_evaluations: u64) -> u64 {
        (max_evaluations as f64 * NULL_CONTROL_SHARE).floor() as u64
    }

    fn afford(&self, n: u64, with_reserve: bool) -> bool {
        let r = if with_reserve { self.reserve() } else { 0 };
        self.spec
            .budget
            .max_evaluations
            .is_none_or(|m| self.used.evaluations + n + r <= m)
    }

    fn halt(&mut self, why: StopReason) {
        if self.stop.is_none() {
            self.stop = Some(why);
        }
    }

    /// A stop decided by time or an external event: whatever it leaves unexplored is not
    /// reproducible from the replay key (ADR-0021 §5).
    fn halt_timed(&mut self, why: StopReason) {
        if self.stop.is_none() {
            self.time_stop = true;
            self.nondeterministic = true;
        }
        self.halt(why);
    }

    /// Between chunks: cancel, power, throttle, thermal, backstops, plateau.
    fn check_control(&mut self) {
        if self.stop.is_some() {
            return;
        }
        if self.control.is_cancelled() {
            self.halt_timed(StopReason::Cancelled);
            return;
        }
        let power = self.control.power();
        if !power.allows(self.spec.profile) {
            self.refused_power = Some(power);
            self.nondeterministic = true;
            self.halt(StopReason::Budget);
            return;
        }
        if power == PowerPolicy::Battery && !self.battery_applied {
            self.battery_applied = true;
            self.threads = halve(self.threads);
            self.nondeterministic = true;
        }
        let lost = self.control.lost_samples();
        if lost > self.last_lost {
            self.last_lost = lost;
            self.threads = halve(self.threads);
            self.throttle_events += 1;
            self.nondeterministic = true;
            self.set_state(JobState::Throttled);
            self.set_state(JobState::Searching);
        }
        if self.control.thermal() {
            self.nondeterministic = true;
            self.set_state(JobState::Throttled);
            while self.control.thermal()
                && !self.control.is_cancelled()
                && self.start.elapsed().as_secs_f64() < self.spec.budget.wall_s
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            self.set_state(JobState::Searching);
            if self.control.is_cancelled() {
                self.halt_timed(StopReason::Cancelled);
                return;
            }
        }
        let b = &self.spec.budget;
        if self.start.elapsed().as_secs_f64() >= b.wall_s || self.cpu.as_secs_f64() >= b.cpu_s {
            self.halt_timed(StopReason::Budget);
        }
    }

    /// Plateau (§3.3), judged at level end, after the level's work has been scored — judging it
    /// before would charge a proposal call's cost without crediting what it found.
    fn check_plateau(&mut self) {
        if self.stop.is_some() {
            return;
        }
        let (p, wall_max) = self.progress_fraction();
        if p - self.last_gain_progress >= PLATEAU_FRACTION {
            if wall_max {
                self.halt_timed(StopReason::Plateau);
            } else {
                self.halt(StopReason::Plateau);
            }
        }
    }

    fn note_score(&mut self, stage: Stage, bits: f32) {
        let gain = match self.best {
            None => true,
            Some((s, b)) => stage > s || (stage == s && bits > b + PLATEAU_GAIN_BITS),
        };
        if gain {
            self.best = Some((stage, bits));
            self.last_gain_progress = self.progress_fraction().0;
        } else if let Some((s, b)) = self.best
            && stage == s
            && bits > b
        {
            // Better, but not by the plateau margin: keep the new best without resetting.
            self.best = Some((s, bits));
        }
        self.progress.stage_max = Some(self.progress.stage_max.map_or(stage, |m| m.max(stage)));
    }

    fn set_state(&mut self, s: JobState) {
        self.progress.state = Some(s);
        self.emit();
    }

    fn emit(&mut self) {
        self.progress.beam = self.beam;
        self.progress.evaluations = self.used.evaluations;
        self.progress.proposal_calls = self.used.proposal_calls;
        self.progress.elided = self.sink.nodes_elided();
        let p = self.progress;
        self.observer.progress(&p);
    }

    // ---- scores ----

    fn l(&self, stage: Stage) -> f32 {
        look_elsewhere_bits(self.counts[stage.index() as usize])
    }

    fn evidence(&self, id: u32) -> f32 {
        let n = &self.nodes[id as usize];
        let l: f32 = Stage::ALL
            .iter()
            .filter(|s| n.chain_mask & stage_bit(**s) != 0)
            .map(|s| self.l(*s))
            .sum();
        n.capped_sum - l
    }

    fn skeleton(&self, root: u32) -> &Skeleton {
        &self.spec.roots[root as usize].skeleton
    }

    fn next_slot(&self, root: u32, after: Option<Stage>) -> Option<Stage> {
        self.skeleton(root)
            .stages()
            .find(|s| after.is_none_or(|a| *s > a))
    }

    /// Optimistic remaining bits for the search key: capped stages at their cap, uncapped ones
    /// at their floor (an infinite optimism would erase the order). `None` for the bound test
    /// when an uncapped stage remains.
    fn remaining(&self, root: u32, after: Stage, for_bound: bool) -> Option<f32> {
        let mut sum = 0.0;
        for s in self.skeleton(root).stages().filter(|s| *s > after) {
            match default_cap_bits(s) {
                Some(c) => sum += c,
                None if for_bound => return None,
                None => sum += default_floor_bits(s).unwrap_or(0.0),
            }
        }
        Some(sum)
    }

    fn deepest_slot(&self, root: u32) -> Stage {
        self.skeleton(root).stages().last().unwrap_or(Stage::S0)
    }

    fn search_key(&self, id: u32) -> f32 {
        let n = &self.nodes[id as usize];
        self.evidence(id) + n.prior_bits + self.remaining(n.root, n.stage, false).unwrap_or(0.0)
    }

    fn rank_key(&self, id: u32) -> (Stage, f32) {
        (self.nodes[id as usize].stage, self.evidence(id))
    }

    fn best_complete(&self) -> Option<(Stage, f32)> {
        self.complete
            .iter()
            .map(|&c| self.rank_key(c))
            .max_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)))
    }

    // ---- candidates ----

    fn chain(&self, id: u32) -> Vec<u32> {
        let mut v = vec![id];
        let mut cur = self.nodes[id as usize].parent;
        while let Some(p) = cur {
            v.push(p);
            cur = self.nodes[p as usize].parent;
        }
        v.reverse();
        v
    }

    /// Assembles a prefix from `(stage, alt, bind)` steps, returning it and the last step's
    /// node range.
    fn assemble(
        &self,
        root: u32,
        steps: &[(Stage, usize, &BTreeMap<String, Value>)],
    ) -> (Candidate, Range<usize>) {
        let r = &self.spec.roots[root as usize];
        let mut nodes: Vec<NodeSpec> = Vec::new();
        let mut choices = BTreeMap::new();
        let mut last = 0..0;
        for (stage, alt, bind) in steps {
            let a = &r.skeleton.alternatives(*stage)[*alt];
            let start = nodes.len();
            nodes.extend(a.nodes.iter().cloned());
            for (p, v) in *bind {
                set_param(&mut nodes[start..], p, v);
            }
            last = start..nodes.len();
            choices.insert(*stage, a.id.clone());
        }
        let from = nodes
            .last()
            .map_or_else(|| "input".to_owned(), |n| n.id.clone());
        let present: BTreeSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        let free = r
            .free
            .iter()
            .filter(|f| parse_path(&f.path).is_none_or(|(id, _)| !present.contains(id)))
            .cloned()
            .collect();
        let recipe = Recipe {
            schema: hk_recipe::RECIPE_SCHEMA.to_owned(),
            schema_version: hk_recipe::RECIPE_SCHEMA_VERSION,
            id: "synth-candidate".to_owned(),
            version: 1,
            name: r.skeleton.key(),
            description: String::new(),
            match_hints: Default::default(),
            input: self.spec.head.input.clone(),
            nodes,
            field_maps: self.spec.head.field_maps.clone(),
            outputs: vec![OutputSpec {
                id: "tail".to_owned(),
                kind: OutputKind::Stage,
                from,
                view: None,
                decode: None,
                channels: None,
                profile: None,
            }],
            output_policy: self.spec.head.output_policy.clone(),
            refine: None,
        };
        (
            Candidate {
                skeleton: r.skeleton.key(),
                choices,
                recipe,
                free,
            },
            last,
        )
    }

    fn candidate(&self, id: u32) -> (Candidate, Range<usize>) {
        let chain = self.chain(id);
        let steps: Vec<_> = chain
            .iter()
            .filter_map(|&c| {
                let n = &self.nodes[c as usize];
                n.alt.map(|a| (n.stage, a, &n.bind))
            })
            .collect();
        self.assemble(self.nodes[id as usize].root, &steps)
    }

    // ---- node bookkeeping ----

    fn node_id(id: u32) -> String {
        format!("n{id}")
    }

    fn push_node(&mut self, n: Node) -> u32 {
        let id = self.nodes.len() as u32;
        if let Some(p) = n.parent {
            self.sink.pin(p);
        }
        self.nodes.push(n);
        id
    }

    fn blank(&self, parent: Option<u32>, root: u32, stage: Stage, alt: Option<usize>) -> Node {
        let (prior, family, family_node, source, mask, capped_sum) = match parent {
            Some(p) => {
                let n = &self.nodes[p as usize];
                (
                    n.prior_bits,
                    n.family.clone(),
                    n.family_node,
                    n.seed_source,
                    n.chain_mask,
                    n.capped_sum,
                )
            }
            None => match self.spec.roots.get(root as usize) {
                Some(r) => (r.prior_bits, r.family.clone(), None, r.seed_source, 0, 0.0),
                None => (0.0, None, None, SeedSource::Open, 0, 0.0),
            },
        };
        Node {
            parent,
            root,
            stage,
            alt,
            bind: BTreeMap::new(),
            swept: Vec::new(),
            key: String::new(),
            reuse: None,
            prior_bits: prior,
            seed_source: source,
            family,
            family_node,
            evaluated: false,
            b: 0.0,
            capped_sum,
            chain_mask: mask,
            ladder: Vec::new(),
            measured: None,
            check: None,
            frames: 0,
            evaluations: 0,
            cpu: Duration::ZERO,
            complete: false,
            holdout: None,
            finalised: false,
            label: None,
            note: None,
            overflow: false,
        }
    }

    fn hypothesis(&self, id: u32) -> TraceHypothesis {
        let n = &self.nodes[id as usize];
        let (skeleton, choice) = match &n.label {
            Some(l) => l.clone(),
            None => {
                let sk = self.skeleton(n.root);
                let choice = n.alt.map_or_else(
                    || "*".to_owned(),
                    |a| sk.alternatives(n.stage)[a].id.clone(),
                );
                (sk.key(), choice)
            }
        };
        TraceHypothesis {
            skeleton,
            slot: n.stage,
            choice,
            family: n.family.clone(),
            params: n.bind.clone(),
            swept: n.swept.clone(),
        }
    }

    fn summary(&self, id: u32, outcome: &Outcome) -> String {
        let n = &self.nodes[id as usize];
        let h = self.hypothesis(id);
        let what = format!("{} {} at {}", h.skeleton, h.choice, n.stage.as_str());
        let m = n
            .measured
            .as_ref()
            .map(|m| format!("{} {:.1} bits", m.metric.as_str(), m.bits))
            .unwrap_or_default();
        match outcome {
            Outcome::Survived { children } => format!("{what}: {m}; expanded into {children}"),
            Outcome::PrunedFloor {
                floor_bits,
                measured_bits,
            } => format!(
                "{what}: {measured_bits:.1} bits, below the {floor_bits:.0}-bit {} floor",
                n.stage.as_str()
            ),
            Outcome::PrunedBound {
                bound_bits,
                best_bits,
            } => format!(
                "{what}: at most {bound_bits:.1} bits reachable, below the best complete {best_bits:.1}"
            ),
            Outcome::PrunedBeam { rank, width, cause } => format!(
                "{what}: {m}; ranked {rank}, outside the beam ({})",
                match cause {
                    BeamCause::Width => format!("width {width}"),
                    BeamCause::Diversity => format!("{DIVERSITY_CAP} per family and skeleton"),
                }
            ),
            Outcome::EvaluatedWorse { rank, gap_bits } => {
                format!("{what}: complete, rank {rank}, {gap_bits:.1} bits behind the winner")
            }
            Outcome::RefinedInto { into } => format!("{what}: refined into {into}"),
            Outcome::Memoised { reused } => format!("{what}: same prefix as {reused}, reused"),
            Outcome::DeferredPrior { posterior, .. } => {
                format!("{what}: not tried; deferred by the seed (posterior {posterior:.3})")
            }
            Outcome::DeferredBudget { stop, .. } => format!(
                "{what}: not tried; the search stopped ({}) first",
                serde_json::to_value(stop)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default()
            ),
            Outcome::Unsupported {
                structure,
                missing_block,
                ..
            } => format!("{what}: not tried; no block for {structure} ({missing_block})"),
            Outcome::NotApplicable { conflicts_with } => {
                format!("{what}: not tried; inconsistent with {conflicts_with}")
            }
            Outcome::RefusedPower { policy } => {
                format!("{what}: not tried; refused by the {policy} power policy")
            }
        }
    }

    fn finalise(&mut self, id: u32, outcome: Outcome) {
        let t = Instant::now();
        // Everything from here to the sink's retention is trace work: its wall is `trace_wall`,
        // and its allocations are what a counting allocator attributes to the trace.
        let _scope = crate::trace_sink::TraceScope::enter();
        // A memoised hit is recorded as `memoised {reused}` whatever its beam fate: the trace's
        // job for it is to show that the engine did not pay twice (ADR-0021 §1 rule 3). It still
        // competes in the beam, under its own prior, and its children name it as their parent.
        let outcome = match self.nodes[id as usize].reuse {
            Some(r) if outcome.tried() => Outcome::Memoised {
                reused: Self::node_id(r),
            },
            _ => outcome,
        };
        let tried = outcome.tried();
        let kind = outcome.kind();
        // ADR-0021 §5: a not-tried node whose cause is time or an external event. A
        // `deferred_budget` left by a time-caused stop is; the over-cap discrete node (which
        // carries a note) and one behind a count cap are not.
        let nondeterministic = match &outcome {
            Outcome::RefusedPower { .. } => true,
            Outcome::DeferredBudget { stop, .. } => {
                self.time_stop && self.stop == Some(*stop) && !self.nodes[id as usize].overflow
            }
            _ => false,
        };
        let evidence = tried.then(|| self.evidence(id));
        let mut summary = self.summary(id, &outcome);
        if let Some(note) = &self.nodes[id as usize].note {
            summary.push_str(" (");
            summary.push_str(note);
            summary.push(')');
        }
        let hypothesis = self.hypothesis(id);
        let n = &mut self.nodes[id as usize];
        debug_assert!(!n.finalised, "a node leaves the frontier once");
        n.finalised = true;
        let measured = if tried && kind != OutcomeKind::Memoised {
            n.measured.clone()
        } else {
            None
        };
        let node = TraceNode {
            id: Self::node_id(id),
            parent: n.parent.map(Self::node_id),
            stage: n.stage,
            hypothesis,
            seed_source: n.seed_source,
            prior_bits: n.prior_bits,
            measured,
            evidence_bits: evidence,
            tried,
            outcome,
            evaluations: n.evaluations,
            cpu_ms: n.cpu.as_millis() as u64,
            nondeterministic,
            summary,
        };
        let (parent, family) = (n.parent, n.family.clone());
        match kind {
            OutcomeKind::PrunedFloor | OutcomeKind::PrunedBound | OutcomeKind::PrunedBeam => {
                self.progress.pruned += 1
            }
            OutcomeKind::DeferredBudget | OutcomeKind::DeferredPrior => self.progress.deferred += 1,
            _ => {}
        }
        if tried {
            self.progress.tried += 1;
        } else {
            self.progress.not_tried += 1;
        }
        self.note_family(id, kind);
        self.sink.insert(id, parent, family, node);
        self.trace_wall += t.elapsed();
    }

    fn note_family(&mut self, id: u32, kind: OutcomeKind) {
        let n = &self.nodes[id as usize];
        let Some(f) = n.family.clone() else {
            return;
        };
        let bits = kind.tried().then(|| self.evidence(id));
        let stage = n.stage;
        let row = self.families.entry(f.clone()).or_insert(FamilyCoverage {
            family: f,
            state: FamilyState::Deferred,
            deepest_stage: None,
            best_bits: None,
            missing_block: None,
            deferred_as: None,
        });
        match kind {
            k if k.tried() => {
                row.state = FamilyState::Tried;
                row.deferred_as = None;
                row.missing_block = None;
                row.deepest_stage = Some(row.deepest_stage.map_or(stage, |s| s.max(stage)));
                let b = bits.unwrap_or(f32::NEG_INFINITY);
                row.best_bits = Some(row.best_bits.map_or(b, |x| x.max(b)));
            }
            OutcomeKind::Unsupported if row.state != FamilyState::Tried => {
                row.state = FamilyState::Unsupported;
            }
            OutcomeKind::DeferredPrior
            | OutcomeKind::DeferredBudget
            | OutcomeKind::RefusedPower
                if row.state == FamilyState::Deferred =>
            {
                row.deferred_as = Some(kind);
            }
            _ => {}
        }
    }

    fn not_tried_unsupported(&mut self, u: &UnsupportedStructure) {
        // A suspicion with no block has no skeleton: the trace names the structure itself.
        let mut n = self.blank(None, u32::MAX, u.slot, None);
        n.family = u.family.clone();
        n.prior_bits = u.posterior.map_or(0.0, crate::evidence::prior_bits);
        n.seed_source = SeedSource::Classification;
        n.label = Some((u.structure.clone(), u.missing_block.clone()));
        let id = self.push_node(n);
        self.finalise(
            id,
            Outcome::Unsupported {
                structure: u.structure.clone(),
                missing_block: u.missing_block.clone(),
                reference: u.reference.clone(),
            },
        );
        if let Some(f) = &u.family
            && let Some(row) = self.families.get_mut(f)
            && row.state == FamilyState::Unsupported
        {
            row.missing_block = Some(u.missing_block.clone());
        }
    }

    fn not_tried_root(&mut self, root: u32, outcome: Outcome) {
        let stage = self.next_slot(root, None).unwrap_or(Stage::S0);
        let n = self.blank(None, root, stage, None);
        let id = self.push_node(n);
        self.finalise(id, outcome);
    }

    fn not_tried_child(&mut self, parent: Option<u32>, root: u32, stage: Stage, alt: usize) {
        self.not_tried_group(parent, root, stage, alt, 1);
    }

    /// `count` hypotheses under one slot alternative of one parent that the stop left: ONE
    /// not-tried node, whose summary says how many (ADR-0021 §2.3: not-tried rows are few —
    /// one per deferred alternative, never one per parameter combination — which is what lets
    /// retention keep every one of them).
    fn not_tried_group(
        &mut self,
        parent: Option<u32>,
        root: u32,
        stage: Stage,
        alt: usize,
        count: usize,
    ) {
        let mut n = self.blank(parent, root, stage, Some(alt));
        if count > 1 {
            n.note = Some(format!("{count} parameter combinations, none reached"));
        }
        if let Some(f) = &self.skeleton(root).alternatives(stage)[alt].family
            && n.family.is_none()
        {
            n.family = Some(f.clone());
        }
        let id = self.push_node(n);
        let outcome = self.stop_outcome();
        self.finalise(id, outcome);
    }

    fn stop_outcome(&mut self) -> Outcome {
        if let Some(p) = self.refused_power {
            return Outcome::RefusedPower {
                policy: p.as_str().to_owned(),
            };
        }
        let stop = self.stop.unwrap_or(StopReason::Budget);
        self.stop_outcome_for(stop)
    }

    fn stop_outcome_for(&mut self, stop: StopReason) -> Outcome {
        self.queue_position += 1;
        Outcome::DeferredBudget {
            stop,
            queue_position: self.queue_position,
        }
    }

    // ---- outputs ----

    /// The stage output of `id` on the search window, recomputing from the deepest cached
    /// ancestor if evicted. `Err(())` when the budget cannot pay for the recomputation or an
    /// evaluation failed (the stop is set).
    fn output(&mut self, id: u32) -> Result<Arc<E::Output>, ()> {
        let src = self.nodes[id as usize].reuse.unwrap_or(id);
        if let Some(o) = self.cache.get(src) {
            return Ok(o);
        }
        let chain = self.chain(src);
        let mut start = 0;
        let mut out: Option<Arc<E::Output>> = None;
        for (i, &c) in chain.iter().enumerate().rev() {
            let c = self.nodes[c as usize].reuse.unwrap_or(c);
            if let Some(o) = self.cache.get(c) {
                start = i + 1;
                out = Some(o);
                break;
            }
        }
        let need = (chain.len() - start) as u64;
        if !self.afford(need, true) {
            self.halt(StopReason::Budget);
            return Err(());
        }
        for &c in &chain[start..] {
            let (cand, range) = self.candidate(c);
            let t = Instant::now();
            let res = self.ev.evaluate(&EvalRequest {
                window: EvalWindow::Search,
                stage: self.nodes[c as usize].stage,
                candidate: &cand,
                new_nodes: range,
                parent: out.as_deref(),
            });
            let dt = t.elapsed();
            self.cpu += dt;
            self.used.evaluations += 1;
            // Charged to the node whose output is being recomputed.
            self.nodes[id as usize].evaluations += 1;
            self.nodes[id as usize].cpu += dt;
            match res {
                Ok(ev) => {
                    let o = Arc::new(ev.output);
                    let slot = self.nodes[c as usize].reuse.unwrap_or(c);
                    self.cache.put(slot, Arc::clone(&o), ev.output_bytes);
                    out = Some(o);
                }
                Err(e) => {
                    self.eval_error(e);
                    return Err(());
                }
            }
        }
        out.ok_or(())
    }

    fn eval_error(&mut self, e: EvalError) {
        match e {
            EvalError::SourceEnded => self.halt(StopReason::SourceEnded),
            EvalError::Evicted => self.halt(StopReason::Evicted),
            EvalError::Failed(msg) => {
                if self.error.is_none() {
                    self.error = Some(msg);
                }
                self.halt(StopReason::Exhausted);
            }
        }
    }

    // ---- the pass ----

    fn run_pass(&mut self, roots: &[u32]) {
        let mut frontier: Vec<Live> = roots
            .iter()
            .filter_map(|&r| {
                self.next_slot(r, None).map(|s| Live {
                    at: At::Root(r),
                    next: s,
                })
            })
            .collect();
        for stage in Stage::ALL {
            if frontier.is_empty() {
                break;
            }
            let (here, carry): (Vec<Live>, Vec<Live>) =
                frontier.into_iter().partition(|l| l.next == stage);
            frontier = carry;
            if here.is_empty() {
                continue;
            }
            if self.stop.is_some() {
                frontier.extend(here);
                continue;
            }
            let selected = self.expand_level(stage, here);
            self.check_plateau();
            frontier.extend(selected);
            self.beam = frontier.len() as u32;
            self.emit();
        }
        if self.stop.is_some() {
            for l in frontier {
                self.defer_live(l);
            }
        }
    }

    /// A live entry the stop left unexpanded: its next slot's alternatives become not-tried
    /// children, and a node itself leaves the frontier as survived.
    fn defer_live(&mut self, l: Live) {
        match l.at {
            At::Root(r) => {
                let outcome = self.stop_outcome();
                self.not_tried_root(r, outcome);
            }
            At::Node(id) => {
                let root = self.nodes[id as usize].root;
                let alts = self.skeleton(root).alternatives(l.next).len();
                for a in 0..alts {
                    self.not_tried_child(Some(id), root, l.next, a);
                }
                self.finalise(
                    id,
                    Outcome::Survived {
                        children: alts as u32,
                    },
                );
            }
        }
    }

    fn expand_level(&mut self, stage: Stage, mut here: Vec<Live>) -> Vec<Live> {
        // Expand in search-key order (roots by prior), stable by id.
        let key = |e: &Self, l: &Live| match l.at {
            At::Root(r) => e.spec.roots[r as usize].prior_bits,
            At::Node(id) => e.search_key(id),
        };
        let mut keyed: Vec<(f32, usize, Live)> = here
            .drain(..)
            .enumerate()
            .map(|(i, l)| (key(self, &l), i, l))
            .collect();
        keyed.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));

        let mut children: Vec<u32> = Vec::new();
        let mut child_count: HashMap<u32, u32> = HashMap::new();
        let mut expanded_nodes: Vec<u32> = Vec::new();
        let mut unplanned: Vec<Live> = Vec::new();
        let mut plans: Vec<Plan> = Vec::new();
        let mut outputs: Vec<Arc<E::Output>> = Vec::new();

        for (_, _, l) in keyed {
            if self.stop.is_some() {
                unplanned.push(l);
                continue;
            }
            let (parent, root) = match l.at {
                At::Root(r) => (None, r),
                At::Node(id) => (Some(id), self.nodes[id as usize].root),
            };
            let parent_out = match parent {
                Some(p) => match self.output(p) {
                    Ok(o) => {
                        outputs.push(o);
                        Some(outputs.len() - 1)
                    }
                    Err(()) => {
                        unplanned.push(l);
                        continue;
                    }
                },
                None => None,
            };
            match l.at {
                At::Root(r) => {
                    self.root_tried[r as usize] = true;
                }
                At::Node(id) => expanded_nodes.push(id),
            }
            let n_alts = self.skeleton(root).alternatives(stage).len();
            for alt in 0..n_alts {
                let made = self.plan_alternative(
                    parent,
                    root,
                    stage,
                    alt,
                    parent_out,
                    &outputs,
                    &mut plans,
                    &mut children,
                );
                if let Some(p) = parent {
                    *child_count.entry(p).or_default() += made;
                }
            }
        }

        // Evaluate plans in fixed-size chunks.
        let mut i = 0;
        while i < plans.len() {
            self.check_control();
            if self.stop.is_some() {
                break;
            }
            let mut end = i;
            let mut reserved = 0u64;
            while end < plans.len() && end - i < CHUNK {
                if !self.afford(reserved + plans[end].max_evals, true) {
                    break;
                }
                reserved += plans[end].max_evals;
                end += 1;
            }
            if end == i {
                self.halt(StopReason::Budget);
                break;
            }
            let chunk = &plans[i..end];
            let results = self.evaluate_chunk(chunk, &outputs);
            for (plan, res) in chunk.iter().zip(results) {
                if let Some(id) = self.accept(plan, res) {
                    children.push(id);
                }
            }
            i = end;
            self.emit();
        }
        // Plans the stop left: not tried, one node per (parent, slot alternative).
        let mut left = Vec::new();
        for plan in plans.drain(i..) {
            group_into(&mut left, (plan.parent, plan.root, plan.stage, plan.alt));
        }
        for ((parent, root, stage, alt), count) in left {
            self.not_tried_group(parent, root, stage, alt, count);
            if let Some(p) = parent {
                *child_count.entry(p).or_default() += 1;
            }
        }
        drop(outputs);
        // Same-level duplicates: memoised on the one that ran, not tried if it did not.
        self.pending_keys.clear();
        let mut left = Vec::new();
        for d in std::mem::take(&mut self.later) {
            match self.memo.get(&d.key).copied() {
                Some(reused) => children.push(self.memoised(d, reused)),
                None => group_into(&mut left, (d.parent, d.root, d.stage, d.alt)),
            }
        }
        for ((parent, root, stage, alt), count) in left {
            self.not_tried_group(parent, root, stage, alt, count);
        }

        // Score and prune the level's children.
        let selected = self.select(stage, children);

        // Parents leave the frontier: expanded.
        for id in expanded_nodes {
            let c = child_count.get(&id).copied().unwrap_or(0);
            self.finalise(id, Outcome::Survived { children: c });
        }
        for l in unplanned {
            self.defer_live(l);
        }
        selected
    }

    /// Plans one alternative's children; returns how many nodes it created under the parent
    /// (planned, memoised or not-tried).
    #[allow(clippy::too_many_arguments)]
    fn plan_alternative(
        &mut self,
        parent: Option<u32>,
        root: u32,
        stage: Stage,
        alt: usize,
        parent_out: Option<usize>,
        outputs: &[Arc<E::Output>],
        plans: &mut Vec<Plan>,
        children: &mut Vec<u32>,
    ) -> u32 {
        let a = self.skeleton(root).alternatives(stage)[alt].clone();
        // Family consistency with a fixed ancestor choice.
        let (fam, fam_node) = match parent {
            Some(p) => (
                self.nodes[p as usize].family.clone(),
                self.nodes[p as usize].family_node,
            ),
            None => (self.spec.roots[root as usize].family.clone(), None),
        };
        if let (Some(f), Some(g)) = (&a.family, &fam)
            && f != g
        {
            let mut n = self.blank(parent, root, stage, Some(alt));
            n.family = Some(f.clone());
            let id = self.push_node(n);
            let conflicts_with = fam_node.map_or_else(|| "seed".to_owned(), Self::node_id);
            self.finalise(id, Outcome::NotApplicable { conflicts_with });
            return 1;
        }
        // This alternative commits the family when nothing above it did.
        let commits = a.family.is_some() && fam.is_none();
        let (family, family_node) = if commits {
            (a.family.clone(), None)
        } else {
            (fam, fam_node)
        };

        // Base prefix at the seed point.
        let steps_parent: Vec<(Stage, usize, BTreeMap<String, Value>)> = match parent {
            Some(p) => self
                .chain(p)
                .iter()
                .filter_map(|&c| {
                    let n = &self.nodes[c as usize];
                    n.alt.map(|al| (n.stage, al, n.bind.clone()))
                })
                .collect(),
            None => Vec::new(),
        };
        let empty = BTreeMap::new();
        let mut steps: Vec<(Stage, usize, &BTreeMap<String, Value>)> =
            steps_parent.iter().map(|(s, a, b)| (*s, *a, b)).collect();
        steps.push((stage, alt, &empty));
        let (base, range) = self.assemble(root, &steps);
        let ids: BTreeSet<&str> = a.nodes.iter().map(|n| n.id.as_str()).collect();
        let free: Vec<FreeParam> = self.spec.roots[root as usize]
            .free
            .iter()
            .filter(|f| parse_path(&f.path).is_some_and(|(id, _)| ids.contains(id)))
            .cloned()
            .collect();

        // Unknown blocks: unsupported, not tried.
        if let Some(missing) = a
            .nodes
            .iter()
            .find(|n| self.catalogue.descriptor(&n.block).is_none())
        {
            let mut n = self.blank(parent, root, stage, Some(alt));
            n.family = family.clone();
            let id = self.push_node(n);
            self.finalise(
                id,
                Outcome::Unsupported {
                    structure: a.family.clone().unwrap_or_else(|| a.id.clone()),
                    missing_block: missing.block.clone(),
                    reference: "ADR-0011 §1.5".to_owned(),
                },
            );
            return 1;
        }

        // Discrete options per parameter (proposals run now, sequentially).
        let mut axes: Vec<Vec<DiscreteOption>> = Vec::new();
        let mut sweeps: Vec<(String, Vec<Value>, Scale)> = Vec::new();
        for f in &free {
            if let Some((grid, scale)) = continuous_grid(&f.domain, f.seed.as_ref()) {
                if !grid.is_empty() {
                    sweeps.push((f.path.clone(), grid, scale));
                }
                continue;
            }
            if let Some(opts) = discrete_options(&f.domain) {
                axes.push(
                    opts.into_iter()
                        .map(|(v, p)| (BTreeMap::from([(f.path.clone(), v)]), p, false))
                        .collect(),
                );
                continue;
            }
            if let Domain::Proposal(op) = f.domain {
                match self.call_proposal(
                    op,
                    &f.path,
                    stage,
                    &base,
                    parent_out.map(|i| &*outputs[i]),
                ) {
                    Ok(reply) => axes.push(
                        reply
                            .suggestions
                            .into_iter()
                            .map(|s| (s.bind, s.prior_bits.clamp(-8.0, 0.0), true))
                            .collect(),
                    ),
                    Err(()) => {
                        // Budget or failure: this alternative is not tried.
                        let mut n = self.blank(parent, root, stage, Some(alt));
                        n.family = family.clone();
                        let id = self.push_node(n);
                        let outcome = self.stop_outcome();
                        self.finalise(id, outcome);
                        return 1;
                    }
                }
            }
        }
        // An operator that proposed nothing: looked, found nothing.
        if axes.iter().any(Vec::is_empty) {
            let mut n = self.blank(parent, root, stage, Some(alt));
            n.family = family.clone();
            n.evaluated = true;
            n.chain_mask |= stage_bit(stage);
            n.measured = Some(Measured {
                metric: default_metric(stage),
                raw: 0.0,
                n: 0,
                bits: 0.0,
                quality: 0.0,
                floor_bits: default_floor_bits(stage),
                look_elsewhere_bits: self.l(stage),
            });
            let id = self.push_node(n);
            self.finalise(
                id,
                Outcome::PrunedFloor {
                    floor_bits: default_floor_bits(stage).unwrap_or(0.0),
                    measured_bits: 0.0,
                },
            );
            return 1;
        }
        // Cartesian product of the discrete axes, best prior first, capped.
        let mut combos: Vec<DiscreteOption> = vec![(BTreeMap::new(), 0.0, false)];
        for axis in &axes {
            let mut next = Vec::new();
            for (b, p, prop) in &combos {
                for (ab, ap, aprop) in axis {
                    let mut nb = b.clone();
                    nb.extend(ab.clone());
                    next.push((nb, p + ap, *prop || *aprop));
                }
            }
            next.sort_by(|a, b| b.1.total_cmp(&a.1));
            combos = next;
        }
        let over_cap = combos.len().saturating_sub(MAX_DISCRETE_CHILDREN);
        combos.truncate(MAX_DISCRETE_CHILDREN);

        let parent_key = parent.map_or_else(String::new, |p| self.nodes[p as usize].key.clone());
        let parent_prior = parent.map_or(self.spec.roots[root as usize].prior_bits, |p| {
            self.nodes[p as usize].prior_bits
        });
        let parent_source = parent.map_or(self.spec.roots[root as usize].seed_source, |p| {
            self.nodes[p as usize].seed_source
        });
        let mut made = 0;
        if over_cap > 0 {
            let mut n = self.blank(parent, root, stage, Some(alt));
            n.family = family.clone();
            n.overflow = true;
            n.note = Some(format!(
                "{over_cap} lower-prior parameter combinations over the {MAX_DISCRETE_CHILDREN} cap"
            ));
            let id = self.push_node(n);
            let outcome = self.stop_outcome_for(StopReason::Budget);
            self.finalise(id, outcome);
            made += 1;
        }
        for (bind, prior, from_proposal) in combos {
            let mut cand = base.clone();
            for (p, v) in &bind {
                set_param(&mut cand.recipe.nodes, p, v);
            }
            for (path, grid, _) in &sweeps {
                set_param(&mut cand.recipe.nodes, path, &grid[0]);
            }
            let key = format!(
                "{parent_key}|{}:{}:{}:{}",
                stage.as_str(),
                serde_json::to_string(&cand.recipe.nodes[range.clone()]).unwrap_or_default(),
                serde_json::to_string(&bind).unwrap_or_default(),
                serde_json::to_string(&sweeps.iter().map(|(p, g, _)| (p, g)).collect::<Vec<_>>())
                    .unwrap_or_default()
            );
            let prior_bits = (parent_prior + prior).clamp(-8.0, 0.0);
            let seed_source = if from_proposal {
                SeedSource::Proposal
            } else {
                parent_source
            };
            made += 1;
            let dup = Dup {
                parent,
                root,
                stage,
                alt,
                key: key.clone(),
                prior_bits,
                seed_source,
                family: family.clone(),
                family_node,
                commits,
            };
            if let Some(&reused) = self.memo.get(&key) {
                let id = self.memoised(dup, reused);
                children.push(id);
                continue;
            }
            if self.pending_keys.contains(&key) {
                // The same hypothesis is already planned in this level: reuse it once it runs.
                self.later.push(dup);
                continue;
            }
            if let Err(err) = cand.check_prefix(self.catalogue) {
                // The seed point is not a runnable prefix: the skeleton or template is at fault.
                let mut n = self.blank(parent, root, stage, Some(alt));
                n.family = family.clone();
                let id = self.push_node(n);
                self.finalise(
                    id,
                    Outcome::Unsupported {
                        structure: a.id.clone(),
                        missing_block: "invalid_prefix".to_owned(),
                        reference: match err {
                            CandidateError::Recipe(errs) => errs
                                .iter()
                                .map(RecipeError::to_string)
                                .collect::<Vec<_>>()
                                .join("; "),
                            other => other.to_string(),
                        },
                    },
                );
                continue;
            }
            let max_evals = 1 + sweeps
                .iter()
                .map(|(_, g, _)| g.len() as u64 - 1)
                .sum::<u64>();
            self.pending_keys.insert(key.clone());
            plans.push(Plan {
                parent,
                root,
                stage,
                alt,
                base: base.clone(),
                new_nodes: range.clone(),
                bind,
                sweeps: sweeps.clone(),
                prior_bits,
                seed_source,
                family: family.clone(),
                family_node,
                commits,
                key,
                parent_output: parent_out,
                max_evals,
            });
        }
        made
    }

    /// A memoised node: `d`'s hypothesis reuses `reused`'s evaluation (ADR-0021 §1 rule 3).
    fn memoised(&mut self, d: Dup, reused: u32) -> u32 {
        let r = &self.nodes[reused as usize];
        let mut n = self.blank(d.parent, d.root, d.stage, Some(d.alt));
        n.bind = r.bind.clone();
        n.swept = r.swept.clone();
        n.reuse = Some(r.reuse.unwrap_or(reused));
        n.b = r.b;
        n.ladder = r.ladder.clone();
        n.measured = r.measured.clone();
        n.check = r.check.clone();
        n.frames = r.frames;
        n.key = d.key;
        n.prior_bits = d.prior_bits;
        n.seed_source = d.seed_source;
        n.family = d.family;
        n.family_node = d.family_node;
        n.evaluated = true;
        n.capped_sum += capped(d.stage, n.b);
        n.chain_mask |= stage_bit(d.stage);
        let id = self.push_node(n);
        if d.commits {
            self.nodes[id as usize].family_node = Some(id);
        }
        id
    }

    fn call_proposal(
        &mut self,
        op: ProposalOp,
        path: &str,
        stage: Stage,
        base: &Candidate,
        parent: Option<&E::Output>,
    ) -> Result<ProposalReply, ()> {
        let b = self.spec.budget;
        if b.max_proposal_calls
            .is_some_and(|m| self.used.proposal_calls >= m)
        {
            self.halt(StopReason::Budget);
            return Err(());
        }
        let grant = match b.max_assist_ops {
            Some(pool) => {
                let left = pool.saturating_sub(self.used.assist_ops);
                let calls_left = b
                    .max_proposal_calls
                    .map_or(1, |m| m.saturating_sub(self.used.proposal_calls).max(1));
                (left / calls_left).min(DEFAULT_MAX_OPS)
            }
            None => DEFAULT_MAX_OPS,
        };
        if grant == 0 {
            self.halt(StopReason::Budget);
            return Err(());
        }
        let t = Instant::now();
        let res = self.ev.propose(&ProposeRequest {
            op,
            path,
            stage,
            candidate: base,
            parent,
            budget: AssistBudget { max_ops: grant },
        });
        self.cpu += t.elapsed();
        self.used.proposal_calls += 1;
        match res {
            Ok(reply) => {
                self.used.assist_ops += reply.ops.min(grant);
                self.counts[stage.index() as usize] += reply.hypotheses;
                Ok(reply)
            }
            Err(e) => {
                self.eval_error(e);
                Err(())
            }
        }
    }

    fn evaluate_chunk(
        &mut self,
        chunk: &[Plan],
        outputs: &[Arc<E::Output>],
    ) -> Vec<PlanResult<E::Output>> {
        let workers = (self.threads as usize).min(chunk.len()).max(1);
        let ev = self.ev;
        let catalogue = self.catalogue;
        let run = |p: &Plan| run_plan(ev, catalogue, p, p.parent_output.map(|i| &*outputs[i]));
        if workers == 1 {
            return chunk.iter().map(run).collect();
        }
        let next = AtomicUsize::new(0);
        let slots: Mutex<Vec<Option<PlanResult<E::Output>>>> =
            Mutex::new((0..chunk.len()).map(|_| None).collect());
        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= chunk.len() {
                            break;
                        }
                        let r = run(&chunk[i]);
                        slots.lock().unwrap_or_else(|e| e.into_inner())[i] = Some(r);
                    }
                });
            }
        });
        slots
            .into_inner()
            .unwrap_or_else(|e| e.into_inner())
            .into_iter()
            .map(|r| r.expect("every plan ran"))
            .collect()
    }

    /// Turns a plan's result into a node (or records its failure).
    fn accept(&mut self, plan: &Plan, res: PlanResult<E::Output>) -> Option<u32> {
        self.used.evaluations += res.evaluations;
        self.counts[plan.stage.index() as usize] += res.evaluations;
        self.cpu += res.cpu;
        if let Some(e) = res.error.clone() {
            self.eval_error(e);
        }
        let Some((b, out)) = res.best else {
            // Nothing ran (every point failed or the stop hit first): not tried.
            self.not_tried_child(plan.parent, plan.root, plan.stage, plan.alt);
            return None;
        };
        let mut n = self.blank(plan.parent, plan.root, plan.stage, Some(plan.alt));
        n.bind = res.bind;
        n.swept = res.swept;
        n.key = plan.key.clone();
        n.prior_bits = plan.prior_bits;
        n.seed_source = plan.seed_source;
        n.family = plan.family.clone();
        n.family_node = plan.family_node;
        n.evaluated = true;
        n.b = b;
        n.capped_sum += capped(plan.stage, b);
        n.chain_mask |= stage_bit(plan.stage);
        n.evaluations = res.evaluations;
        n.cpu = res.cpu;
        let mut primary: Option<(String, Evidence)> = None;
        for ne in &out.evidence {
            for e in ne.evidence.iter().filter(|e| e.stage == plan.stage) {
                n.ladder
                    .push(StageEvidence::from_evidence(ne.node.clone(), e));
                if primary.as_ref().is_none_or(|(_, p)| e.bits > p.bits) {
                    primary = Some((ne.node.clone(), *e));
                }
                if e.metric == MetricId::CheckDistinctValid {
                    n.frames = n.frames.max(differences_of(e));
                }
            }
        }
        n.measured = Some(match primary {
            Some((_, e)) => Measured {
                metric: e.metric,
                raw: e.raw,
                n: e.n,
                bits: e.bits,
                quality: e.quality,
                floor_bits: default_floor_bits(plan.stage),
                look_elsewhere_bits: self.l(plan.stage),
            },
            None => Measured {
                metric: default_metric(plan.stage),
                raw: 0.0,
                n: 0,
                bits: b,
                quality: quality_from_bits(b),
                floor_bits: default_floor_bits(plan.stage),
                look_elsewhere_bits: self.l(plan.stage),
            },
        });
        n.check = out.check.clone();
        let id = self.push_node(n);
        if plan.commits {
            self.nodes[id as usize].family_node = Some(id);
        }
        self.cache.put(id, Arc::new(out.output), out.output_bytes);
        self.memo.insert(plan.key.clone(), id);
        Some(id)
    }

    /// Floor, completion, bound and beam at one level (§1.3, §3.1 step 5).
    fn select(&mut self, stage: Stage, children: Vec<u32>) -> Vec<Live> {
        let floor = default_floor_bits(stage);
        let mut open: Vec<u32> = Vec::new();
        for id in children {
            let (stage_reached, ev) = self.rank_key(id);
            self.note_score(stage_reached, ev);
            let b = self.nodes[id as usize].b;
            if let Some(f) = floor
                && b < f
            {
                self.finalise(
                    id,
                    Outcome::PrunedFloor {
                        floor_bits: f,
                        measured_bits: b,
                    },
                );
                continue;
            }
            let root = self.nodes[id as usize].root;
            if self.next_slot(root, Some(stage)).is_none() {
                self.nodes[id as usize].complete = true;
                self.complete.push(id);
                self.maybe_validate_early(id);
                continue;
            }
            if stage >= Stage::S5 {
                self.maybe_validate_early(id);
            }
            open.push(id);
        }
        // Optimistic-bound pruning against the best complete result, in rank terms.
        if let Some(best) = self.best_complete() {
            open.retain(|&id| {
                let root = self.nodes[id as usize].root;
                let Some(rem) = self.remaining(root, stage, true) else {
                    return true;
                };
                let bound = (self.deepest_slot(root), self.evidence(id) + rem);
                if bound.0 < best.0 || (bound.0 == best.0 && bound.1 < best.1) {
                    self.finalise(
                        id,
                        Outcome::PrunedBound {
                            bound_bits: bound.1,
                            best_bits: best.1,
                        },
                    );
                    false
                } else {
                    true
                }
            });
        }
        // The beam.
        let mut keyed: Vec<(f32, u32)> = open.iter().map(|&id| (self.search_key(id), id)).collect();
        keyed.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        let width = beam_width(stage);
        let mut per: HashMap<(Option<String>, String), usize> = HashMap::new();
        let mut selected = Vec::new();
        for (rank, (_, id)) in keyed.into_iter().enumerate() {
            let n = &self.nodes[id as usize];
            let k = (n.family.clone(), self.skeleton(n.root).key());
            let used = per.get(&k).copied().unwrap_or(0);
            let cause = if selected.len() >= width {
                Some(BeamCause::Width)
            } else if used >= DIVERSITY_CAP {
                Some(BeamCause::Diversity)
            } else {
                None
            };
            match cause {
                None => {
                    *per.entry(k).or_default() += 1;
                    let next = self.next_slot(n.root, Some(stage)).unwrap_or(Stage::S6);
                    selected.push(Live {
                        at: At::Node(id),
                        next,
                    });
                }
                Some(cause) => self.finalise(
                    id,
                    Outcome::PrunedBeam {
                        rank: rank as u32 + 1,
                        width: width as u32,
                        cause,
                    },
                ),
            }
        }
        if self.solved.is_some() {
            self.halt(StopReason::Solved);
        }
        selected
    }

    // ---- validation ----

    fn maybe_validate_early(&mut self, id: u32) {
        let id = self.nodes[id as usize].reuse.unwrap_or(id);
        if self.solved.is_some() || self.nodes[id as usize].holdout.is_some() {
            return;
        }
        let n = &self.nodes[id as usize];
        // A trigger, not a verdict: search-window evidence that could clear the analytic floor.
        if n.stage >= Stage::S5 && self.evidence(id) >= self.spec.solve.min_analytic_holdout_bits {
            self.validate(id);
        }
    }

    /// Runs `id`'s whole chain over `window`, charging every stage as an evaluation. `None` when
    /// an evaluation failed (recorded through [`Self::eval_error`] for hold-out; a null window's
    /// failure only means the control did not run).
    fn run_chain(&mut self, id: u32, window: EvalWindow) -> Option<ChainRun> {
        let chain = self.chain(id);
        let mut out: Option<E::Output> = None;
        let mut run = ChainRun {
            capped_sum: 0.0,
            mask: 0,
            analytic: Vec::new(),
            ladder: Vec::new(),
            differences: 0,
            check: None,
            frames: Vec::new(),
        };
        for &c in &chain {
            let (cand, range) = self.candidate(c);
            let stage = self.nodes[c as usize].stage;
            let t = Instant::now();
            let res = self.ev.evaluate(&EvalRequest {
                window,
                stage,
                candidate: &cand,
                new_nodes: range,
                parent: out.as_ref(),
            });
            let dt = t.elapsed();
            self.cpu += dt;
            self.used.evaluations += 1;
            if matches!(window, EvalWindow::Null(_)) {
                self.null_evals += 1;
            }
            self.nodes[id as usize].evaluations += 1;
            self.nodes[id as usize].cpu += dt;
            let ev = match res {
                Ok(ev) => ev,
                Err(e) => {
                    if window == EvalWindow::Holdout {
                        self.eval_error(e);
                    }
                    return None;
                }
            };
            let b = stage_bits(&ev.evidence, stage);
            run.capped_sum += capped(stage, b);
            run.mask |= stage_bit(stage);
            if let Some(a) = analytic_stage_bits(&ev.evidence, stage) {
                run.analytic.push((stage, capped(stage, a)));
            }
            for n in &ev.evidence {
                run.ladder.extend(
                    n.evidence
                        .iter()
                        .filter(|e| e.stage == stage)
                        .map(|e| StageEvidence::from_evidence(n.node.clone(), e)),
                );
            }
            if let Some(check) = ev.check {
                // `width` and `differences` from one block (T-575 review): the summary's node.
                run.differences = differences(&ev.evidence, check.node.as_deref());
                run.check = Some(check);
            }
            if window == EvalWindow::Holdout && !ev.frames.is_empty() {
                // The deepest stage that decodes wins: its frames carry the most structure.
                run.frames = ev.frames;
                run.frames.truncate(MAX_HOLDOUT_FRAMES);
            }
            out = Some(ev.output);
        }
        Some(run)
    }

    /// `L_j` summed over the stages a run touched.
    fn l_of(&self, mask: u8) -> f32 {
        Stage::ALL
            .iter()
            .filter(|s| mask & stage_bit(**s) != 0)
            .map(|s| self.l(*s))
            .sum()
    }

    /// ADR-0022 §2.1 / §6 accounting of a hold-out run.
    fn holdout_evidence(&self, root: u32, run: &ChainRun) -> HoldoutEvidence {
        let origin = self.spec.roots[root as usize].check_origin;
        let inherited = origin.inherited_bits();
        // ADR-0022 §5.1: a **template-fixed** check tried zero hypotheses, so `L_check = 0` — the
        // S5 stage's own job-wide count is not its charge (T-884 item 4). Every other origin pays
        // that count plus whatever it inherited.
        let l5 = if origin == CheckOrigin::TemplateFixed {
            0.0
        } else {
            self.l(Stage::S5)
        };
        // An unknown inherited charge is left out of the two sums below — which makes them upper
        // bounds — and `l_check` is `None`, which the solve rule and the confirm gate refuse.
        let extra = inherited.unwrap_or(0.0);
        let l_check = inherited.map(|i| l5 + i);
        let width = run.check.as_ref().map(|c| c.width);
        let reported = run
            .analytic
            .iter()
            .find(|(s, _)| *s == Stage::S5)
            .map(|(_, b)| b - l5 - extra);
        // ADR-0022 §4.2: a check is worth at most `width × differences − L_check`, whatever the
        // block reports — the same clamp `ConfirmPolicy.synthesized` applies to the stored row, so
        // the solve rule and the confirm gate read one number. The excess comes off the analytic
        // total too, since the S5 bits are part of it. No width: the clamp cannot be computed, and
        // the solve rule refuses a check with no width anyway.
        let check_bits = reported.map(|b| match width {
            Some(w) => b.min(w as f32 * run.differences as f32 - l5 - extra),
            None => b,
        });
        let excess = match (reported, check_bits) {
            (Some(r), Some(c)) => (r - c).max(0.0),
            _ => 0.0,
        };
        // Each analytic stage pays its own `L_j`; the inherited `L_check` is charged once more.
        let analytic_bits = run
            .analytic
            .iter()
            .map(|(s, b)| b - if *s == Stage::S5 { l5 } else { self.l(*s) })
            .sum::<f32>()
            - extra
            - excess;
        HoldoutEvidence {
            evidence_bits: run.capped_sum - self.l_of(run.mask),
            analytic_bits,
            check_bits,
            l_check,
            check_width: width,
            differences: run.differences,
            check_origin: origin,
            stages: run.ladder.clone(),
            null_control: None,
        }
    }

    /// Re-runs `id`'s whole chain on the hold-out window (§3.1 step 7). A memoised node is
    /// validated through the node it reused — the same prefix is never paid for twice.
    fn validate(&mut self, id: u32) {
        let id = self.nodes[id as usize].reuse.unwrap_or(id);
        if self.nodes[id as usize].holdout.is_some() {
            return;
        }
        let need = self.chain(id).len() as u64;
        if !self.afford(need, false) {
            return;
        }
        let prev = self.progress.state;
        self.set_state(JobState::Validating);
        let Some(run) = self.run_chain(id, EvalWindow::Holdout) else {
            if let Some(s) = prev {
                self.set_state(s);
            }
            return;
        };
        let root = self.nodes[id as usize].root;
        let stage = self.nodes[id as usize].stage;
        let mut ev = self.holdout_evidence(root, &run);
        let mut solved = self.spec.solve.met(stage, &ev);
        let mut capped = false;
        if solved && ev.check_origin.searched() && self.spec.profile.null_windows() > 0 {
            let nc = self.null_control(id, ev.evidence_bits);
            capped = nc.capped;
            solved = nc.passed();
            if capped {
                self.null_capped = true;
            }
            ev.null_control = Some(nc);
        }
        self.nodes[id as usize].holdout = Some(Holdout {
            ev,
            solved,
            capped,
            frames: run.frames,
        });
        if solved && self.solved.is_none() {
            self.solved = Some(id);
            self.halt(StopReason::Solved);
        }
        if let Some(s) = prev {
            self.set_state(s);
        }
    }

    /// ADR-0021 §8.2: the winning prefix, **unchanged and unrefit**, over `K` null windows. It can
    /// only cap. Recorded whether or not it fires; a control that could not run is not a pass.
    fn null_control(&mut self, id: u32, holdout_bits: f32) -> NullControl {
        let k = self.spec.profile.null_windows();
        let need = u64::from(k) * self.chain(id).len() as u64;
        let within_share = self
            .spec
            .budget
            .max_evaluations
            .is_none_or(|m| self.null_evals + need <= self.null_share(m));
        let mut nc = NullControl {
            k,
            ran: false,
            best_null_bits: f32::NEG_INFINITY,
            margin_bits: 0.0,
            capped: false,
        };
        if !within_share || !self.afford(need, false) {
            nc.best_null_bits = 0.0;
            return nc;
        }
        for i in 0..k {
            let Some(run) = self.run_chain(id, EvalWindow::Null(i)) else {
                nc.best_null_bits = nc.best_null_bits.max(0.0);
                return nc;
            };
            let bits = run.capped_sum - self.l_of(run.mask);
            nc.best_null_bits = nc.best_null_bits.max(bits);
        }
        nc.ran = true;
        nc.margin_bits = holdout_bits - nc.best_null_bits;
        // A NaN margin is not a pass: the control can only cap.
        nc.capped = nc.margin_bits.is_nan() || nc.margin_bits < MIN_NULL_MARGIN_BITS;
        nc
    }

    fn validate_top(&mut self) {
        let mut c: Vec<u32> = self
            .complete
            .iter()
            .map(|&id| self.nodes[id as usize].reuse.unwrap_or(id))
            .collect::<BTreeSet<u32>>()
            .into_iter()
            .collect();
        c.sort_by(|&a, &b| self.cmp_rank(b, a));
        for id in c.into_iter().take(VALIDATE_TOP) {
            if self.nodes[id as usize].holdout.is_none() {
                self.validate(id);
            }
            if self.solved.is_some() || self.error.is_some() {
                break;
            }
        }
    }

    fn cmp_rank(&self, a: u32, b: u32) -> std::cmp::Ordering {
        let (sa, ea) = self.rank_key(a);
        let (sb, eb) = self.rank_key(b);
        sa.cmp(&sb).then(ea.total_cmp(&eb)).then(b.cmp(&a))
    }

    fn finalise_complete(&mut self) {
        let mut c = self.complete.clone();
        c.sort_by(|&a, &b| self.cmp_rank(b, a));
        let Some(&winner) = c.first() else {
            return;
        };
        let top = self.evidence(winner);
        for (i, id) in c.into_iter().enumerate() {
            if self.nodes[id as usize].finalised {
                continue;
            }
            let outcome = if i == 0 {
                Outcome::Survived { children: 0 }
            } else {
                Outcome::EvaluatedWorse {
                    rank: i as u32 + 1,
                    gap_bits: (top - self.evidence(id)).max(0.0),
                }
            };
            self.finalise(id, outcome);
        }
    }

    // ---- the outcome ----

    fn results(&self) -> Vec<PipelineResult> {
        let mut pool: Vec<u32> = (0..self.nodes.len() as u32)
            .filter(|&id| {
                let n = &self.nodes[id as usize];
                n.evaluated && n.reuse.is_none() && n.alt.is_some() && self.sink.contains(id)
            })
            .collect();
        // Solved results first, then the §3.4 rank key.
        pool.sort_by(|&a, &b| {
            let sa = self.solved_flag(a);
            let sb = self.solved_flag(b);
            sb.cmp(&sa).then(self.cmp_rank(b, a))
        });
        let mut excluded: BTreeSet<u32> = BTreeSet::new();
        let mut out = Vec::new();
        for id in pool {
            if out.len() >= MAX_RESULTS {
                break;
            }
            if excluded.contains(&id) {
                continue;
            }
            for c in self.chain(id) {
                excluded.insert(c);
                if let Some(r) = self.nodes[c as usize].reuse {
                    excluded.insert(r);
                }
            }
            out.push(self.result(id, out.len() as u32 + 1));
        }
        out
    }

    fn solved_flag(&self, id: u32) -> bool {
        self.nodes[id as usize]
            .holdout
            .as_ref()
            .is_some_and(|h| h.solved)
    }

    fn result(&self, id: u32, rank: u32) -> PipelineResult {
        let n = &self.nodes[id as usize];
        let (cand, _) = self.candidate(id);
        let chain = self.chain(id);
        let stages: Vec<StageEvidence> = chain
            .iter()
            .flat_map(|&c| {
                let m = &self.nodes[c as usize];
                let src = m.reuse.unwrap_or(c);
                self.nodes[src as usize].ladder.clone()
            })
            .collect();
        let check = chain.iter().rev().find_map(|&c| {
            self.nodes[self.nodes[c as usize].reuse.unwrap_or(c) as usize]
                .check
                .clone()
        });
        let verdict = if self.solved_flag(id) {
            Verdict::Solved
        } else if n.holdout.as_ref().is_some_and(|h| h.capped) {
            // ADR-0021 §8.2: the null control caps at `framed`, and only caps.
            Verdict::unsolved_at(n.stage).min(Verdict::Framed)
        } else {
            Verdict::unsolved_at(n.stage)
        };
        let evidence_bits = self.evidence(id);
        let label = serde_json::to_value(verdict)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        let mut summary = format!(
            "{}: reached {} ({label}), {evidence_bits:.1} bits after look-elsewhere",
            cand.skeleton,
            n.stage.as_str()
        );
        if let Some(h) = &n.holdout {
            summary.push_str(&format!(
                "; hold-out {:.1} bits, {:.1} analytic",
                h.ev.evidence_bits, h.ev.analytic_bits
            ));
            if let Some(nc) = &h.ev.null_control {
                summary.push_str(&if !nc.ran {
                    "; null control could not run".to_owned()
                } else if nc.capped {
                    format!(
                        "; capped by the null control ({:.1}-bit margin < {MIN_NULL_MARGIN_BITS})",
                        nc.margin_bits
                    )
                } else {
                    format!("; null control passed ({:.1}-bit margin)", nc.margin_bits)
                });
            }
        }
        PipelineResult {
            rank,
            verdict,
            summary,
            recipe: cand.recipe,
            template: self.spec.roots[n.root as usize].template.clone(),
            stage_reached: n.stage,
            stages,
            evidence_bits,
            prior_bits: n.prior_bits,
            analytic_holdout_bits: n.holdout.as_ref().map(|h| h.ev.analytic_bits),
            check,
            frames_preview: Vec::new(),
            characterisation: None,
            holdout: n.holdout.as_ref().map(|h| h.ev.clone()),
        }
    }

    fn coverage(&self, stop: StopReason) -> Coverage {
        let mut sk = SkeletonCoverage {
            offered: self.spec.roots.len() as u32,
            unsupported: self.spec.unsupported.len() as u32,
            ..SkeletonCoverage::default()
        };
        for (i, _) in self.spec.roots.iter().enumerate() {
            if self.root_tried[i] {
                sk.tried += 1;
            } else {
                sk.deferred += 1;
            }
        }
        let hypotheses: u64 = self.counts.iter().sum();
        Coverage {
            profile: self.spec.profile,
            engine: crate::ENGINE.to_owned(),
            t: self.spec.started_at.clone(),
            skeletons: sk,
            families: self.families.values().cloned().collect(),
            budget: BudgetCoverage {
                spent: Spent {
                    wall_s: self.start.elapsed().as_secs_f64(),
                    cpu_s: self.cpu.as_secs_f64(),
                    evaluations: self.used.evaluations,
                },
                exhausted: stop == StopReason::Budget,
                stop,
            },
            hypotheses,
            look_elsewhere_bits: Stage::ALL.iter().map(|s| self.l(*s)).sum(),
            window: None,
        }
    }

    /// ADR-0021 §7A.3, read mechanically from what the engine did.
    fn reason(
        &self,
        stop: StopReason,
        results: &[PipelineResult],
        cov: &Coverage,
    ) -> Option<Reason> {
        if self.null_capped && self.solved.is_none() {
            // ADR-0021 §8.2: a candidate met the rule on hold-out and the null windows scored
            // too close to it — the engine cannot tell the fit from chance.
            return Some(Reason::Tied);
        }
        if self.solved.is_some()
            || matches!(
                stop,
                StopReason::Cancelled | StopReason::SourceEnded | StopReason::Evicted
            )
            || self.error.is_some()
        {
            return None;
        }
        // The highest-posterior suspicion has no block.
        let top_suspicion = self
            .spec
            .unsupported
            .iter()
            .filter_map(|u| u.posterior.map(|p| (p, u)))
            .max_by(|a, b| a.0.total_cmp(&b.0));
        // Roots carry their prior in bits; 2^prior is the posterior it came from.
        let top_root = self
            .spec
            .roots
            .iter()
            .map(|r| 2f64.powf(f64::from(r.prior_bits)))
            .fold(0.0f64, f64::max);
        if let Some((p, u)) = top_suspicion
            && p > top_root
            && cov.families.iter().any(|f| {
                Some(&f.family) == u.family.as_ref() && f.state == FamilyState::Unsupported
            })
        {
            return Some(Reason::UnsupportedStructure);
        }
        let deepest = results.first().map(|r| r.stage_reached);
        if deepest.is_none_or(|s| s == Stage::S0) {
            let s0_best = self
                .nodes
                .iter()
                .filter(|n| n.evaluated && n.stage == Stage::S0)
                .map(|n| n.b)
                .fold(f32::NEG_INFINITY, f32::max);
            let floor0 = default_floor_bits(Stage::S0).unwrap_or(0.0);
            if s0_best < floor0 {
                return Some(Reason::NoSignal);
            }
        }
        if stop == StopReason::Budget && self.progress.deferred > 0 {
            return Some(Reason::BudgetExhausted);
        }
        // Tied: the top two *complete* candidates within the 4-bit margin (ADR-0021 §7A.3).
        let mut complete = self.complete.clone();
        complete.sort_by(|&a, &b| self.cmp_rank(b, a));
        if let [a, b, ..] = complete.as_slice()
            && self.rank_key(*a).0 == self.rank_key(*b).0
            && self.evidence(*a) - self.evidence(*b) < 4.0
        {
            return Some(Reason::Tied);
        }
        Some(Reason::NothingScored)
    }

    /// ADR-0021 §5's key: everything the decisions are a function of, bar the input IQ.
    fn replay_key(&self) -> ReplayKey {
        let spec = self.spec;
        let mut templates: Vec<TemplateRef> = spec
            .roots
            .iter()
            .filter_map(|r| r.template.clone())
            .collect();
        templates.sort_by(|a, b| a.id.cmp(&b.id).then(a.version.cmp(&b.version)));
        templates.dedup();
        let mut names: BTreeSet<&str> = BTreeSet::new();
        for r in &spec.roots {
            for alts in r.skeleton.slots.values() {
                for a in alts {
                    names.extend(a.nodes.iter().map(|n| n.block.as_str()));
                }
            }
        }
        let blocks = names
            .into_iter()
            .filter_map(|name| {
                self.catalogue.descriptor(name).map(|d| BlockVersion {
                    name: name.to_owned(),
                    version: d.version,
                })
            })
            .collect();
        let roots: Vec<Value> = spec
            .roots
            .iter()
            .map(|r| {
                serde_json::json!({
                    "skeleton": r.skeleton,
                    "template": r.template,
                    "free": r.free,
                    "prior_bits": r.prior_bits,
                    "seed_source": r.seed_source,
                    "family": r.family,
                    "deferred": r.deferred,
                })
            })
            .collect();
        let unsupported: Vec<Value> = spec
            .unsupported
            .iter()
            .map(|u| {
                serde_json::json!({
                    "structure": u.structure,
                    "missing_block": u.missing_block,
                    "reference": u.reference,
                    "family": u.family,
                    "posterior": u.posterior,
                    "slot": u.slot,
                })
            })
            .collect();
        let seeding = serde_json::json!({
            "head": spec.head,
            "roots": roots,
            "unsupported": unsupported,
            "solve": spec.solve,
            "trace_bounds": spec.trace_bounds,
        });
        let seed_ref = hk_model::hash::ContentHash::of(&seeding)
            .map(|h| h.to_hex())
            .unwrap_or_default();
        let b = spec.budget;
        ReplayKey {
            engine: crate::ENGINE.to_owned(),
            templates,
            blocks,
            calibration_hash: None,
            window: None,
            profile: spec.profile,
            budget: ReplayBudget {
                max_evaluations: b.max_evaluations,
                max_proposal_calls: b.max_proposal_calls,
                max_assist_ops: b.max_assist_ops,
                max_cache_bytes: b.max_cache_bytes,
            },
            seed_ref,
        }
    }

    fn finish(mut self) -> SearchOutcome {
        let stop = if self.error.is_some() {
            None
        } else if self.solved.is_some() && self.stop != Some(StopReason::Cancelled) {
            Some(StopReason::Solved)
        } else {
            Some(self.stop.unwrap_or(StopReason::Exhausted))
        };
        let state = match (&self.error, stop) {
            (Some(_), _) => JobState::Failed,
            (None, Some(StopReason::Cancelled)) => JobState::Cancelled,
            _ => JobState::Done,
        };
        self.set_state(state);
        debug_assert!(
            self.nodes.iter().all(|n| n.finalised),
            "every node leaves the frontier through the trace sink"
        );
        let results = self.results();
        let holdout_frames = self
            .solved
            .filter(|_| {
                results
                    .first()
                    .is_some_and(|r| r.verdict == Verdict::Solved)
            })
            .and_then(|id| self.nodes[id as usize].holdout.as_ref())
            .map(|h| h.frames.clone())
            .unwrap_or_default();
        let coverage = self.coverage(stop.unwrap_or(StopReason::Exhausted));
        let reason = stop.and_then(|s| self.reason(s, &results, &coverage));
        let wall = self.start.elapsed();
        self.used.wall_s = wall.as_secs_f64();
        self.used.cpu_s = self.cpu.as_secs_f64();
        self.used.hypotheses = self.counts.iter().sum();
        self.used.cache_bytes = self.cache.peak;
        let trace_wall = self.trace_wall.as_secs_f64();
        let replay_key = self.replay_key();
        let retention_s = self.sink.cost().as_secs_f64();
        let trace = self.sink.finish();
        let trace_cost = TraceCost {
            wall_s: trace_wall,
            retention_s,
            decisions: trace.nodes.len() as u64 + trace.nodes_elided,
            bytes: trace.bytes,
            fraction: if wall.as_secs_f64() > 0.0 {
                trace_wall / wall.as_secs_f64()
            } else {
                0.0
            },
        };
        SearchOutcome {
            state,
            stop,
            error: self.error,
            results,
            trace,
            coverage,
            reason,
            used: self.used,
            trace_cost,
            nondeterministic: self.nondeterministic,
            replay_key,
            throttle_events: self.throttle_events,
            refused_power: self.refused_power,
            holdout_frames,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::{EnumDomain, FloatDomain, IntDomain};
    use crate::evidence::GroupId;

    fn node(entries: &[Evidence]) -> NodeEvidence {
        let mut set = EvidenceSet::new();
        for e in entries {
            set.push(*e).unwrap();
        }
        NodeEvidence {
            node: "n".into(),
            evidence: set,
        }
    }

    /// ADR-0022 §4.2 (T-575): `differences` is the check block's chance-corrected `raw`, never
    /// its `n` (every unit tested). A beacon repeating one payload twelve times is one difference.
    #[test]
    fn differences_are_the_chance_corrected_raw_never_the_tested_support() {
        let beacon = Evidence::new(
            Stage::S5,
            MetricId::CheckDistinctValid,
            GroupId::Undeclared,
            1.0,
            12,
            13.0,
        );
        assert_eq!(differences_of(&beacon), 1);
        assert_eq!(differences(&[node(&[beacon])], None), 1);
        // Never more than was tested, and nothing from an unreadable count.
        let mut e = beacon;
        e.raw = 40.0;
        assert_eq!(differences_of(&e), 12);
        e.raw = f32::NAN;
        assert_eq!(differences_of(&e), 0);
        e.raw = -3.0;
        assert_eq!(differences_of(&e), 0);
        // Paired with the block the check summary names; the smallest when it names none.
        let sync = Evidence::new(
            Stage::S5,
            MetricId::SyncExcess,
            GroupId::Undeclared,
            30.0,
            30,
            20.0,
        );
        let mut three = beacon;
        three.raw = 3.0;
        let mut other = node(&[three]);
        other.node = "crc_b".into();
        let both = [node(&[sync, beacon]), other];
        assert_eq!(differences(&both, Some("crc_b")), 3);
        assert_eq!(differences(&both, Some("n")), 1);
        assert_eq!(
            differences(&both, None),
            1,
            "unpaired: the weakest, never the max"
        );
        assert_eq!(differences(&both, Some("absent")), 0);
        assert_eq!(differences(&[node(&[sync])], None), 0);
    }

    /// ADR-0022 §4.3.1 (T-577, applied by T-575): the solve rule's width floor is the measured
    /// 16 — a CRC-8 carrying any number of bits does not solve.
    #[test]
    fn the_solve_rule_width_floor_is_t577s_measured_16() {
        let rule = SolveRule::default();
        assert_eq!(rule.min_check_width, 16);
        let h = |w: u32| HoldoutEvidence {
            evidence_bits: 200.0,
            analytic_bits: 200.0,
            check_bits: Some(160.0),
            l_check: Some(0.0),
            check_width: Some(w),
            differences: 20,
            check_origin: CheckOrigin::TemplateFixed,
            stages: Vec::new(),
            null_control: None,
        };
        assert!(!rule.met(Stage::S5, &h(8)));
        assert!(!rule.met(Stage::S5, &h(15)));
        assert!(rule.met(Stage::S5, &h(16)));
    }

    /// ADR-0022 §2.1 (T-575): only the four "contributes: yes" metrics pay for a confirmation.
    /// `sync_regularity` and `plausibility` are analytic but rank only; calibrated metrics never pay.
    #[test]
    fn only_paying_metrics_reach_the_confirm_key() {
        let mk =
            |stage, metric, bits| Evidence::new(stage, metric, GroupId::Undeclared, 1.0, 10, bits);
        let s4 = [node(&[
            mk(Stage::S4, MetricId::SyncExcess, 12.0),
            mk(Stage::S4, MetricId::SyncRegularity, 30.0),
        ])];
        // §13.1 takes the max within a group: without the restriction, the 30 bits of
        // sync_regularity would have been the stage's analytic bits.
        assert_eq!(analytic_stage_bits(&s4, Stage::S4), Some(12.0));
        let s6 = [node(&[mk(Stage::S6, MetricId::Plausibility, 40.0)])];
        assert_eq!(analytic_stage_bits(&s6, Stage::S6), None);
        let s2 = [node(&[mk(Stage::S2, MetricId::EyeOpen, 40.0)])];
        assert_eq!(analytic_stage_bits(&s2, Stage::S2), None);
    }

    /// T-884 item 7: the confirm key is built from ADR-0022 §2.1's table, not from every metric
    /// with a closed-form null. `sync_regularity` has an analytic null and is reported and ranked,
    /// but it may not pay for an irreversible confirm, so it is not in `analytic_stage_bits`.
    /// Red before the fix: the filter was `is_analytic`, and the regularity bits were counted.
    #[test]
    fn t884_the_confirm_key_counts_only_the_metrics_adr0022_2_1_lists() {
        let mut set = crate::EvidenceSet::new();
        set.push(Evidence::new(
            Stage::S4,
            MetricId::SyncExcess,
            GroupId::default(),
            0.0,
            100,
            12.0,
        ))
        .unwrap();
        set.push(Evidence::new(
            Stage::S4,
            MetricId::SyncRegularity,
            GroupId::default(),
            0.0,
            100,
            18.0,
        ))
        .unwrap();
        let ev = vec![NodeEvidence {
            node: "sync".into(),
            evidence: set,
        }];
        let paid = analytic_stage_bits(&ev, Stage::S4).expect("S4 emitted a paying metric");
        let all = stage_bits(&ev, Stage::S4);
        assert!(
            (paid - 12.0).abs() < 1e-3,
            "only sync_excess pays: {paid} (the stage total is {all})"
        );
        assert!(
            all > paid,
            "the stronger regularity bits still rank the node: {all}"
        );

        // A stage whose only evidence is a non-paying metric contributes nothing to the key.
        let mut only = crate::EvidenceSet::new();
        only.push(Evidence::new(
            Stage::S6,
            MetricId::Plausibility,
            GroupId::default(),
            0.0,
            10,
            7.0,
        ))
        .unwrap();
        let ev = vec![NodeEvidence {
            node: "fields".into(),
            evidence: only,
        }];
        assert_eq!(analytic_stage_bits(&ev, Stage::S6), None);
        assert!(stage_bits(&ev, Stage::S6) > 0.0);
    }

    #[test]
    fn paths_parse_only_in_the_adr_shape() {
        assert_eq!(
            parse_path("nodes[clock].params.symbol_rate_bd"),
            Some(("clock", "symbol_rate_bd"))
        );
        assert_eq!(parse_path("nodes[].params.x"), None);
        assert_eq!(parse_path("clock.symbol_rate_bd"), None);
        assert_eq!(parse_path("nodes[a].params.x.y"), None);
    }

    #[test]
    fn a_log_grid_is_the_adr_symbol_rate_grid() {
        let d = Domain::Float(FloatDomain {
            lo: 100.0,
            hi: 100_000.0,
            scale: Scale::Log,
            resolution: None,
        });
        let (g, scale) = continuous_grid(&d, Some(&Value::from(4800))).unwrap();
        assert_eq!(scale, Scale::Log);
        let v: Vec<f64> = g.iter().map(|x| x.as_f64().unwrap()).collect();
        assert_eq!(v[0], 4800.0, "the seed is evaluated first");
        // ×{½, 1, 2} ± 3 steps of 1 %: 21 points, the seed counted once.
        assert_eq!(v.len(), 21);
        assert!(v.iter().any(|x| (x - 2400.0).abs() < 1e-6));
        assert!(v.iter().any(|x| (x - 9600.0 * 1.03).abs() < 1e-6));
        // Clipped to the domain.
        let narrow = Domain::Float(FloatDomain {
            lo: 4700.0,
            hi: 4900.0,
            scale: Scale::Log,
            resolution: None,
        });
        let (g, _) = continuous_grid(&narrow, Some(&Value::from(4800))).unwrap();
        assert!(
            g.iter()
                .all(|x| (4700.0..=4900.0).contains(&x.as_f64().unwrap()))
        );
        assert!(g.len() >= 5);
    }

    #[test]
    fn linear_and_int_grids_stay_in_range() {
        let d = Domain::Float(FloatDomain {
            lo: -1000.0,
            hi: 1000.0,
            scale: Scale::Linear,
            resolution: None,
        });
        let (g, _) = continuous_grid(&d, None).unwrap();
        assert_eq!(g.len(), 7);
        assert_eq!(g[0].as_f64(), Some(0.0));
        let i = Domain::Int(IntDomain {
            lo: 1,
            hi: 4,
            resolution: None,
        });
        let (g, _) = continuous_grid(&i, Some(&Value::from(2))).unwrap();
        assert_eq!(g[0], Value::from(2));
        let mut v: Vec<i64> = g.iter().map(|x| x.as_i64().unwrap()).collect();
        v.sort_unstable();
        assert_eq!(v, [1, 2, 3, 4]);
    }

    #[test]
    fn enum_weights_order_and_become_priors() {
        let d = Domain::Enum(EnumDomain {
            values: vec![Value::from(512), Value::from(1200), Value::from(2400)],
            weights: Some(vec![0.25, 1.0, 0.5]),
        });
        let o = discrete_options(&d).unwrap();
        assert_eq!(o[0], (Value::from(1200), 0.0));
        assert_eq!(o[1], (Value::from(2400), -1.0));
        assert_eq!(o[2], (Value::from(512), -2.0));
    }

    #[test]
    fn the_cache_evicts_least_recently_used_within_its_cap() {
        let mut c: Cache<u8> = Cache::new(10);
        c.put(1, Arc::new(1), 4);
        c.put(2, Arc::new(2), 4);
        assert!(c.get(1).is_some());
        c.put(3, Arc::new(3), 4);
        assert!(c.get(2).is_none(), "2 was least recently used");
        assert!(c.get(1).is_some() && c.get(3).is_some());
        assert!(c.bytes <= 10 && c.peak <= 10);
        c.put(4, Arc::new(4), 11);
        assert!(
            c.get(4).is_none(),
            "an output larger than the cap is never cached"
        );
    }
}
