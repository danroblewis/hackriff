//! T-206: the M3 classification acceptance suite — the milestone's **exit gate** (ADR-0016 §7).
//!
//! Run with `just acceptance-m3`. It is kept apart from `just acceptance` and `just acceptance-m2`
//! for wall time: the full evaluation grid is ~1 700 classified snippets, which ADR-0016 §7 calls
//! for before a number is trusted for a gate, and that costs well over a minute in a debug build.
//!
//! Two halves, both required:
//!
//! - [`m3_grid`] measures classification accuracy against the five a-priori floors of ADR-0016 §7,
//!   per family and per SNR bin, over the acceptance seed range the densities were never fitted on.
//!   It also records this gate's **ruling** on which held-out population the unknown-recall and
//!   false-known floors are read over.
//! - [`m3_scene`] drives blind scenes **through the mock SDR device** and asserts that a real run
//!   produces the rows §7 is stated over: a classification, a signature match, and a cluster of
//!   repeated unknowns.
//!
//! **Blind.** Truth lives only in the assertions. No test looks a value up and tunes to it, and
//! nothing seeds the catalogue or the inventory from truth — the catalogue entry that explains an
//! emitter is minted from what the run itself measured.

// The shared harness modules carry helpers only the other suites use.
#![allow(dead_code)]

#[path = "acceptance/common.rs"]
mod common;

#[path = "acceptance/blind.rs"]
mod blind;

#[path = "acceptance/m3_grid.rs"]
mod m3_grid;

#[path = "acceptance/m3_scene.rs"]
mod m3_scene;
