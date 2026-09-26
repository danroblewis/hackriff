//! **The "all captured signals decode" milestone gate** (T-936, extended by T-969; `SIGNAL-062`).
//!
//! ```text
//! just acceptance-captured-signals                    # the four green controls
//! just acceptance-captured-signals --run-ignored all   # + the six red proofs
//! ```
//!
//! One blind acceptance test per ticket assertion over **every signal the explorer agent has
//! captured off the air**, replayed through the mock SDR device with no frequency, modulation or
//! protocol passed in. The set grows: a new capture joins by adding its fixture stem to
//! `captured_signals::CAPTURES`, and its `hackriff:truth` emissions are then asserted by every test
//! in the file.
//!
//! This target is a **milestone exit gate**, registered in the justfile's `e2e_milestones` and run
//! by `just acceptance-milestones` — deliberately not in CI's per-merge acceptance gate, for the
//! same reason `acceptance_mauto` and `acceptance_m3` are not: most of it is red *by design* until
//! the tickets it names (T-926, T-937, T-938, T-940) land, and a known-red exit target must not pin
//! every merge red. Its four green controls, which prove the fixture, the explanation path, the
//! decode path and the attachment of a decode to its station are sound, run by default.
//!
//! The whole argument — what each test proves, what each red measured today, and which ticket must
//! delete each `#[ignore]` — is in [`captured_signals`]'s module documentation.

// The shared harness modules carry helpers only the other suites use.
#![allow(dead_code)]

#[path = "acceptance/common.rs"]
mod common;

#[path = "acceptance/blind.rs"]
mod blind;

#[path = "acceptance/captured_signals.rs"]
mod captured_signals;
