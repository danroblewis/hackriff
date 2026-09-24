//! Search budgets, profiles, stop reasons, job states and the ML slot (ADR-0015 §3.3, §5.2).
//!
//! **The engine is M-3's**: staged beam search with prefix memoisation, floor and
//! optimistic-bound pruning, the `synth` chain kind below real-time chains, throttling on
//! `lost_samples`, the power policy, and — per ADR-0021 §3 — the `TraceSink` at every
//! frontier-removal site. This module fixes the vocabulary it runs under.
//!
//! The profile numbers are ADR-0015 §3.3's **unverified guesses**, to be measured in M-3 on the
//! Mac and in M-13 on the Jetson (T-552 owns the measurement). docs/20 §U2 recommends (not yet
//! answered by the user) that auto-queued jobs run at `quick` only, on mains only.

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
/// max_cache_bytes, threads }`). A `None` count cap means the profile does not bound it; wall and
/// CPU always do.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SynthBudget {
    /// Wall-clock cap, s.
    pub wall_s: f64,
    /// CPU cap, s.
    pub cpu_s: f64,
    /// Evaluation cap (ADR-0021 §8.4's acceptance jobs set one for determinism).
    pub max_evaluations: Option<u64>,
    /// IQ-sample cap for acquisition.
    pub max_iq_samples: Option<u64>,
    /// Memo-cache cap, bytes.
    pub max_cache_bytes: u64,
    /// Search threads (halved when throttled or on battery).
    pub threads: u32,
}

impl Profile {
    /// The profile's default budget (§3.3 table).
    pub const fn budget(self) -> SynthBudget {
        let (wall_s, cpu_s, threads) = match self {
            Profile::Quick => (3.0, 3.0, 1),
            Profile::Standard => (20.0, 40.0, 2),
            Profile::Deep => (120.0, 400.0, 4),
        };
        SynthBudget {
            wall_s,
            cpu_s,
            max_evaluations: None,
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
