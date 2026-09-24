//! The search trace and the negative result (ADR-0021; declared in M-1 per ADR-0021 §12).
//!
//! **Producer: M-3** (a `TraceSink` at every frontier-removal site, retention on insert).
//! **Sealing and persistence: M-9.** **Serving: M-8.** This module fixes the shapes so those three
//! agree, and so the two distinctions ADR-0021 exists for are structural from the start:
//!
//! - **not tried is not ruled out** — [`Outcome::tried`] splits the closed outcome enum, and a
//!   not-tried node carries no measurement ([`TraceNode::check`]);
//! - **not searched is not unknown** — [`ResolutionKind::NotSearched`] is its own kind, and only a
//!   finished job may write `unknown` (ADR-0021 §7A.4).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::candidate::{Scale, SeedSource};
use crate::evidence::MetricId;
use crate::result::Verdict;
use crate::search::{Profile, StopReason};
use crate::stage::Stage;

/// Trace residency bounds (ADR-0021 §2.3), applied **on insert**.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceBounds {
    /// Node cap: the semantic limit.
    pub max_trace_nodes: u32,
    /// Byte cap: the hard limit (binds first at `deep`).
    pub max_trace_bytes: u32,
}

impl Profile {
    /// The profile's trace bounds (ADR-0021 §2.3; first guesses, measured in M-3).
    pub const fn trace_bounds(self) -> TraceBounds {
        let max_trace_nodes = match self {
            Profile::Quick => 128,
            Profile::Standard => 512,
            Profile::Deep => 2048,
        };
        TraceBounds {
            max_trace_nodes,
            max_trace_bytes: 256 * 1024,
        }
    }
}

/// A continuous sweep collapsed onto the node that owns it (ADR-0021 §1 rule 2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Swept {
    /// Parameter path.
    pub path: String,
    /// Lowest point.
    pub lo: f64,
    /// Highest point.
    pub hi: f64,
    /// Points evaluated.
    pub points: u32,
    /// Step scale.
    pub scale: Scale,
}

/// The hypothesis a trace node stands for.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceHypothesis {
    /// Skeleton or template, `id@version`.
    pub skeleton: String,
    /// The slot this node fixes.
    pub slot: Stage,
    /// The alternative chosen there.
    pub choice: String,
    /// `hk-mod@1` family, for filtering ("why not PSK").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// Bound parameters.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, Value>,
    /// Sweeps collapsed onto this node.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub swept: Vec<Swept>,
}

/// A tried node's measurement.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Measured {
    /// Metric.
    pub metric: MetricId,
    /// Natural-unit value.
    pub raw: f32,
    /// Support.
    pub n: u32,
    /// Significance, bits.
    pub bits: f32,
    /// Display quality.
    pub quality: f32,
    /// The stage floor it was tested against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor_bits: Option<f32>,
    /// The stage's look-elsewhere charge.
    pub look_elsewhere_bits: f32,
}

/// Why the beam dropped a scored node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeamCause {
    /// Outside beam width W.
    Width,
    /// Over the 2-per-family diversity cap.
    Diversity,
}

/// How a node left the frontier (ADR-0021 §2.2). **Closed**; adding a variant is a contract
/// change. Serialised as `"outcome": "<name>", "outcome_detail": {...}` on the node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "outcome_detail", rename_all = "snake_case")]
pub enum Outcome {
    // ---- tried ----
    /// Stayed in the beam and was expanded.
    Survived {
        /// Children created.
        children: u32,
    },
    /// Stage-j `b_j` below `floor_j`.
    PrunedFloor {
        /// The floor.
        floor_bits: f32,
        /// What was measured.
        measured_bits: f32,
    },
    /// Optimistic bound below the best complete result.
    PrunedBound {
        /// The node's optimistic bound.
        bound_bits: f32,
        /// The best complete result.
        best_bits: f32,
    },
    /// Scored, but outside the beam.
    PrunedBeam {
        /// Its rank at that stage.
        rank: u32,
        /// Beam width.
        width: u32,
        /// Width or diversity.
        cause: BeamCause,
    },
    /// A complete candidate that finished below the winner.
    EvaluatedWorse {
        /// Its rank.
        rank: u32,
        /// Bits behind the winner.
        gap_bits: f32,
    },
    /// Replaced by its refined child.
    RefinedInto {
        /// The child's node id.
        into: String,
    },
    /// Prefix-hash hit: reused another node's stage output, no new work.
    Memoised {
        /// The node reused.
        reused: String,
    },
    // ---- not tried: never elided ----
    /// Posterior < 0.02, sent to the side queue; budget never reached it.
    DeferredPrior {
        /// The family's posterior.
        posterior: f64,
        /// Its place in the side queue.
        queue_position: u32,
    },
    /// Enqueued and ready, but a cap hit first.
    DeferredBudget {
        /// Which stop.
        stop: StopReason,
        /// Its place in the queue.
        queue_position: u32,
    },
    /// No block exists for the structure.
    Unsupported {
        /// Structure (`psk`, `css`, `ofdm`).
        structure: String,
        /// Stable missing-block id (T-554's gap list).
        missing_block: String,
        /// Reference.
        #[serde(rename = "ref")]
        reference: String,
    },
    /// Inconsistent with a fixed ancestor choice.
    NotApplicable {
        /// The ancestor node id.
        conflicts_with: String,
    },
    /// The power policy refused the expansion.
    RefusedPower {
        /// Which policy (`battery`, `low`).
        policy: String,
    },
}

