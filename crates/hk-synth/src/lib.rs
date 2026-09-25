//! Decoder synthesis (ADR-0015, MAUTO): search for the pipeline structure and parameters that best
//! explain a signal, guided by per-stage evidence in bits and ending in confirm-by-decode.
//!
//! # What exists (T-848 = M-1 the scaffold; T-853 = M-2 evidence; T-854 = M-3 the engine; T-858 = M-7 the objective)
//!
//! **The search engine runs (M-3) over real evidence (M-2).** [`engine::search`] runs the staged
//! beam over an [`engine::Evaluator`]; M-2 added per-block evidence (`hk_blocks::Block::evidence`),
//! the batch driver (`hk_blocks::run_window`), the analytic nulls ([`nulls`]), the shipped
//! calibration tables and their scoring ([`score`]), and the noise corpora they are drawn from
//! ([`nullchain`]); M-4's proposal adapters implement the rest of the seam. Each module names the
//! MAUTO ticket that fills it:
//!
//! | Module | Contract | Filled by |
//! |---|---|---|
//! | [`stage`] | the S0–S6 ladder, default caps and floors (§1.1, §1.3, §13.2) | — (complete) |
//! | [`evidence`] | `Evidence`, `EvidenceSet`, `NodeScore`, `prior_bits` (§1.3, §2.1, §13.1) | M-2 (T-853, per-block evidence), T-660 (group combination) — complete |
//! | [`calibration`] | `hackriff.calibration/1` answers: `Score`, `Threshold`, fill buckets (§13.2–§13.3) | T-660 (loader + generator), T-853 (support matching, file groups, fill bounds) |
//! | [`score`] | window → scored evidence → `b_j` ladder; the built-in tables (§1.3, §2.2) | T-853 — complete; `L_j`, floors and pruning are M-3's |
//! | [`nullchain`] | the noise corpora the tables are drawn from and re-checked against (§2.2, §13.3) | T-853 — complete |
//! | [`candidate`] | a candidate = recipe prefix + typed free parameters (§1.2) | — (complete; built by [`engine`]) |
//! | [`skeleton`] | structure alternatives per stage slot (§1.2) | M-5 (built-in skeletons) |
//! | [`template`] | `hackriff.template/1` (§4.1, §15) | M-5 (schema) |
//! | [`library`] | template loader, consistency validation, built-ins, seeding (§4.1, §4.2, §15.6) | M-5 (T-856) |
//! | [`seed`] | the adapter over ADR-0016's `SearchSeed` (§4.2) | M-5 (match and band terms) |
//! | [`proposal`] | proposal operators over `hk_estimate::assist` (§3.2) | M-4 |
//! | [`search`] | profiles, budgets (count caps + wall backstop), stop reasons, job states, the ML slot (§3.3, §5.2) | **M-3 (T-854)** |
//! | [`engine`] | the beam: memoisation, pruning, sweeps, validation, stop rules (§3.1–§3.4) | **M-3 (T-854)** |
//! | [`admission`] | power policy, job admission, auto-analyze, cancel/throttle/thermal inputs (§3.3) | **M-3 (T-854)** |
//! | [`trace_sink`] | the `TraceSink`: incremental retention on insert, elided counts, peak residency, the allocation-measurement scope (ADR-0021 §2.3, §3) | **M-3 (T-854, T-565)** |
//! | [`result`] | `PipelineResult` and the verdict ladder (§3.4) | M-3, M-9 |
//! | [`trace`] | the search trace and the negative result (ADR-0021) | M-3 (producer), M-9 (sealing) |
//! | [`objective`] | `EvidenceObjective` over T-070's `RefinementLoop`; hold-out validation, support alignment (§2.3) | **M-7 (T-858)** |
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

pub mod admission;
pub mod calibration;
pub mod candidate;
pub mod engine;
pub mod evidence;
pub mod library;
pub mod nullchain;
pub mod objective;
pub mod proposal;
pub mod result;
pub mod score;
pub mod search;
pub mod seed;
pub mod skeleton;
pub mod stage;
pub mod template;
pub mod trace;
pub mod trace_sink;

pub use admission::{AutoProfile, Control, Origin, PowerPolicy, Refusal, admit};
pub use calibration::{
    CalibrationTable, CellId, Fill, FillBucket, Level, LoadError, NullKind, Score, Threshold,
    Unexpressible, UnexpressibleLevel,
};
pub use candidate::{Candidate, Domain, FreeParam, SeedSource};
pub use engine::{
    EvalError, EvalRequest, EvalWindow, Evaluated, Evaluator, NodeEvidence, Progress,
    ProposalReply, ProposeRequest, RecipeHead, Root, SearchOutcome, SearchSpec, SolveRule,
    Suggestion, UnsupportedStructure, search,
};
pub use evidence::{NodeScore, combine_stage_bits, prior_bits};
/// The analytic nulls (ADR-0015 §2.2's first list), defined beside the vocabulary so blocks can
/// compute them.
pub use hk_model::synth::null as nulls;
pub use hk_model::synth::{
    EVIDENCE_SET_CAPACITY, Evidence, EvidenceSet, EvidenceSetFull, GroupId, MetricId, Stage,
    quality_from_bits,
};
pub use objective::{
    ChannelSearch, EVIDENCE_OBJECTIVE, EvidenceContext, EvidenceObjective, ObjectiveError,
    ParamAxis, WindowSplit,
};
pub use proposal::ProposalOp;
pub use result::{PipelineResult, Verdict};
pub use score::{CalibrationSet, Scored, StageLadder, WindowEvidence, evaluate_window, score_node};
pub use search::{JobState, NodeHeuristic, NodeView, Profile, StopReason, SynthBudget};
pub use skeleton::{Skeleton, SlotAlternative};
pub use template::{SaveAsTemplate, Template, save_as_template};
pub use trace::{Outcome, Reason, Resolution, ResolutionKind, TraceNode};
pub use trace_sink::{Trace, TraceSink};

/// Engine name and contract version, as provenance records carry it (`engine: "hk-synth@1"`,
/// ADR-0015 §11.1). Bumped when a scoring rule changes what `evidence_bits` means.
pub const ENGINE: &str = "hk-synth@1";
