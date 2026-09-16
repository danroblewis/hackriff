//! M4 (trunking) acceptance suite.
//!
//! Run with `HK_E2E_REQUIRE_SYNTH=1 cargo test -p hk-e2e --test acceptance_m4`.
//!
//! Two halves, and both are needed:
//!
//! - [`t267_trunk_cc`] proves the **capability** blind through the device interface: candidacy
//!   from occupancy on the LMR raster, confirmation from frame sync plus CRC, and the continuous
//!   decoy rejected.
//! - [`t287_trunk_cc_pipeline`] proves the capability has a **caller**: a normal run, with the
//!   built-in chain registry and nothing configured by the test, writes the confirmed
//!   control-channel row. Without it the first half could pass while the pipeline could never
//!   reach the hunt at all.

// The shared harness modules carry helpers only the other suites use.
#![allow(dead_code)]

#[path = "acceptance/common.rs"]
mod common;

#[path = "acceptance/blind.rs"]
mod blind;

#[path = "acceptance/t267_trunk_cc.rs"]
mod t267_trunk_cc;

#[path = "acceptance/t287_trunk_cc_pipeline.rs"]
mod t287_trunk_cc_pipeline;
