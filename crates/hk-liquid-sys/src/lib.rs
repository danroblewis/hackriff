//! Raw FFI over **liquid-dsp 1.8.2** (MIT), vendored and built by `cc` (T-607).
//!
//! ADR-0010 names liquid-dsp as this project's DSP-kernel library, and ADR-0011 §1.6 says a block
//! is an adapter over an existing kernel. Until T-607 nothing linked it (docs/18 §7.1). This crate
//! is that link and nothing more: `unsafe` declarations, no safe wrappers and no block. Safe
//! wrappers belong to the block that uses them (T-609 `psk_demod`), so that each wrapper exists
//! because a caller needs it.
//!
//! # What liquid covers, measured (docs/18 §7.1.1)
//!
//! * **PSK carrier/timing recovery: covered.** `symtrack_cccf` (AGC → polyphase RRC matched
//!   filter + symbol timing → equaliser → NCO/PLL) with `modemcf` de-mapping recovers BPSK, QPSK,
//!   8PSK, D8PSK, π/4-DQPSK and DBPSK with no symbol errors at 22 dB Es/N0 on synthetic RRC signals
//!   with a fractional timing offset, a phase offset and a carrier offset. Coherent pull-in is
//!   bounded, though: BPSK held to 0.02 rad/sample, QPSK to 0.01 and 8PSK to 0.005 (k = 2, loop
//!   bandwidth 0.02). Beyond that the block needs a coarse CFO correction first (see
//!   `tests/coverage.rs`). **OQPSK has no liquid modem.**
//! * **Convolutional (Viterbi) and Reed–Solomon FEC: NOT covered.** In liquid these are wrappers
//!   over Phil Karn's *libfec* (LGPL-2.1), compiled in only when liquid's autotools build finds it.
//!   Its CMake build never looks for it, and this crate does not build it. `fec_create` returns
//!   NULL for every `v27*`/`v29*`/`v39`/`v615`/`rs8` scheme, and `tests/coverage.rs` asserts that
//!   so nobody re-assumes otherwise. Liquid's own FEC is Hamming, Golay, SEC-DED and repetition
//!   only.
//!
//! # Calling convention
//!
//! liquid's objects are opaque pointers (`*mut c_void` here), and its enums are C `int`. Look
//! scheme ids up by name with [`liquid_getopt_str2mod`] and friends, rather than hard-coding
//! enum ordinals that upstream is free to renumber. C99 `float complex` is [`Complex32`] (see the
//! note in `Cargo.toml`); bindgen does not model `_Complex`, which is one reason these
//! declarations are written by hand and checked by calling every one of them in the tests.
//!
//! Errors: functions returning `c_int` return `LIQUID_OK` (0) on success. liquid also logs
//! errors to stderr itself.

#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_int, c_uint, c_void};

pub use num_complex::Complex32;

/// `LIQUID_OK`: the success return code of every `int`-returning liquid function.
pub const LIQUID_OK: c_int = 0;
/// `LIQUID_NCO`: the numerically-controlled oscillator type for [`nco_crcf_create`].
pub const LIQUID_NCO: c_int = 0;

/// Opaque `modemcf` object.
pub type modemcf = *mut c_void;
/// Opaque `symtrack_cccf` object.
pub type symtrack_cccf = *mut c_void;
/// Opaque `nco_crcf` object.
pub type nco_crcf = *mut c_void;
/// Opaque `firinterp_crcf` object.
pub type firinterp_crcf = *mut c_void;
/// Opaque `fec` object.
pub type fec = *mut c_void;