/// The outcome's name without its detail, for `elided` buckets and coverage rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeKind {
    /// See [`Outcome::Survived`].
    Survived,
    /// See [`Outcome::PrunedFloor`].
    PrunedFloor,
    /// See [`Outcome::PrunedBound`].
    PrunedBound,
    /// See [`Outcome::PrunedBeam`].
    PrunedBeam,
    /// See [`Outcome::EvaluatedWorse`].
    EvaluatedWorse,
    /// See [`Outcome::RefinedInto`].
    RefinedInto,
    /// See [`Outcome::Memoised`].
    Memoised,
    /// See [`Outcome::DeferredPrior`].
    DeferredPrior,
    /// See [`Outcome::DeferredBudget`].
    DeferredBudget,
    /// See [`Outcome::Unsupported`].
    Unsupported,
    /// See [`Outcome::NotApplicable`].
    NotApplicable,
    /// See [`Outcome::RefusedPower`].
    RefusedPower,
}

impl OutcomeKind {
    /// Whether the node was evaluated. The not-tried kinds are never elided (ADR-0021 §2.3).
    pub const fn tried(self) -> bool {
        matches!(
            self,
            OutcomeKind::Survived
                | OutcomeKind::PrunedFloor
                | OutcomeKind::PrunedBound
                | OutcomeKind::PrunedBeam
                | OutcomeKind::EvaluatedWorse
                | OutcomeKind::RefinedInto
                | OutcomeKind::Memoised
        )
    }
}

impl Outcome {
    /// The outcome's name.
    pub const fn kind(&self) -> OutcomeKind {
        match self {
            Outcome::Survived { .. } => OutcomeKind::Survived,
            Outcome::PrunedFloor { .. } => OutcomeKind::PrunedFloor,
            Outcome::PrunedBound { .. } => OutcomeKind::PrunedBound,
            Outcome::PrunedBeam { .. } => OutcomeKind::PrunedBeam,
            Outcome::EvaluatedWorse { .. } => OutcomeKind::EvaluatedWorse,
            Outcome::RefinedInto { .. } => OutcomeKind::RefinedInto,
            Outcome::Memoised { .. } => OutcomeKind::Memoised,
            Outcome::DeferredPrior { .. } => OutcomeKind::DeferredPrior,
            Outcome::DeferredBudget { .. } => OutcomeKind::DeferredBudget,
            Outcome::Unsupported { .. } => OutcomeKind::Unsupported,
            Outcome::NotApplicable { .. } => OutcomeKind::NotApplicable,
            Outcome::RefusedPower { .. } => OutcomeKind::RefusedPower,
        }
    }

    /// Whether the node was evaluated: "we looked" versus "we did not look".
    pub const fn tried(&self) -> bool {
        self.kind().tried()
    }
}

/// One frontier event (ADR-0021 §2.1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TraceNode {
    /// Node id (`n17`).
    pub id: String,
    /// Parent node id; `None` at the root.
    pub parent: Option<String>,
    /// Stage.
    pub stage: Stage,
    /// What it stands for.
    pub hypothesis: TraceHypothesis,
    /// Where the hypothesis came from.
    pub seed_source: SeedSource,
    /// Reported; never ranks, never sorts the trace.
    pub prior_bits: f32,
    /// `None` iff the outcome is a not-tried kind (a memoised hit may also carry none).
    pub measured: Option<Measured>,
    /// Cumulative prefix score after look-elsewhere; `None` if not tried.
    pub evidence_bits: Option<f32>,
    /// How it left the frontier, and the typed detail.
    #[serde(flatten)]
    pub outcome: Outcome,
    /// Derived from `outcome`, but **served**, so no client infers it.
    pub tried: bool,
    /// Evaluations this node accounts for (including collapsed sweeps).
    pub evaluations: u64,
    /// CPU spent, ms.
    pub cpu_ms: u64,
    /// Backend-rendered text.
    pub summary: String,
}

