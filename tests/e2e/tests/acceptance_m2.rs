//! T-124: the M2 attention acceptance suite (AWARE-042, AWARE-044, AWARE-027), blind, through the
//! mock SDR. Run with `just acceptance-m2` (kept apart from `just acceptance` for wall time).
//!
//! - `m2_scene`: one time-compressed multi-day occupancy scene, shared by its test functions (one
//!   pipeline run), driven by the bandit scheduler: FCO vs hidden truth, busier-than-usual alarm on
//!   the injected channel, false alarms, a gain step explained by provenance, and the survey
//!   report's coverage/POI disclosure.
//! - `m2_simulator`: the recorded bandit vs round-robin scheduler simulator comparison (hk-sim).

// The shared harness modules carry helpers only the M0 suite uses.
#![allow(dead_code)]

#[path = "acceptance/common.rs"]
mod common;

#[path = "acceptance/blind.rs"]
mod blind;

#[path = "acceptance/m2_scene.rs"]
mod m2_scene;

#[path = "acceptance/m2_simulator.rs"]
mod m2_simulator;
