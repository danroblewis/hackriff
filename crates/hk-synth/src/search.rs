//! Search budgets, profiles, stop reasons, job states and the ML slot (ADR-0015 §3.3, §5.2).
//!
//! The engine that runs under this vocabulary is [`crate::engine`] (M-3, T-854); admission and
//! the power/throttle inputs are [`crate::admission`].
//!
//! # The budget unit (T-854, settling ADR-0015 §16.8 item 2 per docs/27 §6)
//!
//! T-552 measured (docs/27 §3–§4) that a proposal-operator call costs 0.3–2.4 s while a DSP
//! re-evaluation of one cached stage costs ≤ 5 ms, and that the per-operation cost of
//! `hk_estimate::assist` is near machine-independent (1.2–1.6 ns/op). A wall-clock budget
//! therefore buys wildly different amounts of search. So **how much search happens is budgeted
//! by count** — [`SynthBudget::max_proposal_calls`], a shared [`SynthBudget::max_assist_ops`]
//! pool handed to each call as an `assist::Budget{max_ops}`, and
//! [`SynthBudget::max_evaluations`] — and `wall_s` / `cpu_s` are **backstops** for the API and
//! power story, not the unit. A job bounded only by counts is deterministic (ADR-0021 §5);
//! one that stops on a wall or CPU cap is not, and says so.
//!
//! The counts are first guesses sized to fit inside the §3.3 wall backstops at docs/27's
//! measured rates (≈ 1.5 ns/assist op, ≤ 5 ms per cached-stage evaluation); M-13 re-baselines
//! them on the Jetson. `quick`'s six proposal calls are exactly docs/27 §4's "two open skeletons
//! × sync + codes + fields" minimum: no margin for a wrong first guess, which is docs/27 §6
//! item 3's finding, kept visible rather than hidden.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::stage::Stage;

/// A search budget profile (ADR-0015 §3.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    /// Context-menu default: templates and the top-2 open skeletons.
    Quick,
    /// "Analyze" default.
    #[default]
    Standard,
    /// Opt-in open search.
    Deep,
}

/// Default memo-cache cap, bytes (ADR-0015 §3.1 step 4: 256 MiB).
pub const DEFAULT_MAX_CACHE_BYTES: u64 = 256 * 1024 * 1024;

/// Per-job caps (ADR-0015 §3.3, `SynthBudget { wall_s, cpu_s, max_evaluations, max_iq_samples,
/// max_cache_bytes, threads }`, plus T-854's count caps for proposal operators, docs/27 §6).
///
/// The count caps decide how much search happens; wall and CPU are backstops (module docs).
/// A `None` count cap means the budget does not bound it.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SynthBudget {
    /// Wall-clock backstop, s. Hitting it makes the job non-replayable (ADR-0021 §5).
    pub wall_s: f64,
    /// CPU backstop, s (busy time summed over search threads).
    pub cpu_s: f64,
    /// Evaluation cap: one evaluation is one stage run over one window (a grid point, a hold-out
    /// re-run, or an evicted prefix recomputed). ADR-0021 §8.4's acceptance jobs set it for
    /// determinism.
    pub max_evaluations: Option<u64>,
    /// Proposal-operator calls (`assist.sync` / `assist.codes` / `assist.fields`), the expensive,
    /// variable part (docs/27 §4).
    #[serde(default)]
    pub max_proposal_calls: Option<u64>,
    /// Shared `hk_estimate::assist` word-operation pool across every proposal call. Each call is
    /// granted a fair share of what remains, never more than the assist crate's own default cap.
    #[serde(default)]
    pub max_assist_ops: Option<u64>,
    /// IQ-sample cap for acquisition (M-6; the engine does not read IQ).
    pub max_iq_samples: Option<u64>,
    /// Memo-cache cap, bytes. Not a stop: the least-recently-used stage output is evicted and
    /// recomputed (charged as evaluations) if needed again.
    pub max_cache_bytes: u64,
    /// Search threads (halved when throttled or on battery, never below 1).
    pub threads: u32,
}

impl Profile {
    /// The profile's default budget (§3.3 table for wall/CPU/threads; T-854's count caps).
    pub const fn budget(self) -> SynthBudget {
        let (wall_s, cpu_s, threads, evals, calls, ops) = match self {
            Profile::Quick => (3.0, 3.0, 1, 128, 6, 1_000_000_000),
            Profile::Standard => (20.0, 40.0, 2, 2_048, 40, 8_000_000_000),
            Profile::Deep => (120.0, 400.0, 4, 20_000, 240, 50_000_000_000),
        };
        SynthBudget {
            wall_s,
            cpu_s,
            max_evaluations: Some(evals),
            max_proposal_calls: Some(calls),
            max_assist_ops: Some(ops),
            max_iq_samples: None,
            max_cache_bytes: DEFAULT_MAX_CACHE_BYTES,
            threads,
        }
    }
}