/// A node whose fields disagree with its outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TraceNodeError {
    /// `tried` does not match the outcome.
    TriedFlag,
    /// A not-tried node carries a measurement or evidence.
    NotTriedButMeasured,
    /// A tried, non-memoised node carries no measurement or evidence.
    TriedButUnmeasured,
}

impl TraceNode {
    /// The honesty invariants of ADR-0021 §2.1–§2.2: `tried` matches the outcome; a not-tried node
    /// has no `measured` and no `evidence_bits`; a tried node has both, except a `memoised` hit,
    /// which reuses another node's output and may carry none (ADR-0021 §1 rule 3).
    pub fn check(&self) -> Result<(), TraceNodeError> {
        let tried = self.outcome.tried();
        if self.tried != tried {
            return Err(TraceNodeError::TriedFlag);
        }
        let has = (self.measured.is_some(), self.evidence_bits.is_some());
        match (tried, self.outcome.kind(), has) {
            (false, _, (false, false)) => Ok(()),
            (false, _, _) => Err(TraceNodeError::NotTriedButMeasured),
            (true, OutcomeKind::Memoised, _) => Ok(()),
            (true, _, (true, true)) => Ok(()),
            (true, _, _) => Err(TraceNodeError::TriedButUnmeasured),
        }
    }
}

/// A count of nodes dropped by the bound: the trace is complete in counts, lossy in detail.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Elided {
    /// Stage.
    pub stage: Stage,
    /// Family, when the nodes committed to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// Their outcome (always a tried kind).
    pub outcome: OutcomeKind,
    /// How many.
    pub count: u64,
    /// Best bits among them.
    pub bits_max: f32,
    /// Worst bits among them.
    pub bits_min: f32,
    /// Evaluations they accounted for (the accounting identity, ADR-0021 §1).
    pub evaluations: u64,
}

/// What a finished (or never-run) analysis means (ADR-0021 §7A.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResolutionKind {
    /// Searched, and nothing solved: a positive, durable, dated finding.
    Unknown,
    /// Framed and check-valid, identity unknown: a real result (ADR-0021 §7A.5).
    StructuredUnidentified,
    /// The highest-posterior suspicion has no block.
    UnsupportedStructure,
    /// Never analysed, or the only attempt was cancelled or failed. **Not** `unknown`.
    NotSearched,
}

/// Why nothing won (ADR-0021 §7A.3). **Closed**; `no-signal` and `nothing-scored` never merge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reason {
    /// S0 itself measured below floor.
    NoSignal,
    /// Energy existed; every hypothesis measured below its floor; the beam emptied.
    NothingScored,
    /// ≥ 2 complete candidates within the 4-bit margin, none solved.
    Tied,
    /// The queue was non-empty when the budget ran out: the space was not covered.
    BudgetExhausted,
    /// The highest-posterior suspicion has no block.
    UnsupportedStructure,
}

/// Per-family coverage state: the decode-side grey.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FamilyState {
    /// Hypotheses under this family were evaluated.
    Tried,
    /// No block exists.
    Unsupported,
    /// Queued and not reached.
    Deferred,
}

/// One family's coverage row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FamilyCoverage {
    /// `hk-mod@1` family.
    pub family: String,
    /// State.
    pub state: FamilyState,
    /// `tried`: deepest stage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deepest_stage: Option<Stage>,
    /// `tried`: best bits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub best_bits: Option<f32>,
    /// `unsupported`: the missing block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing_block: Option<String>,
    /// `deferred`: which not-tried kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred_as: Option<OutcomeKind>,
}

/// Skeleton counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkeletonCoverage {
    /// Offered by seeding.
    pub offered: u32,
    /// Evaluated.
    pub tried: u32,
    /// Queued, not reached.
    pub deferred: u32,
    /// No block.
    pub unsupported: u32,
}

/// What the search spent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spent {
    /// Wall, s.
    pub wall_s: f64,
    /// CPU, s.
    pub cpu_s: f64,
    /// Evaluations.
    pub evaluations: u64,
}

/// Budget state at the end.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetCoverage {
    /// What was spent.
    pub spent: Spent,
    /// Whether a cap was hit.
    pub exhausted: bool,
    /// Why the search stopped.
    pub stop: StopReason,
}

