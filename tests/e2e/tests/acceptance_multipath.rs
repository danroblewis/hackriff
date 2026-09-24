//! Multipath acceptance (T-222, AWARE-053, C40 content half): one emission arriving over two
//! paths, blind, through the mock SDR.
//!
//! Every relationship rule before this one reasons from geometry - an overlap of bands, mixer
//! arithmetic, a slope against the local oscillator - and none of them can see two rows that
//! overlap nowhere and are nonetheless the same transmission, reaching the antenna twice over
//! paths of different length. What says so is what the rows CARRY.
//!
//! The scene puts the same 2-FSK burst transmission on two channels, the second a delayed and
//! attenuated copy, beside an independent station of the same family, bandwidth and modulation.
//! The pair must collapse to one emission with the lag measured; the decoy must not.
//!
//! Run with `just acceptance-multipath`.

// The shared harness modules carry helpers only the other suites use.
#![allow(dead_code)]

#[path = "acceptance/common.rs"]
mod common;

#[path = "acceptance/blind.rs"]
mod blind;

#[path = "acceptance/t222_multipath.rs"]
mod t222_multipath;
