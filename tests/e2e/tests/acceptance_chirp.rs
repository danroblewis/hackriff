//! Chirp acceptance (T-255): a signal with a time extent and **no stable frequency**.
//!
//! CLAUDE.md invariant 1 makes two claims about every detection — that it has a time extent, and
//! that it needs no carrier and no stable frequency. A LoRa up-chirp is the sharpest available
//! test of the second, so it gets its own suite rather than being buried in a milestone one.
//!
//! Run with `just acceptance-chirp`.

// The shared harness modules carry helpers only the other suites use.
#![allow(dead_code)]

#[path = "acceptance/common.rs"]
mod common;

#[path = "acceptance/blind.rs"]
mod blind;

#[path = "acceptance/t255_lora_chirp.rs"]
mod t255_lora_chirp;

#[path = "acceptance/t297_chirp_characterisation.rs"]
mod t297_chirp_characterisation;
