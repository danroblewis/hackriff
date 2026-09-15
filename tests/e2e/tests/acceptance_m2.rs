//! T-124: the M2 attention acceptance suite (AWARE-042, AWARE-044, AWARE-027), blind, through the
//! mock SDR. Run with `just acceptance-m2` (kept apart from `just acceptance` for wall time).
//!
//! - `m2_scene`: time-compressed multi-day occupancy scenes driven by the bandit scheduler under
//!   default scheduler settings. One shared 46 h run: FCO vs hidden truth, busier-than-usual alarm
//!   on the injected channel, false alarms, a gain step explained by provenance, and the survey
//!   report's coverage/POI disclosure. Its own runs: a single persistent new emitter on a quiet
//!   9-day site (T-138 rule), and a restart on the same data dir keeping the pinned site (T-136).
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