/// The decode-side coverage map (ADR-0021 §7A.2): what was actually searched.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Coverage {
    /// Profile.
    pub profile: Profile,
    /// Engine version (`hk-synth@1…`).
    pub engine: String,
    /// When.
    pub t: String,
    /// Skeleton counts.
    pub skeletons: SkeletonCoverage,
    /// Per-family state.
    pub families: Vec<FamilyCoverage>,
    /// Budget.
    pub budget: BudgetCoverage,
    /// Hypotheses evaluated.
    pub hypotheses: u64,
    /// Reporting only; never a `ConfirmPolicy` input (ADR-0022 §2.3).
    pub look_elsewhere_bits: f32,
    /// The analysed window (`duration_s`, `samples`, `snr_db`, `bursts`, `clip_id`); M-6/M-9.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<Value>,
}

/// What `unsupported-structure` suspected (ADR-0021 §7A.6).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Suspected {
    /// Structure (`css`).
    pub structure: String,
    /// Missing block id.
    pub missing_block: String,
    /// `classification | features | template | operator`.
    pub suspected_by: String,
    /// Posterior of the suspicion, when a classification made it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub posterior: Option<f64>,
}

/// A template tried and not solved.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuledOut {
    /// Template id.
    pub template: String,
    /// Version.
    pub version: u32,
    /// Deepest stage reached.
    pub deepest_stage: Stage,
    /// Best bits.
    pub best_bits: f32,
}

/// Least margin, bits, the real hold-out result must hold over the best null window
/// (ADR-0021 §8.2: `min_null_margin = 8`, 256:1). ADR-0021's number; T-568 measures it.
pub const MIN_NULL_MARGIN_BITS: f32 = 8.0;

/// The shuffled-null control's record (ADR-0021 §8.2), kept **whether or not it fired**: "the
/// null control ran and passed with a 12.9-bit margin" is a checkable fact; an absence is not.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NullControl {
    /// Null windows asked for (the profile's `K`).
    pub k: u32,
    /// Whether all `k` ran. A control that could not run (budget, evaluator error) is **not** a
    /// pass: an open-search result without one neither solves nor confirms.
    pub ran: bool,
    /// Best hold-out-style `evidence_bits` the unchanged winning prefix reached on any null window.
    pub best_null_bits: f32,
    /// The real hold-out `evidence_bits` minus [`Self::best_null_bits`].
    pub margin_bits: f32,
    /// The verdict was **capped at `framed`** because the margin fell below
    /// [`MIN_NULL_MARGIN_BITS`]. The control can only cap; it never raises a verdict.
    pub capped: bool,
}

impl NullControl {
    /// Whether the control ran and did not cap — the only state in which a searched check may
    /// confirm (ADR-0022 §6 step 4).
    pub const fn passed(&self) -> bool {
        self.ran && !self.capped
    }
}

/// The negative result (ADR-0021 §7A.2), present whenever no result reached `solved`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resolution {
    /// What it means.
    pub kind: ResolutionKind,
    /// How deep the best result got; `None` when not searched.
    pub deepest_verdict: Option<Verdict>,
    /// Why nothing won; `None` when not searched.
    pub reason: Option<Reason>,
    /// What was searched; `None` when not searched.
    pub coverage: Option<Coverage>,
    /// `unsupported-structure`: what was suspected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspected: Option<Suspected>,
    /// The null control.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub null_control: Option<NullControl>,
    /// Templates tried and not solved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ruled_out: Vec<RuledOut>,
    /// ADR-0021 §10's retry advice (M-9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<Value>,
    /// ADR-0021 §4.1 (M-8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_summary: Option<Value>,
    /// ADR-0021 §5 (M-9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_key: Option<Value>,
    /// ADR-0021 §9.3: attached **after** the object is sealed, modifying nothing above.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub explanations: Vec<Value>,
    /// `not-searched` after a cancelled or failed job: when that was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_attempt: Option<String>,
    /// Backend-rendered text.
    pub summary: String,
}

