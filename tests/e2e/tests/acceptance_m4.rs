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
//! - [`t268_tsbk`] proves the run reads what the control channel **said**: TSBKs decoded, IDEN_UP
//!   turned into a band plan, a grant's 16-bit channel number resolved to the right frequency —
//!   and a grant naming an identifier that was never announced reported as `unmapped-channel`
//!   rather than resolved, through another identifier's plan, to a plausible wrong frequency.
//! - [`t269_follow`] proves the run **acts** on those grants: a channel inside the dwell window is
//!   followed onto a channelizer output and becomes a call with measured boundaries, and one
//!   outside it is logged as `grant-outside-window` instead of silently dropped. Metadata only —
//!   no audio is attempted, and every call states `unknown` encryption.
//! - [`t270_encryption`] proves the run **checks encryption before any voice path**: a grant
//!   carrying the verified service-options encryption bit produces a call flagged `encrypted`,
//!   while a channel announced only by a grant update — late entry, no header — records `unknown`
//!   and never `clear`. Both refuse a voice path, nothing is decrypted, and `recordings == 0`.
//! - [`t271_dmr`] proves the hunt is **not P25-only**, and that a second protocol's refusal is as
//!   honest as its decode: a DMR Tier III control channel is found blind by its own framing and
//!   named from corroborated CSBKs, its grants are fully decoded, and **none of them resolves to a
//!   frequency** — because DMR announces no channel parameters this build could corroborate — while
//!   real traffic sits exactly where an assumed band plan would have put them. It also asserts the
//!   run **reports** the systems no control-channel decoder can reach at all (Capacity Plus's
//!   moving rest channel, NXDN Type-D's distributed trunking), since otherwise they are
//!   indistinguishable from empty spectrum.
//! - [`t345_nxdn`] proves a **third** protocol, decoded through its real channel coding rather than
//!   a flattened one: an NXDN Type-C outbound RCCH found blind, its CACs descrambled,
//!   deinterleaved, depunctured, Viterbi decoded and CRC checked, its channel assignments fully
//!   read — and **none of them resolving to a frequency**, because the air interface carries a
//!   channel *number* and defines no mapping from one to hertz. Two baited frequencies carry real
//!   emissions and neither is ever reported.

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

#[path = "acceptance/t268_tsbk.rs"]
mod t268_tsbk;

#[path = "acceptance/t269_follow.rs"]
mod t269_follow;

#[path = "acceptance/t270_encryption.rs"]
mod t270_encryption;

#[path = "acceptance/t271_dmr.rs"]
mod t271_dmr;

#[path = "acceptance/t345_nxdn.rs"]
mod t345_nxdn;
