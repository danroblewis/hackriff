//! `EvidenceObjective` (ADR-0015 §2.3) — **pre-added, empty; M-7 fills it.**
//!
//! M-7 implements `hk_demod::refine::Objective` over a candidate prefix so T-070's
//! `RefinementLoop`, `Termination` and hysteresis are reused unchanged:
//!
//! - `space()` maps the candidate's continuous free parameters to `ParameterSpace` centre and
//!   bandwidth plus `Tuning.mode` axes (deviation, symbol rate, loop bandwidth) by name;
//! - `evaluate(window, tuning, depth)` runs the prefix and returns `Measurement { quality:
//!   evidence_bits, locked: deepest b_k ≥ floor_k }`;
//! - `EvalDepth::{Acquire, Track, Validate}` map to the short, search and hold-out windows.
//!
//! Recipe `refine.objective` gains `{"evidence": "deepest"}` (recipe `schema_version` 3,
//! ADR-0011 §2.4), so a running synthesized pipeline keeps tuning from the same evidence.
//! The WFM objective stays the specialised S1 evidence for broadcast FM.