impl Resolution {
    /// The resolution of an emitter nobody has analysed (or whose only attempt did not finish):
    /// `not-searched`, no coverage. **Not** `unknown`.
    pub fn not_searched(last_attempt: Option<String>) -> Self {
        Self {
            kind: ResolutionKind::NotSearched,
            deepest_verdict: None,
            reason: None,
            coverage: None,
            suspected: None,
            null_control: None,
            ruled_out: Vec::new(),
            retry: None,
            trace_summary: None,
            replay_key: None,
            explanations: Vec::new(),
            last_attempt,
            summary: "Not analysed.".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(outcome: Outcome, measured: bool) -> TraceNode {
        TraceNode {
            id: "n17".into(),
            parent: Some("n4".into()),
            stage: Stage::S1,
            hypothesis: TraceHypothesis {
                skeleton: "generic-fsk-framed@1".into(),
                slot: Stage::S1,
                choice: "fsk".into(),
                family: Some("fsk".into()),
                params: BTreeMap::new(),
                swept: Vec::new(),
            },
            seed_source: SeedSource::Estimate,
            prior_bits: -1.2,
            measured: measured.then_some(Measured {
                metric: MetricId::Bimodality,
                raw: 0.71,
                n: 4096,
                bits: 4.1,
                quality: 0.4,
                floor_bits: Some(6.0),
                look_elsewhere_bits: 2.3,
            }),
            evidence_bits: measured.then_some(7.1),
            tried: outcome.tried(),
            outcome,
            evaluations: 7,
            cpu_ms: 41,
            summary: String::new(),
        }
    }

    #[test]
    fn the_adr_node_serialises_with_outcome_and_detail_side_by_side() {
        let n = node(
            Outcome::PrunedFloor {
                floor_bits: 6.0,
                measured_bits: 4.1,
            },
            true,
        );
        n.check().unwrap();
        let v = serde_json::to_value(&n).unwrap();
        assert_eq!(v["outcome"], "pruned_floor");
        assert_eq!(v["outcome_detail"]["measured_bits"], 4.1f32 as f64);
        assert_eq!(v["seed_source"], "estimate");
        assert_eq!(v["tried"], true);
        let back: TraceNode = serde_json::from_value(v).unwrap();
        assert_eq!(back, n);
    }

    #[test]
    fn not_tried_is_not_ruled_out() {
        let deferred = node(
            Outcome::DeferredBudget {
                stop: StopReason::Budget,
                queue_position: 3,
            },
            false,
        );
        assert!(!deferred.tried);
        deferred.check().unwrap();
        // A not-tried node may not carry a measurement.
        let mut lying = deferred.clone();
        lying.measured = node(Outcome::Survived { children: 1 }, true).measured;
        assert_eq!(lying.check(), Err(TraceNodeError::NotTriedButMeasured));
        // And the flag must match the outcome.
        let mut flag = deferred;
        flag.tried = true;
        assert_eq!(flag.check(), Err(TraceNodeError::TriedFlag));
        // A tried node must be measured, except a memoised hit.
        assert_eq!(
            node(Outcome::Survived { children: 2 }, false).check(),
            Err(TraceNodeError::TriedButUnmeasured)
        );
        node(
            Outcome::Memoised {
                reused: "n9".into(),
            },
            false,
        )
        .check()
        .unwrap();

        let tried: Vec<_> = [
            OutcomeKind::Survived,
            OutcomeKind::PrunedFloor,
            OutcomeKind::PrunedBound,
            OutcomeKind::PrunedBeam,
            OutcomeKind::EvaluatedWorse,
            OutcomeKind::RefinedInto,
            OutcomeKind::Memoised,
            OutcomeKind::DeferredPrior,
            OutcomeKind::DeferredBudget,
            OutcomeKind::Unsupported,
            OutcomeKind::NotApplicable,
            OutcomeKind::RefusedPower,
        ]
        .iter()
        .map(|k| k.tried())
        .collect();
        assert_eq!(tried.iter().filter(|t| **t).count(), 7);
        assert_eq!(tried.iter().filter(|t| !**t).count(), 5);
    }

    #[test]
    fn not_searched_is_its_own_kind_with_no_coverage() {
        let r = Resolution::not_searched(None);
        assert_eq!(r.kind, ResolutionKind::NotSearched);
        assert!(r.coverage.is_none() && r.reason.is_none());
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["kind"], "not-searched");
        assert_eq!(
            serde_json::to_value(Reason::NothingScored).unwrap(),
            "nothing-scored"
        );
        assert_ne!(Reason::NoSignal, Reason::NothingScored);
    }

    #[test]
    fn trace_bounds_per_profile() {
        assert_eq!(Profile::Quick.trace_bounds().max_trace_nodes, 128);
        assert_eq!(Profile::Standard.trace_bounds().max_trace_nodes, 512);
        assert_eq!(Profile::Deep.trace_bounds().max_trace_nodes, 2048);
        assert_eq!(Profile::Deep.trace_bounds().max_trace_bytes, 262_144);
    }
}
