//! **FLEX paging** (T-950, `SIGNAL-088`): 2- and 4-level FSK at 1600 and 3200 Bd (1600, 3200 and
//! 6400 bit/s), sync-1 and the frame information word, BCH(31,21) with parity, the block
//! interleave, and the block / address / vector / message fields that make a page.
//!
//! FLEX is the dominant 929–932 MHz paging protocol where the explorer listened (three channels,
//! no POCSAG, 2026-09-25), and the app could not decode any of it. This module is written in Rust
//! from the public description of the format; no GPL decoder code is used, so nothing here needs
//! the plugin process boundary (ADR-0010). Which parts of the format were **checked on the air**
//! and which are **unverified** is stated in [`frame`].
//!
//! **Why a native decoder and not a recipe over `hk-blocks`.** The `hk-blocks` `bch` and
//! `mlevel_slicer` blocks decode a stream whose rate and level count a recipe has already fixed;
//! FLEX changes both per frame (the header is always 2-level 1600 Bd, the data follows the frame
//! information word), and this decoder *measures* the data mode by re-slicing each frame under
//! every hypothesis FLEX defines and keeping the one whose codewords check. That needs the
//! codeword check inside the slicing loop, per frame, which a block graph does not express; and
//! `hk-blocks` depends on `hk-demod`, so the check cannot be borrowed from it either. The
//! frame-hunting pipeline chain (`hk-pipeline::chains::frames`) runs it on every narrowband track.
//!
//! - [`bch`] — codewords, in FLEX bit order.
//! - [`frame`] — sync-1, FIW, interleave and page fields.
//! - [`demod`] — baseband to frames; the data rate and level count are **measured** per frame
//!   (the hypothesis whose codewords check), with the header's declaration kept beside them.
//!
//! Nothing here names a frequency, an allocation or a service: it is handed baseband and answers
//! with frames, and whether anything *is* FLEX is decided by sync plus BCH, never by where it was
//! found.

pub mod bch;
pub mod demod;
pub mod frame;

pub use demod::{FlexError, FlexFrame, FlexReport, Hypothesis, decode};
pub use frame::{Fiw, Mode, Page, PageKind, Phase};

/// Decoder id (`Decode.decoder_id`, and the family vocabulary's decoder evidence).
pub const FLEX_DECODER_ID: &str = "flex";
/// Decoder version (`Decode.decoder_version`, `Demodulation.demod_version`).
pub const FLEX_DECODER_VERSION: &str = "hk-demod/flex@0.1.0";
