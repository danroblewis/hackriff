//! **The "all captured signals decode" milestone gate** (T-936, extended by T-969 and T-976;
//! `SIGNAL-062`, `SIGNAL-085`).
//!
//! ```text
//! just acceptance-captured-signals                    # the seven green controls
//! just acceptance-captured-signals --run-ignored all   # + the nine red proofs
//! ```
//!
//! One blind acceptance test per ticket assertion over **every signal the explorer agent has
//! captured off the air**, replayed through the mock SDR device with no frequency, modulation or
//! protocol passed in. The set grows: a new FM/RDS capture joins by adding its fixture stem to
//! `captured_signals::CAPTURES`, and its `hackriff:truth` emissions are then asserted by every test
//! in that module. A capture of a different air interface asks different questions of the same
//! harness, so it gets its own module in this binary: the P25 member (T-976) is
//! [`captured_p25`], and a later P25 capture joins it by adding a line to its `P25_CAPTURES`.
//!
//! This target is a **milestone exit gate**, registered in the justfile's `e2e_milestones` and run
//! by `just acceptance-milestones` — deliberately not in CI's per-merge acceptance gate, for the
//! same reason `acceptance_mauto` and `acceptance_m3` are not: most of it is red *by design* until
//! the tickets it names (T-926, T-937, T-938, T-940, and the three T-976's hand-back asks for)
//! land, and a known-red exit target must not pin every merge red. Its green controls — the FM
//! half's four, which prove the fixture, the explanation path, the decode path and the attachment
//! of a decode to its station are sound, and the P25 half's three (detection and answer key, one
//! region, a public-safety explanation) — run by default.
//!
//! The whole argument — what each test proves, what each red measured today, and which ticket must
//! delete each `#[ignore]` — is in [`captured_signals`]'s and [`captured_p25`]'s module
//! documentation.

// The shared harness modules carry helpers only the other suites use.
#![allow(dead_code)]

#[path = "acceptance/common.rs"]
mod common;

#[path = "acceptance/blind.rs"]
mod blind;

#[path = "acceptance/captured_signals.rs"]
mod captured_signals;

#[path = "acceptance/captured_p25.rs"]
mod captured_p25;
