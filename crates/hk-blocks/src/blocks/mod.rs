//! Block implementations, one module per group. Each group owns its directory, its
//! `planned()` descriptors and its `register()` function, so the parallel M1 tasks never edit
//! the same file (ADR-0011 §7):
//!
//! | Module | Task | Blocks |
//! |---|---|---|
//! | [`iq`] | T-086, T-609 | mix, lowpass, resample, fm_demod, am_demod, fsk_demod, msk_demod, ppm_demod, subcarrier; psk_demod (T-609) |
//! | [`symbol`] | T-086, T-608 | clock_recovery, slicer, diff_decode, nrzi, manchester, descramble |
//! | [`framing`] | T-087 | sync_search, deframe, interleave, deinterleave |
//! | [`fec`] | T-087, T-610, T-611 | crc, bch, parity, checksum; viterbi, viterbi_frames (T-610); reed_solomon (T-611) |
//! | [`parse`] | T-089 | fields, text |
//! | [`multi`] | T-093 | follow_hops |
//! | [`util`] | T-085 | identity (contract example) |
//! | [`audio`] | T-866 | squelch, agc, deemphasis, audio_out (ADR-0011 §8.4) |

use crate::Registry;

pub mod audio;
pub mod fec;
pub mod framing;
pub mod iq;
pub mod multi;
pub mod parse;
pub mod symbol;
pub mod util;

/// Registers every implemented block.
pub(crate) fn register_all(r: &mut Registry) {
    iq::register(r);
    symbol::register(r);
    framing::register(r);
    fec::register(r);
    parse::register(r);
    multi::register(r);
    util::register(r);
    audio::register(r);
}
