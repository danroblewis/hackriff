//! ISM burst acceptance (T-254): the 902-928 MHz short-burst playground, blind, through the mock
//! SDR and the IQ ring.
//!
//! CLAUDE.md invariant 1 names 902-928 MHz as its canonical playground - rtl_433 sensors, remotes
//! and LoRa, short bursts everywhere - and says ephemeral emissions are first-class: a signal is a
//! time-frequency region, never forced into a "steady emitter parked on one frequency" shape.
//! This suite is that invariant's acceptance test: a bounded time extent, one emitter per burst,
//! and ephemera catalogued as past events rather than left sitting as live candidates. It also
//! re-asks the user's 2026-09-15 field case near 100.3 MHz (B0.497) now that bursts are detected.
//!
//! T-255's `acceptance_chirp` asks the *other* half of the same invariant - no stable frequency -
//! on the same generator, which is why the two are separate suites over one scene.
//!
//! Run with `just acceptance-ism`.

// The shared harness modules carry helpers only the other suites use.
#![allow(dead_code)]

#[path = "acceptance/common.rs"]
mod common;

#[path = "acceptance/blind.rs"]
mod blind;

#[path = "acceptance/t254_ism_bursts.rs"]
mod t254_ism_bursts;