/// Why a search stopped (ADR-0015 §3.3). First to hit wins; there are always ranked partial
/// results.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The solve rule was met on hold-out.
    Solved,
    /// No best-score gain > 1 bit over 25 % of the budget.
    Plateau,
    /// A budget cap hit first.
    Budget,
    /// Beam and deferred queue empty.
    Exhausted,
    /// Cancelled by the user.
    Cancelled,
    /// The source ended during acquisition or search.
    SourceEnded,
    /// The window left the ring before acquisition.
    Evicted,
}

/// An analyze job's state (ADR-0015 §5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Waiting for the single running slot.
    Queued,
    /// Reading the window from the ring or live.
    Acquiring,
    /// Beam search.
    Searching,
    /// Local refinement (§2.3).
    Refining,
    /// Hold-out validation of the top candidates.
    Validating,
    /// Capture is losing samples; threads halved before capture is hurt.
    Throttled,
    /// Finished.
    Done,
    /// Cancelled; partial results kept.
    Cancelled,
    /// Failed.
    Failed,
}

impl JobState {
    /// Whether the job has finished, one way or another. Only a `Done` job may write `unknown`
    /// (ADR-0021 §7A.4); a cancelled or failed one ruled nothing out.
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            JobState::Done | JobState::Cancelled | JobState::Failed
        )
    }
}

/// What a [`NodeHeuristic`] may look at: a beam node's structure and its seeded prior. Never its
/// measured evidence — a heuristic reorders, it never scores.
#[derive(Clone, Copy, Debug)]
pub struct NodeView<'a> {
    /// `id@version` of the skeleton or template.
    pub skeleton: &'a str,
    /// Slot alternatives fixed so far.
    pub choices: &'a BTreeMap<Stage, String>,
    /// The `hk-mod@1` family the node commits to, if any.
    pub family: Option<&'a str>,
    /// The stage the node would expand next.
    pub next_stage: Stage,
    /// The prior the seeding step gave it (§4.2), bits.
    pub seed_prior_bits: f32,
}

/// The ML slot (ADR-0015 §3.3): an ML value function may replace the classical prior later. It
/// only reorders; it never scores evidence.
pub trait NodeHeuristic {
    /// The node's prior, bits in [−8, 0].
    fn prior_bits(&self, node: &NodeView<'_>) -> f32;
}

/// The classical default: the seeded §4.2 prior, unchanged.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClassicalPrior;

impl NodeHeuristic for ClassicalPrior {
    fn prior_bits(&self, node: &NodeView<'_>) -> f32 {
        node.seed_prior_bits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_carry_the_adr_budgets() {
        assert_eq!(Profile::default(), Profile::Standard);
        let q = Profile::Quick.budget();
        assert_eq!((q.wall_s, q.cpu_s, q.threads), (3.0, 3.0, 1));
        let s = Profile::Standard.budget();
        assert_eq!((s.wall_s, s.cpu_s, s.threads), (20.0, 40.0, 2));
        let d = Profile::Deep.budget();
        assert_eq!((d.wall_s, d.cpu_s, d.threads), (120.0, 400.0, 4));
        assert_eq!(d.max_cache_bytes, DEFAULT_MAX_CACHE_BYTES);
        // T-854: the count caps are the unit (docs/27 §6) and grow with the profile.
        for (a, b) in [(q, s), (s, d)] {
            assert!(a.max_evaluations < b.max_evaluations);
            assert!(a.max_proposal_calls < b.max_proposal_calls);
            assert!(a.max_assist_ops < b.max_assist_ops);
        }
        // `quick` affords docs/27 §4's minimum: two open skeletons × sync + codes + fields.
        assert!(q.max_proposal_calls.unwrap() >= 6);
        // The shared op pool fits under the wall backstop at docs/27 §3's ≈1.5 ns/op.
        for b in [q, s, d] {
            assert!(b.max_assist_ops.unwrap() as f64 * 1.5e-9 <= b.wall_s);
        }
        assert_eq!(serde_json::to_value(Profile::Deep).unwrap(), "deep");
    }

    #[test]
    fn stop_and_state_wire_names() {
        assert_eq!(
            serde_json::to_value(StopReason::SourceEnded).unwrap(),
            "source_ended"
        );
        assert!(JobState::Cancelled.is_terminal());
        assert!(!JobState::Throttled.is_terminal());
    }

    #[test]
    fn the_classical_heuristic_is_the_seeded_prior() {
        let choices = BTreeMap::new();
        let v = NodeView {
            skeleton: "generic-fsk-framed@1",
            choices: &choices,
            family: Some("fsk"),
            next_stage: Stage::S1,
            seed_prior_bits: -1.5,
        };
        assert_eq!(ClassicalPrior.prior_bits(&v), -1.5);
    }
}