unsafe extern "C" {
    // --- library ---------------------------------------------------------------------------
    /// Version string, e.g. `"1.8.2"`.
    pub fn liquid_libversion() -> *const c_char;
    /// Version number, `major << 16 | minor << 8 | patch` (1.8.2 is `0x01_08_02`).
    pub fn liquid_libversion_number() -> c_int;
    /// Modulation scheme id by name (`"qpsk"`, `"dpsk8"`, `"pi4dqpsk"`…); 0 if unknown.
    pub fn liquid_getopt_str2mod(s: *const c_char) -> c_int;
    /// FEC scheme id by name (`"v27"`, `"rs8"`, `"g2412"`…); 0 if unknown.
    pub fn liquid_getopt_str2fec(s: *const c_char) -> c_int;
    /// FIR prototype id by name (`"rrcos"`, `"rcos"`, `"kaiser"`…); 0 if unknown.
    pub fn liquid_getopt_str2firfilt(s: *const c_char) -> c_int;

    // --- modemcf: linear modem (map / de-map) ------------------------------------------------
    pub fn modemcf_create(scheme: c_int) -> modemcf;
    pub fn modemcf_destroy(q: modemcf) -> c_int;
    pub fn modemcf_reset(q: modemcf) -> c_int;
    /// Bits per symbol.
    pub fn modemcf_get_bps(q: modemcf) -> c_uint;
    pub fn modemcf_modulate(q: modemcf, s: c_uint, y: *mut Complex32) -> c_int;
    pub fn modemcf_demodulate(q: modemcf, x: Complex32, s: *mut c_uint) -> c_int;
    /// Hard symbol plus `bps` soft bits (0..=255, 255 = confident 1).
    pub fn modemcf_demodulate_soft(
        q: modemcf,
        x: Complex32,
        s: *mut c_uint,
        soft_bits: *mut u8,
    ) -> c_int;
    pub fn modemcf_get_demodulator_phase_error(q: modemcf) -> f32;
    pub fn modemcf_get_demodulator_evm(q: modemcf) -> f32;

    // --- symtrack_cccf: AGC + RRC matched filter/timing + EQ + carrier PLL --------------------
    /// `ftype` from [`liquid_getopt_str2firfilt`], `k` samples/symbol (≥ 2), `m` filter delay in
    /// symbols, `beta` excess bandwidth, `ms` from [`liquid_getopt_str2mod`].
    pub fn symtrack_cccf_create(
        ftype: c_int,
        k: c_uint,
        m: c_uint,
        beta: f32,
        ms: c_int,
    ) -> symtrack_cccf;
    pub fn symtrack_cccf_destroy(q: symtrack_cccf) -> c_int;
    pub fn symtrack_cccf_reset(q: symtrack_cccf) -> c_int;
    pub fn symtrack_cccf_set_modscheme(q: symtrack_cccf, ms: c_int) -> c_int;
    pub fn symtrack_cccf_set_bandwidth(q: symtrack_cccf, bw: f32) -> c_int;
    /// Nudge the internal NCO by `dphi` rad/sample (a coarse CFO correction from outside).
    pub fn symtrack_cccf_adjust_frequency(q: symtrack_cccf, dphi: f32) -> c_int;
    pub fn symtrack_cccf_adjust_phase(q: symtrack_cccf, phi: f32) -> c_int;
    pub fn symtrack_cccf_set_eq_cm(q: symtrack_cccf) -> c_int;
    pub fn symtrack_cccf_set_eq_dd(q: symtrack_cccf) -> c_int;
    pub fn symtrack_cccf_set_eq_off(q: symtrack_cccf) -> c_int;
    /// `y` must hold at least `2 * nx` symbols; `*ny` receives how many were written.
    pub fn symtrack_cccf_execute_block(
        q: symtrack_cccf,
        x: *mut Complex32,
        nx: c_uint,
        y: *mut Complex32,
        ny: *mut c_uint,
    ) -> c_int;

    // --- nco_crcf: oscillator + first-order-in-phase PLL ---------------------------------------
    /// `kind` is [`LIQUID_NCO`].
    pub fn nco_crcf_create(kind: c_int) -> nco_crcf;
    pub fn nco_crcf_destroy(q: nco_crcf) -> c_int;
    pub fn nco_crcf_set_frequency(q: nco_crcf, dtheta: f32) -> c_int;
    pub fn nco_crcf_get_frequency(q: nco_crcf) -> f32;
    pub fn nco_crcf_set_phase(q: nco_crcf, phi: f32) -> c_int;
    pub fn nco_crcf_get_phase(q: nco_crcf) -> f32;
    pub fn nco_crcf_step(q: nco_crcf) -> c_int;
    pub fn nco_crcf_pll_set_bandwidth(q: nco_crcf, bw: f32) -> c_int;
    pub fn nco_crcf_pll_step(q: nco_crcf, dphi: f32) -> c_int;
    pub fn nco_crcf_mix_down(q: nco_crcf, x: Complex32, y: *mut Complex32) -> c_int;

    // --- firinterp_crcf: pulse-shaping interpolator (synthesis in tests; TX-side kernels) ------
    pub fn firinterp_crcf_create_prototype(
        ftype: c_int,
        k: c_uint,
        m: c_uint,
        beta: f32,
        dt: f32,
    ) -> firinterp_crcf;
    pub fn firinterp_crcf_destroy(q: firinterp_crcf) -> c_int;
    /// `y` must hold `k * n` samples.
    pub fn firinterp_crcf_execute_block(
        q: firinterp_crcf,
        x: *mut Complex32,
        n: c_uint,
        y: *mut Complex32,
    ) -> c_int;

    // --- fec: declared so the coverage test can prove what is NOT here -------------------------
    /// Returns NULL for any scheme this build cannot provide (all convolutional and Reed–Solomon).
    pub fn fec_create(scheme: c_int, opts: *mut c_void) -> fec;
    pub fn fec_destroy(q: fec) -> c_int;
}

/// Look a liquid scheme id up by its liquid name through one of the `liquid_getopt_str2*`
/// functions; `None` for an unknown name (liquid returns 0, its `*_UNKNOWN`).
pub fn scheme_id(
    lookup: unsafe extern "C" fn(*const c_char) -> c_int,
    name: &str,
) -> Option<c_int> {
    let name = std::ffi::CString::new(name).ok()?;
    // SAFETY: `name` is a valid NUL-terminated string that outlives the call; the lookup functions
    // only read it.
    let id = unsafe { lookup(name.as_ptr()) };
    (id > 0).then_some(id)
}
