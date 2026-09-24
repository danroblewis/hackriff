//! Decoder synthesis (ADR-0015, MAUTO): search for the pipeline structure and parameters that best
//! explain a signal, guided by per-stage evidence in bits and ending in confirm-by-decode.
//!
//! # What exists (T-848, MAUTO M-1: the scaffold)
//!
//! **Types, modules and stubs — no engine.** Nothing here searches, scores a window, loads a
//! calibration table or reads IQ. Each module names the MAUTO ticket that fills it:
//!
//! | Module | Contract | Filled by |
//! |---|---|---|
//! | [`stage`] | the S0–S6 ladder, default caps and floors (§1.1, §1.3, §13.2) | — (complete) |
//! | [`evidence`] | `Evidence`, `EvidenceSet`, `NodeScore`, `prior_bits` (§1.3, §2.1, §13.1) | M-2 (per-block evidence), T-660 (group combination) |
//! | [`calibration`] | `hackriff.calibration/1` answers: `Score`, `Threshold`, fill buckets (§13.2–§13.3) | T-660 (loader + generator) |
//! | [`candidate`] | a candidate = recipe prefix + typed free parameters (§1.2) | M-3 |
//! | [`skeleton`] | structure alternatives per stage slot (§1.2) | M-5 (built-in skeletons) |
//! | [`template`] | `hackriff.template/1` (§4.1, §15) | M-5 (loader, validation, built-ins) |
//! | [`seed`] | the adapter over ADR-0016's `SearchSeed` (§4.2) | M-5 (match and band terms) |
//! | [`proposal`] | proposal operators over `hk_estimate::assist` (§3.2) | M-4 |
//! | [`search`] | profiles, budgets, stop reasons, job states, the ML slot (§3.3, §5.2) | M-3 |
//! | [`result`] | `PipelineResult` and the verdict ladder (§3.4) | M-3, M-9 |
//! | [`trace`] | the search trace and the negative result (ADR-0021) | M-3 (producer), M-9 (sealing) |
//! | [`objective`] | `EvidenceObjective` over T-070's `RefinementLoop` (§2.3) | M-7 |
//!
//! # The rules every later ticket inherits
//!
//! - **Blind first.** Templates, priors and the known-signal database only **order** the search;
//!   measured evidence ranks and confirms. `prior_bits` and `evidence_bits` are separate numbers
//!   and [`evidence::NodeScore::rank_key`] cannot see the prior (§1.3).
//! - **A candidate is a recipe.** Every beam node is a valid, runnable `hk_recipe::Recipe`
//!   prefix; the winner is an ordinary recipe (§1.2).
//! - **Undeclared is not independent** (§13.1), **a table never answers a question it cannot
//!   answer** (§13.2), **unknown fill is under-filled** (§13.3): every default fails toward less
//!   evidence, never more.
//! - **Not tried is not ruled out** (ADR-0021 §2.2, §7A.4): the trace keeps the two apart.
//!
//! The evidence vocabulary itself ([`Stage`], [`MetricId`], [`GroupId`], [`Evidence`],
//! [`EvidenceSet`]) is defined in `hk_model::synth` so `hk-blocks` can emit it without depending
//! on this crate, and re-exported here (ADR-0015 §16, delta D1).

pub mod calibration;
pub mod candidate;
pub mod evidence;
pub mod objective;
pub mod proposal;
pub mod result;
pub mod search;
pub mod seed;
pub mod skeleton;
pub mod stage;
pub mod template;
pub mod trace;

pub use calibration::{
    CalibrationTable, CellId, FillBucket, Level, LoadError, NullKind, Score, Threshold,
    Unexpressible, UnexpressibleLevel,
};
pub use candidate::{Candidate, Domain, FreeParam, SeedSource};
pub use evidence::{NodeScore, combine_stage_bits, prior_bits};
pub use hk_model::synth::{
    EVIDENCE_SET_CAPACITY, Evidence, EvidenceSet, EvidenceSetFull, GroupId, MetricId, Stage,
    quality_from_bits,
};
pub use proposal::ProposalOp;
pub use result::{PipelineResult, Verdict};
pub use search::{JobState, NodeHeuristic, NodeView, Profile, StopReason, SynthBudget};
pub use skeleton::{Skeleton, SlotAlternative};
pub use template::Template;
pub use trace::{Outcome, Reason, Resolution, ResolutionKind, TraceNode};

/// Engine name and contract version, as provenance records carry it (`engine: "hk-synth@1"`,
/// ADR-0015 §11.1). Bumped when a scoring rule changes what `evidence_bits` means.
pub const ENGINE: &str = "hk-synth@1";
