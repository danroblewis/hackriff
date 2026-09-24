//! `psk_demod` (T-609, ADR-0011 §9): coarse carrier acquisition, matched filter, symbol timing
//! and carrier recovery, and an in-block de-map to **one `soft` item per bit** (§9.2 option a).
//!
//! # What it adapts (ADR-0011 §1.6 / §9.4), and what is new DSP
//!
//! * **BPSK, DBPSK, QPSK, DQPSK, π/4-DQPSK, 8PSK, D8PSK: an adapter.** liquid-dsp's
//!   `symtrack_cccf` (AGC → polyphase RRC matched filter + symbol timing → equaliser → NCO/PLL,
//!   vendored by T-607 in `hk-liquid-sys`) recovers the constellation. The safe wrapper
//!   ([`Symtrack`]) lives here, beside its only caller, as `hk-liquid-sys` asks.
//! * **OQPSK: native DSP** (T-607 measured that liquid has no OQPSK modem). The half-symbol rail
//!   offset means a carrier error mixes rails that are *in transition*, so an offset-blind
//!   tracker cannot lock it. [`Oqpsk`] is the textbook receiver: matched filter (RRC, half-sine
//!   for 802.15.4, or rect), a cubic-Lagrange interpolator strobing at half-symbol spacing, a
//!   Gardner detector on each rail at its own strobes, and a decision-directed Costas detector
//!   that reads I at I-strobes and Q at Q-strobes.
//! * **Carrier acquisition: native DSP.** T-607 measured liquid's unaided coherent pull-in at
//!   0.01 rad/sample for QPSK and 0.005 for 8PSK, about 0.3 % of the symbol rate. [`Coarse`] is
//!   the textbook feed-forward M-th-power estimator: over a fixed window after each restart,
//!   `x^M` strips the modulation to a carrier line, and a Hann-windowed FFT finds it. It
//!   de-rotates ahead of the tracker, clamped to `max_offset_hz`, and the tracker's 2nd-order
//!   PLL holds from there. The ticket asked for an "FLL or band-edge" stage, and both were
//!   built and measured first. A delay-and-multiply (band-edge-class) estimator is still
//!   200–400 Hz out after 256 symbols from data self-noise alone. An FLL closed around the
//!   tracker's symbols wandered 110 Hz against liquid's own NCO integrator on DBPSK. The
//!   spectral-line estimator lands within 1 Hz on every constellation in the tests.
//! * **Rate plan: an adapter.** The input is brought to an integer `k` samples per symbol by
//!   the same DDC stages as `resample` ([`Rate`]). liquid's tracker needs an integer `k ≥ 2`;
//!   OQPSK uses `k ≥ 4` where the input allows it. `k` is chosen so that the passband holds the
//!   signal plus `max_offset_hz` on both sides, and so that the acquisition's M-th-power line
//!   cannot alias (8PSK: `k ≥ 5` at the default range).
//!
//! Every algorithm here is a published textbook one (Costas loop, Gardner TED, Mueller–Müller
//! TED for the diagnostic, RRC, max-log de-mapping). Nothing is derived from GPL decoder source
//! (ADR-0010).
//!
//! # Output (the §9.2 de-mapped `soft` contract)
//!
//! k items per symbol, label **MSB first**, positive = 1. The magnitude is the max-log bit LLR
//! (`min d²(b=0) − min d²(b=1)`, symbols normalised to unit power), a common positive scale. The
//! constellations are the textbook ones: M-PSK point `i` sits at `θ₀ + 2πi/M` and carries label
//! `gray(i) = i ^ (i >> 1)` (or `i` for `mapping: natural`). Differential modes carry the label
//! in the phase *change* `φ₀ + 2πi/M` (`φ₀ = π/4` for π/4-DQPSK, else 0) and are detected
//! softly on `y[n]·conj(y[n−1])`, so they emit data bits. OQPSK is Gray QPSK on offset rails:
//! the MSB is the I rail and the LSB the Q rail, with 1 ↔ negative.
//!
//! **Phase ambiguity** is the hot `rotation_deg` (a multiple of 360/M; differential modes have
//! none and take only 0) plus `iq_swap`, applied to each recovered point before de-mapping.
//! OQPSK is the exception: a 90° carrier slip moves the receiver's I strobes onto the
//! transmitter's Q rail, which is a one-bit slip plus a rail inversion, not a rotation of the
//! pair. So for OQPSK `rotation_deg` selects the inverted rails: 90 inverts I, 180 both, 270 Q.
//! A frame sync absorbs the one-bit slip.
//!
//! # Time map
//!
//! `rate_hz` is the item rate `k_bits × symbol_rate_bd`, and `source_per_item` is the source
//! samples per symbol ÷ k_bits. `symbol_rate_bd`, `bits_per_symbol` and the samples per symbol
//! go out as status extras. The chunk's first item is stamped with its symbol's centre in the
//! source timeline: exactly from the interpolator's strobe position (OQPSK), or from the symbol
//! count and the tracker's group delay (liquid, `LIQUID_DELAY_SYMBOLS`, measured in the tests).
//!
//! # Acquisition, hold and DISCONTINUITY
//!
//! * A `DISCONTINUITY` or `RESET` input, or [`Block::reset`], drops **all** state and the block
//!   re-acquires from scratch. That covers resampler history, carrier estimate, tracker and
//!   equaliser (including the liquid state its own reset misses, see [`Symtrack::reset`]),
//!   amplitude, differential reference and lock. The flag goes out on the same chunk. A carrier
//!   or clock from before a gap is never assumed to hold after it.
//! * **Acquire.** The first `ACQ_SYMBOLS` (×2 for x⁸, and for RRC OQPSK's x⁴) pass un-rotated
//!   while the window fills, and `lock` reads `searching`. The estimate then lands, and the
//!   loops pull in behind it (OQPSK's loops restart there).
//! * **Hold.** The tracker's carrier and timing loops follow drift. `lock` comes from the
//!   constellation EVM, per symbol, with hysteresis (`LOCK_ON` / `LOCK_OFF`, after
//!   `LOCK_MIN_SYMBOLS`). A fade is ridden out, not reset.
//! * **Re-acquire.** After `REACQ_SYMBOLS` (and at least two windows) without lock following an
//!   acquisition, a fresh window is taken while the old estimate stays applied. This covers a
//!   burst that began after the first window, and an estimate taken from noise. The decision
//!   takes effect at a fixed segment boundary (`SEGMENT_SYMBOLS`), so it is chunking-invariant.
//! * Items are emitted throughout. `lock` and `quality` tell a consumer (the refinement loop,
//!   the MAUTO search) whether to believe them.
//! * **Bursts: `burst: true` (T-875).** On this streaming path a packet shorter than the
//!   acquisition window (802.15.4 frames run to about 2 000 symbols against a 512-symbol window)
//!   loses its start. Burst mode ([`burst`]) finds each burst by energy, confirms and measures it
//!   feed-forward over its own samples (carrier frequency and phase, symbol timing, amplitude)
//!   and tracks it from its first sample, marking each burst's first item `DISCONTINUITY`
//!   (§9.2). Measured, the shortest burst decoded whole went from none up to 2 048 symbols
//!   (DBPSK aside) to 8 symbols (RRC OQPSK: 64); the table is in [`burst`]'s docs.

use std::f64::consts::{PI, TAU};
use std::ffi::{c_int, c_void};
use std::ptr::NonNull;

use hk_demod::dsp::FirDecimator;
use hk_dsp::{CpuFft, FftBackend};
use hk_liquid_sys as lq;
use hk_recipe::{Params, PortType};
use num_complex::{Complex32, Complex64};

use super::common::*;
use super::filter::Rate;
use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::buffer::ChunkFlags;
use crate::registry::BuildCtx;
use crate::status::{Lock, Status};

#[path = "psk_burst.rs"]
mod burst;

/// Parameters applied in place (the descriptor's `hot` keys).
const HOT: &[&str] = &["rotation_deg", "iq_swap", "loop_bandwidth"];

/// Default `loop_bandwidth` (liquid symtrack's scale). T-607 measured the family at 0.02, whose
/// carrier loop (α = 2·10⁻⁵ per symbol) holds but re-locks 8PSK slowly enough to slip after a
/// large offset is removed; 0.2 held every constellation in the tests without a slip.
const DEFAULT_LOOP_BANDWIDTH: f64 = 0.2;
/// liquid tracker filter semi-length, symbols (T-607 measured the family at 7).
const TRACK_M: u32 = 7;
/// liquid tracker group delay, symbols: the RRC half-length plus the equaliser's 9 taps at 2
/// samples/symbol (2 symbols). Measured by `time_map_stamps_each_symbol_at_its_source_centre`
/// to within 0.15 symbol at 3 and 5 samples/symbol.
const LIQUID_DELAY_SYMBOLS: f64 = TRACK_M as f64 + 2.0;
/// OQPSK RRC matched filter half-length, symbols.
const OQPSK_SPAN: f64 = 6.0;
/// Symbols in the carrier acquisition window (rounded up to a power-of-two sample count).
const ACQ_SYMBOLS: f64 = 512.0;
/// Tracker segment, symbols: decisions that feed back into the carrier stage (a
/// re-acquisition) take effect only at these fixed sample counts since restart, never at a
/// chunk boundary, so the output is chunking-invariant.
const SEGMENT_SYMBOLS: f64 = 8.0;
/// Symbols without lock, after an acquisition, before the carrier is re-acquired (at least
/// this, and at least two acquisition windows).
const REACQ_SYMBOLS: u64 = 1024;
/// Amplitude and EVM averaging, symbols.
const EVM_TAU_SYMBOLS: f64 = 64.0;
/// Symbols before `lock` may read locked.
const LOCK_MIN_SYMBOLS: u64 = 64;
/// Quality at which the lock detector locks, and below which it unlocks.
const LOCK_ON: f64 = 0.6;
const LOCK_OFF: f64 = 0.4;
/// Passband margin of the rate plan over the occupied bandwidth.
const RATE_MARGIN: f64 = 1.25;
/// Largest native timing-loop rate deviation (fraction of the nominal period).
const MAX_TIMING_DEV: f64 = 0.05;

// -------------------------------------------------------------------------------- modulation

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Modulation {
    Bpsk,
    Dbpsk,
    Qpsk,
    Oqpsk,
    Dqpsk,
    Pi4Dqpsk,
    Psk8,
    D8psk,
}

impl Modulation {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "bpsk" => Self::Bpsk,
            "dbpsk" => Self::Dbpsk,
            "qpsk" => Self::Qpsk,
            "oqpsk" => Self::Oqpsk,
            "dqpsk" => Self::Dqpsk,
            "pi4-dqpsk" => Self::Pi4Dqpsk,
            "8psk" => Self::Psk8,
            "d8psk" => Self::D8psk,
            _ => return None,
        })
    }

    /// Bits per symbol.
    fn bits(self) -> usize {
        match self {
            Self::Bpsk | Self::Dbpsk => 1,
            Self::Qpsk | Self::Oqpsk | Self::Dqpsk | Self::Pi4Dqpsk => 2,
            Self::Psk8 | Self::D8psk => 3,
        }
    }

    fn differential(self) -> bool {
        matches!(
            self,
            Self::Dbpsk | Self::Dqpsk | Self::Pi4Dqpsk | Self::D8psk
        )
    }

    /// liquid's modem for the tracker's phase detector; `None`: native path.
    fn liquid(self) -> Option<&'static str> {
        match self {
            Self::Bpsk => Some("bpsk"),
            Self::Dbpsk => Some("dpsk2"),
            Self::Qpsk => Some("qpsk"),
            Self::Oqpsk => None,
            Self::Dqpsk => Some("dpsk4"),
            Self::Pi4Dqpsk => Some("pi4dqpsk"),
            Self::Psk8 => Some("psk8"),
            Self::D8psk => Some("dpsk8"),
        }
    }
}

// ----------------------------------------------------------------------------- constellation

/// An M-PSK constellation on the unit circle with its bit labels, de-mapped max-log.
#[derive(Clone, Copy, Debug)]
struct Constellation {
    points: [Complex64; 8],
    labels: [u8; 8],
    m: usize,
    bits: usize,
    /// Half the minimum distance between points (the decision radius).
    radius: f64,
}

impl Constellation {
    fn psk(m: usize, theta0: f64, gray: bool) -> Self {
        let mut points = [Complex64::new(0.0, 0.0); 8];
        let mut labels = [0u8; 8];
        for i in 0..m {
            points[i] = Complex64::from_polar(1.0, theta0 + TAU * i as f64 / m as f64);
            labels[i] = if gray { (i ^ (i >> 1)) as u8 } else { i as u8 };
        }
        Self {
            points,
            labels,
            m,
            bits: m.trailing_zeros() as usize,
            radius: (PI / m as f64).sin(),
        }
    }

    /// Squared distance to the nearest point.
    fn nearest(&self, y: Complex64) -> (f64, Complex64) {
        let mut best = (f64::INFINITY, self.points[0]);
        for &p in &self.points[..self.m] {
            let d = (y - p).norm_sqr();
            if d < best.0 {
                best = (d, p);
            }
        }
        best
    }

    /// Appends `bits` max-log soft values, MSB first, positive = 1.
    fn demap(&self, y: Complex64, out: &mut Vec<f32>) {
        let mut d = [0.0f64; 8];
        for (i, &p) in self.points[..self.m].iter().enumerate() {
            d[i] = (y - p).norm_sqr();
        }
        for j in (0..self.bits).rev() {
            let (mut d0, mut d1) = (f64::INFINITY, f64::INFINITY);
            for (&label, &dist) in self.labels[..self.m].iter().zip(&d[..self.m]) {
                if label >> j & 1 == 1 {
                    d1 = d1.min(dist);
                } else {
                    d0 = d0.min(dist);
                }
            }
            out.push((d0 - d1) as f32);
        }
    }
}

// --------------------------------------------------------------------------- coarse carrier

/// Carrier acquisition ahead of the tracker, in two parts:
///
/// * **Acquire** (the textbook M-th-power estimator): over the first `acq_len` samples after a
///   restart, `z = x^M / |x|^{M−1}` strips an M-PSK modulation to a carrier line at `M·Δf`.
///   A Hann-windowed FFT and a parabolic peak fit locate it. The line carries far more energy
///   than one bin of the data's self-noise, which is what an unwindowed delay-and-multiply
///   average cannot beat: measured, that estimator is still 200–400 Hz out after 256 symbols.
///   OQPSK's offset rails leave no line in `x⁴`. Instead its square carries a pair of lines at
///   `2Δf ± Rs`, and the search scores that pair. The rate plan keeps every line inside the
///   Nyquist band (`max_offset_hz` is clamped to what the rate can hold unambiguously), so the
///   estimate needs no unwrapping. Until the estimate lands the signal passes un-rotated, and
///   `lock` reads `searching`.
/// * **Hold**: after that the estimate is fixed and the tracker's second-order carrier loop
///   follows drift (liquid's NCO/PLL; OQPSK's Costas loop has its own frequency integrator).
///   A frequency-locked loop around the tracker's symbols was built and measured first: it
///   wandered against liquid's NCO integrator (two integrators on one error; 110 Hz on DBPSK),
///   so it is not here.
/// * **Re-acquire**: if lock is not reached, or is lost, for `REACQ_SYMBOLS` after an
///   acquisition (a burst that began after the window, a fade, an estimate taken from noise),
///   [`Coarse::reacquire`] takes a fresh window while the old estimate stays applied.
struct Coarse {
    /// Largest offset applied, rad/sample (0: off).
    max_w: f64,
    power: i32,
    /// OQPSK: half the spacing of the line pair, in FFT bins (0: a single line).
    side_bins: usize,
    acq: Vec<Complex32>,
    acq_len: usize,
    fft: Option<CpuFft>,
    /// The acquired offset, rad/sample.
    w_acq: f64,
    acquired: bool,
    /// Set once when the acquisition lands; the block takes it to reset its loops.
    just_acquired: bool,
    phase: f64,
}

impl Coarse {
    fn off() -> Self {
        Self::new(0.0, 1, 0, 0)
    }

    fn new(max_w: f64, power: i32, side_bins: usize, acq_len: usize) -> Self {
        let on = max_w > 0.0 && acq_len > 0;
        Self {
            max_w: if on { max_w } else { 0.0 },
            power,
            side_bins,
            acq: Vec::with_capacity(if on { acq_len } else { 0 }),
            acq_len,
            fft: on.then(|| CpuFft::new(acq_len)),
            w_acq: 0.0,
            acquired: false,
            just_acquired: false,
            phase: 0.0,
        }
    }

    fn restart(&mut self) {
        self.acq.clear();
        self.w_acq = 0.0;
        self.acquired = false;
        self.just_acquired = false;
        self.phase = 0.0;
    }

    /// Takes a fresh acquisition window; the current estimate stays applied meanwhile.
    fn reacquire(&mut self) {
        if self.max_w > 0.0 {
            self.acq.clear();
            self.acquired = false;
        }
    }

    /// The offset being removed, rad/sample.
    fn w(&self) -> f64 {
        self.w_acq
    }

    /// Whether the acquisition just landed (cleared by reading).
    fn take_acquired(&mut self) -> bool {
        std::mem::take(&mut self.just_acquired)
    }

    #[inline]
    fn step(&mut self, x: Complex32) -> Complex32 {
        if self.max_w <= 0.0 {
            return x;
        }
        if !self.acquired {
            self.acq.push(x);
            if self.acq.len() == self.acq_len {
                self.acquire();
            }
        }
        let (s, c) = (-self.phase).sin_cos();
        let x64 = Complex64::new(f64::from(x.re), f64::from(x.im));
        let y = x64 * Complex64::new(c, s);
        self.phase = (self.phase + self.w()).rem_euclid(TAU);
        Complex32::new(y.re as f32, y.im as f32)
    }

    /// The M-th-power line search over the stored window (in place: `acq` becomes the
    /// spectrum, then its power).
    fn acquire(&mut self) {
        let n = self.acq_len;
        let m = self.power;
        for (i, v) in self.acq.iter_mut().enumerate() {
            let x = Complex64::new(f64::from(v.re), f64::from(v.im));
            let mag = x.norm();
            let z = if mag > 0.0 {
                x.powi(m) / mag.powi(m - 1)
            } else {
                Complex64::new(0.0, 0.0)
            };
            let hann = 0.5 - 0.5 * (TAU * i as f64 / n as f64).cos();
            *v = Complex32::new((z.re * hann) as f32, (z.im * hann) as f32);
        }
        if let Some(fft) = &mut self.fft {
            fft.forward(&mut self.acq);
        }
        let power = |b: isize| {
            let i = b.rem_euclid(n as isize) as usize;
            f64::from(self.acq[i].norm_sqr())
        };
        let d = self.side_bins as isize;
        let score = |b: isize| {
            if d == 0 {
                power(b)
            } else {
                power(b - d) + power(b + d)
            }
        };
        // Only lines an offset within ±max_w can produce.
        let reach = ((f64::from(m) * self.max_w / TAU * n as f64).ceil() as isize)
            .min(n as isize / 2 - 1 - d);
        let mut best = (f64::NEG_INFINITY, 0isize);
        for b in -reach..=reach {
            let s = score(b);
            if s > best.0 {
                best = (s, b);
            }
        }
        let b = best.1;
        let (l, c, r) = (score(b - 1), score(b), score(b + 1));
        let den = l - 2.0 * c + r;
        let frac = if den.abs() > 0.0 {
            (0.5 * (l - r) / den).clamp(-0.5, 0.5)
        } else {
            0.0
        };
        let line = TAU * (b as f64 + frac) / n as f64;
        self.w_acq = (line / f64::from(m)).clamp(-self.max_w, self.max_w);
        self.acquired = true;
        self.just_acquired = true;
    }
}

// ----------------------------------------------------------------------------- liquid tracker

/// Owns one liquid `symtrack_cccf`.
struct Symtrack(NonNull<c_void>);

// SAFETY: a symtrack object is plain heap memory with no thread affinity; the block that owns
// it is `Send` but not `Sync`, so it is only ever used from one thread at a time.
unsafe impl Send for Symtrack {}

impl Symtrack {
    fn new(k: u32, beta: f32, scheme: c_int, bw: f32) -> Result<Self, BlockError> {
        let rrc = lq::scheme_id(lq::liquid_getopt_str2firfilt, "rrcos")
            .ok_or_else(|| BlockError::Unrealisable("liquid has no RRC prototype".into()))?;
        // SAFETY: arguments are in range (k ≥ 2, m > 0, 0 < beta ≤ 1, a valid scheme id);
        // liquid returns NULL on a configuration error, which is checked.
        let q = unsafe { lq::symtrack_cccf_create(rrc, k, TRACK_M, beta, scheme) };
        let q = NonNull::new(q)
            .ok_or_else(|| BlockError::Unrealisable("liquid refused the tracker".into()))?;
        let t = Self(q);
        t.set_bandwidth(bw);
        Ok(t)
    }

    fn set_bandwidth(&self, bw: f32) {
        // SAFETY: `self.0` is a live symtrack object.
        unsafe { lq::symtrack_cccf_set_bandwidth(self.0.as_ptr(), bw) };
    }

    /// A full reset, as good as a freshly created tracker. liquid 1.8.2's `symsync_reset`
    /// clears the matched filter's history but **not** the derivative matched filter's
    /// (`symsync.proto.c`: only `FIRPFB(_reset)(_q->mf)`), so `symtrack_cccf_reset` alone
    /// leaks the last samples' timing-error history into the next stream. Pushing zeros through
    /// flushes that history, and a second reset then clears everything the zeros touched.
    /// `zeros` must hold at least the filter span; `scratch` at least twice `zeros`. Neither
    /// allocates.
    fn reset(&mut self, zeros: &mut [Complex32], scratch: &mut [Complex32]) {
        // SAFETY: `self.0` is a live symtrack object.
        unsafe { lq::symtrack_cccf_reset(self.0.as_ptr()) };
        zeros.fill(Complex32::new(0.0, 0.0));
        self.execute(zeros, scratch);
        // SAFETY: as above.
        unsafe { lq::symtrack_cccf_reset(self.0.as_ptr()) };
    }

    /// Tracks `x` (modified in place by nothing but liquid's signature), writing symbols to
    /// `y`, which must hold at least `2 · x.len()`. Returns the count written.
    fn execute(&mut self, x: &mut [Complex32], y: &mut [Complex32]) -> usize {
        debug_assert!(y.len() >= 2 * x.len());
        let mut ny = 0u32;
        // SAFETY: `self.0` is live; `x` holds `x.len()` samples and `y` at least twice that,
        // which is liquid's documented output bound for `execute_block`.
        unsafe {
            lq::symtrack_cccf_execute_block(
                self.0.as_ptr(),
                x.as_mut_ptr(),
                x.len() as u32,
                y.as_mut_ptr(),
                &mut ny,
            )
        };
        (ny as usize).min(y.len())
    }
}

impl Drop for Symtrack {
    fn drop(&mut self) {
        // SAFETY: created by `symtrack_cccf_create`, destroyed exactly once.
        unsafe { lq::symtrack_cccf_destroy(self.0.as_ptr()) };
    }
}

/// Angle of liquid's point for symbol 0 of `scheme`, modulo 2π/`m`: where the tracker's phase
/// detector puts the constellation once locked.
fn liquid_theta0(scheme: c_int, m: usize) -> Result<f64, BlockError> {
    // SAFETY: a valid scheme id; the modem is destroyed before returning.
    unsafe {
        let q = lq::modemcf_create(scheme);
        if q.is_null() {
            return Err(BlockError::Unrealisable("liquid refused the modem".into()));
        }
        let mut x = Complex32::new(0.0, 0.0);
        lq::modemcf_modulate(q, 0, &mut x);
        lq::modemcf_destroy(q);
        Ok(f64::from(x.arg()).rem_euclid(TAU / m as f64))
    }
}

// ------------------------------------------------------------------------------ OQPSK tracker

/// Root-raised-cosine impulse response at `t` symbols.
fn rrc(t: f64, a: f64) -> f64 {
    if t.abs() < 1e-9 {
        return 1.0 - a + 4.0 * a / PI;
    }
    if (t.abs() - 1.0 / (4.0 * a)).abs() < 1e-9 {
        let x = PI / (4.0 * a);
        return a / 2f64.sqrt() * ((1.0 + 2.0 / PI) * x.sin() + (1.0 - 2.0 / PI) * x.cos());
    }
    let num = (PI * t * (1.0 - a)).sin() + 4.0 * a * t * (PI * t * (1.0 + a)).cos();
    let den = PI * t * (1.0 - (4.0 * a * t).powi(2));
    num / den
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pulse {
    Rrc,
    Rect,
    HalfSine,
}

/// Matched-filter taps for `pulse` at `k` samples per symbol.
fn matched_taps(pulse: Pulse, k: f64, rolloff: f64) -> Vec<f32> {
    let raw: Vec<f64> = match pulse {
        Pulse::Rrc => {
            let len = ((2.0 * OQPSK_SPAN * k).round() as usize) | 1;
            let mid = (len - 1) as f64 / 2.0;
            (0..len)
                .map(|i| rrc((i as f64 - mid) / k, rolloff))
                .collect()
        }
        Pulse::Rect => vec![1.0; k.round().max(1.0) as usize],
        Pulse::HalfSine => {
            let len = k.round().max(1.0) as usize;
            (0..len)
                .map(|i| (PI * (i as f64 + 0.5) / len as f64).sin())
                .collect()
        }
    };
    let sum: f64 = raw.iter().map(|v| v.abs()).sum();
    raw.iter().map(|v| (v / sum) as f32).collect()
}

/// One OQPSK symbol: the I-rail and Q-rail strobes, and the timing error at it.
#[derive(Clone, Copy, Debug)]
struct OqSymbol {
    point: Complex64,
    timing_error: f64,
    /// Where its I strobe fell, in resampled samples since restart (the input position, the
    /// matched filter's delay removed).
    at: f64,
}

/// Native OQPSK receiver: matched filter → carrier de-rotation → half-symbol interpolator with
/// per-rail Gardner timing and an offset-aware decision-directed Costas loop.
struct Oqpsk {
    k: f64,
    mf: FirDecimator<Complex32>,
    mf_delay: f64,
    /// Matched-filter outputs, de-rotated; `hist[0]` is sample `base`.
    hist: Vec<Complex64>,
    base: u64,
    /// Samples pushed since restart.
    pushed: u64,
    /// Position of the next half-symbol strobe, in matched-filter samples since restart.
    next: f64,
    /// The next strobe is an I strobe.
    on_i: bool,
    /// Timing period correction, samples per half-symbol.
    tf: f64,
    /// Carrier phase and frequency (rad, rad/sample).
    ph: f64,
    w: f64,
    last_i: Complex64,
    last_q: Complex64,
    cur_i: Complex64,
    cur_i_at: f64,
    rail_amp: f64,
    amp_alpha: f64,
    kp: f64,
    ki: f64,
    kpt: f64,
    kit: f64,
    started: bool,
}

impl Oqpsk {
    fn new(k: f64, pulse: Pulse, rolloff: f64, bw: f64, max_in: usize) -> Self {
        let taps = matched_taps(pulse, k, rolloff);
        let len = taps.len();
        let mut o = Self {
            k,
            mf: FirDecimator::new(taps, 1),
            mf_delay: (len as f64 - 1.0) / 2.0,
            hist: Vec::with_capacity(max_in + 512),
            base: 0,
            pushed: 0,
            next: 0.0,
            on_i: true,
            tf: 0.0,
            ph: 0.0,
            w: 0.0,
            last_i: Complex64::new(0.0, 0.0),
            last_q: Complex64::new(0.0, 0.0),
            cur_i: Complex64::new(0.0, 0.0),
            cur_i_at: 0.0,
            rail_amp: 0.0,
            amp_alpha: 1.0 / EVM_TAU_SYMBOLS,
            kp: 0.0,
            ki: 0.0,
            kpt: 0.0,
            kit: 0.0,
            started: false,
        };
        o.set_bandwidth(bw);
        o.restart();
        o
    }

    /// Loop gains from the liquid-scale bandwidth, per symbol. The carrier loop follows
    /// liquid's NCO PLL law (`α = 0.001·bw`, `β = √α`), so one `loop_bandwidth` means the same
    /// loop on both paths. The timing loop runs about ten times faster than the carrier loop.
    fn set_bandwidth(&mut self, bw: f64) {
        let a = 0.001 * bw;
        self.ki = a;
        self.kp = a.sqrt();
        self.kpt = 10.0 * a.sqrt();
        self.kit = self.kpt * self.kpt / 8.0;
    }

    fn restart(&mut self) {
        self.mf.clear();
        self.hist.clear();
        self.base = 0;
        self.pushed = 0;
        self.next = self.mf_delay * 2.0 + 2.0;
        self.on_i = true;
        self.tf = 0.0;
        self.ph = 0.0;
        self.w = 0.0;
        self.last_i = Complex64::new(0.0, 0.0);
        self.last_q = Complex64::new(0.0, 0.0);
        self.cur_i = Complex64::new(0.0, 0.0);
        self.cur_i_at = 0.0;
        self.rail_amp = 0.0;
        self.started = false;
    }

    /// Drops the loop state (carrier phase and frequency, timing rate, amplitude) but keeps
    /// the stream position, so the time map runs on. Used when the carrier acquisition lands:
    /// whatever the loops chased before that was an uncorrected offset, not the signal.
    fn reset_loops(&mut self) {
        self.tf = 0.0;
        self.ph = 0.0;
        self.w = 0.0;
        self.rail_amp = 0.0;
        self.started = false;
    }

    /// Carrier frequency tracked by the loop, rad/sample.
    fn frequency(&self) -> f64 {
        self.w
    }

    /// Position of the next symbol (I strobe), in resampled samples since restart.
    fn next_symbol_at(&self) -> f64 {
        let n = if self.on_i { self.next } else { self.cur_i_at };
        n - self.mf_delay
    }

    fn interpolate(&self, t: f64) -> Complex64 {
        // Cubic Lagrange through samples i−1, i, i+1, i+2 around t = i + mu.
        let i = t.floor();
        let mu = t - i;
        let j = (i as u64 - self.base) as usize;
        let (a, b, c, d) = (
            self.hist[j - 1],
            self.hist[j],
            self.hist[j + 1],
            self.hist[j + 2],
        );
        let c0 = -mu * (mu - 1.0) * (mu - 2.0) / 6.0;
        let c1 = (mu + 1.0) * (mu - 1.0) * (mu - 2.0) / 2.0;
        let c2 = -(mu + 1.0) * mu * (mu - 2.0) / 2.0;
        let c3 = (mu + 1.0) * mu * (mu - 1.0) / 6.0;
        a * c0 + b * c1 + c * c2 + d * c3
    }

    /// Pushes one sample; calls `emit` for each completed symbol.
    fn push(&mut self, x: Complex32, mut emit: impl FnMut(OqSymbol)) {
        let Some(y) = self.mf.push(x) else { return };
        let (s, c) = (-self.ph).sin_cos();
        let y = Complex64::new(f64::from(y.re), f64::from(y.im)) * Complex64::new(c, s);
        self.ph = (self.ph + self.w).rem_euclid(TAU);
        self.hist.push(y);
        self.pushed += 1;
        // Strobe while the four interpolation points are in the history.
        while self.next + 2.0 < self.pushed as f64 {
            let z = self.interpolate(self.next);
            let at = self.next;
            let half = self.k / 2.0 + self.tf;
            if self.on_i {
                self.cur_i = z;
                self.cur_i_at = at;
                self.on_i = false;
            } else {
                // A symbol is complete: I strobe `cur_i`, Q strobe `z`.
                let (zi, zq) = (self.cur_i, z);
                let a = 0.5 * (zi.re.abs() + zq.im.abs());
                self.rail_amp +=
                    (a - self.rail_amp) * if self.started { self.amp_alpha } else { 1.0 };
                let amp = self.rail_amp.max(1e-12);
                let mut te = 0.0;
                if self.started {
                    // Gardner on each rail: I's midpoint is the previous Q strobe, and Q's is
                    // the current I strobe. Positive: sample later.
                    let e_i = self.last_q.re * (self.last_i.re - zi.re);
                    let e_q = zi.im * (self.last_q.im - zq.im);
                    te = ((e_i + e_q) / (2.0 * amp * amp)).clamp(-1.0, 1.0);
                    // Costas, reading each rail at its own strobe.
                    let ec = (zi.re.signum() * zi.im - zq.im.signum() * zq.re) / (2.0 * amp);
                    let ec = ec.clamp(-1.0, 1.0);
                    self.ph = (self.ph + self.kp * ec).rem_euclid(TAU);
                    self.w += self.ki * ec / self.k;
                    self.tf = (self.tf + self.kit * te).clamp(
                        -MAX_TIMING_DEV * self.k / 2.0,
                        MAX_TIMING_DEV * self.k / 2.0,
                    );
                    self.next += self.kpt * te;
                }
                self.started = true;
                self.last_i = zi;
                self.last_q = zq;
                self.on_i = true;
                emit(OqSymbol {
                    point: Complex64::new(zi.re, zq.im) / amp,
                    timing_error: te,
                    at: self.cur_i_at - self.mf_delay,
                });
            }
            self.next += half;
        }
        // Keep what the next interpolation can reach.
        let keep_from = (self.next.floor() as u64).saturating_sub(2);
        if keep_from > self.base + 256 {
            let drop = ((keep_from - self.base) as usize).min(self.hist.len());
            self.hist.drain(..drop);
            self.base += drop as u64;
        }
    }
}

// --------------------------------------------------------------------------------- the block

enum Tracker {
    Liquid(Symtrack),
    Oqpsk(Box<Oqpsk>),
}

pub(crate) fn build(p: &Params, _: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    let modulation = Modulation::parse(str_or(p, "modulation", ""))
        .ok_or_else(|| BlockError::Params("modulation is required".into()))?;
    let symbol_rate = require_f64(p, "symbol_rate_bd")?;
    let pulse = match str_or(p, "pulse", "rrc") {
        "rect" => Pulse::Rect,
        "half-sine" => Pulse::HalfSine,
        _ => Pulse::Rrc,
    };
    if modulation == Modulation::Oqpsk && str_or(p, "mapping", "gray") != "gray" {
        return Err(BlockError::Params(
            "OQPSK rails are independent: its mapping is Gray by construction".into(),
        ));
    }
    if pulse != Pulse::Rrc && modulation != Modulation::Oqpsk {
        return Err(BlockError::Params(
            "rect and half-sine pulses are OQPSK-only; liquid's tracker matches an RRC".into(),
        ));
    }
    let rolloff = f64_or(p, "rolloff", 0.35);
    let mut b = Psk {
        params: p.clone(),
        modulation,
        symbol_rate,
        pulse,
        rolloff,
        gray: str_or(p, "mapping", "gray") == "gray",
        rotation: 0.0,
        rail_flip: (false, false),
        iq_swap: false,
        bw: 0.0,
        max_offset_hz: get_f64(p, "max_offset_hz").unwrap_or(symbol_rate / 4.0),
        k: 0.0,
        rate: None,
        fs_res: 0.0,
        coarse: Coarse::off(),
        segment: 1,
        unlocked_run: 0,
        reacq_after: REACQ_SYMBOLS,
        reacq_pending: false,
        tracker: None,
        table: Constellation::psk(2, 0.0, true),
        track_table: Constellation::psk(2, 0.0, true),
        res: Vec::new(),
        syms: Vec::new(),
        zeros: Vec::new(),
        oq_out: Vec::new(),
        prev_raw: None,
        prev_decision: None,
        power: 0.0,
        evm: 0.0,
        symbols: 0,
        locked: false,
        origin: 0.0,
        need_origin: true,
        pending_restart: false,
        res_count: 0,
        non_finite: 0,
        status: Status::default(),
        burst_mode: bool_or(p, "burst", false),
        burst: None,
        lock_min: LOCK_MIN_SYMBOLS,
    };
    b.set_hot(p)?;
    Ok(Box::new(b))
}

/// PSK demodulator; see the module docs.
struct Psk {
    params: Params,
    modulation: Modulation,
    symbol_rate: f64,
    pulse: Pulse,
    rolloff: f64,
    gray: bool,
    /// De-map rotation, rad (coherent modes).
    rotation: f64,
    /// OQPSK rail inversions (I, Q).
    rail_flip: (bool, bool),
    iq_swap: bool,
    bw: f64,
    max_offset_hz: f64,
    /// Samples per symbol at the tracker.
    k: f64,
    rate: Option<Rate>,
    fs_res: f64,
    coarse: Coarse,
    /// Tracker segment, resampled samples (see `SEGMENT_SYMBOLS`).
    segment: usize,
    /// Symbols since an acquisition without lock; the re-acquisition threshold; and whether
    /// one is due at the next segment boundary.
    unlocked_run: u64,
    reacq_after: u64,
    reacq_pending: bool,
    tracker: Option<Tracker>,
    /// The de-map table: absolute (coherent) or phase-change (differential).
    table: Constellation,
    /// Where the tracker puts the absolute constellation (EVM of coherent modes and the
    /// timing-error diagnostic).
    track_table: Constellation,
    res: Vec<Complex32>,
    syms: Vec<Complex32>,
    /// Zeros that flush liquid's tracker on a reset (see [`Symtrack::reset`]).
    zeros: Vec<Complex32>,
    oq_out: Vec<OqSymbol>,
    /// Previous normalised symbol (differential reference; M&M TED).
    prev_raw: Option<Complex64>,
    prev_decision: Option<Complex64>,
    power: f64,
    evm: f64,
    symbols: u64,
    locked: bool,
    /// Source index of resampled sample 0 of the current segment.
    origin: f64,
    need_origin: bool,
    pending_restart: bool,
    /// Resampled samples since restart.
    res_count: u64,
    non_finite: u64,
    status: Status,
    /// `burst: true`: [`burst`] mode, and its state once initialised.
    burst_mode: bool,
    burst: Option<Box<burst::Burst>>,
    /// Symbols before `lock` may read locked.
    lock_min: u64,
}

impl Psk {
    /// Reads and checks the hot parameters.
    fn set_hot(&mut self, p: &Params) -> Result<(), BlockError> {
        let rot = i64_or(p, "rotation_deg", 0);
        let m = 1i64 << self.modulation.bits();
        let step = if self.modulation == Modulation::Oqpsk {
            90
        } else {
            360 / m
        };
        if self.modulation.differential() && rot != 0 {
            return Err(BlockError::Params(
                "differential modes have no phase ambiguity: rotation_deg must be 0".into(),
            ));
        }
        if rot % step != 0 {
            return Err(BlockError::Params(format!(
                "rotation_deg must be a multiple of {step} for this modulation"
            )));
        }
        self.rotation = (rot as f64).to_radians();
        self.rail_flip = match rot {
            90 => (true, false),
            180 => (true, true),
            270 => (false, true),
            _ => (false, false),
        };
        self.iq_swap = bool_or(p, "iq_swap", false);
        self.bw = f64_or(p, "loop_bandwidth", DEFAULT_LOOP_BANDWIDTH);
        match &mut self.tracker {
            Some(Tracker::Liquid(track)) => track.set_bandwidth(self.bw as f32),
            Some(Tracker::Oqpsk(o)) => o.set_bandwidth(self.bw),
            None => {}
        }
        Ok(())
    }

    fn restart(&mut self) {
        if let Some(r) = &mut self.rate {
            r.restart(0);
        }
        self.coarse.restart();
        self.unlocked_run = 0;
        self.reacq_pending = false;
        match &mut self.tracker {
            Some(Tracker::Liquid(track)) => track.reset(&mut self.zeros, &mut self.syms),
            Some(Tracker::Oqpsk(o)) => o.restart(),
            None => {}
        }
        self.prev_raw = None;
        self.prev_decision = None;
        self.power = 0.0;
        self.evm = 0.0;
        self.symbols = 0;
        self.locked = false;
        self.need_origin = true;
        self.res_count = 0;
        if let Some(b) = &mut self.burst {
            b.restart();
        }
    }

    /// Normalises, scores and de-maps one recovered symbol.
    fn symbol(
        &mut self,
        y: Complex64,
        soft: &mut Vec<f32>,
        diag: Option<(&mut Vec<Complex32>, &mut Vec<f32>, f64)>,
    ) {
        let alpha = if self.symbols < EVM_TAU_SYMBOLS as u64 {
            1.0 / (self.symbols + 1) as f64
        } else {
            1.0 / EVM_TAU_SYMBOLS
        };
        self.power += (y.norm_sqr() - self.power) * alpha;
        let y = y / self.power.sqrt().max(1e-12);
        self.symbols += 1;
        // Score against where the tracker puts the constellation.
        let (err, _) = if self.modulation.differential() {
            let d = self.prev_raw.map_or(y, |p| y * p.conj());
            self.table.nearest(d)
        } else {
            self.track_table.nearest(y)
        };
        self.evm += (err - self.evm) * alpha;
        // The lock detector runs per symbol, so the re-acquisition it drives is
        // chunking-invariant.
        let q = self.quality();
        if self.symbols >= self.lock_min && q >= LOCK_ON {
            self.locked = true;
        } else if q < LOCK_OFF || self.symbols < self.lock_min {
            self.locked = false;
        }
        if self.locked || !self.coarse.acquired {
            self.unlocked_run = 0;
        } else {
            self.unlocked_run += 1;
            if self.unlocked_run >= self.reacq_after {
                self.reacq_pending = true;
                self.unlocked_run = 0;
            }
        }
        // Timing-error diagnostic.
        if let Some((sym_out, te_out, te)) = diag {
            sym_out.push(Complex32::new(y.re as f32, y.im as f32));
            let e = if self.modulation == Modulation::Oqpsk {
                te
            } else {
                // Mueller–Müller on the recovered symbols.
                let (_, d) = self.track_table.nearest(y);
                let e = match (self.prev_raw, self.prev_decision) {
                    (Some(py), Some(pd)) => (pd.conj() * y - d.conj() * py).re,
                    _ => 0.0,
                };
                self.prev_decision = Some(d);
                e
            };
            te_out.push(e as f32);
        } else if self.modulation != Modulation::Oqpsk {
            self.prev_decision = Some(self.track_table.nearest(y).1);
        }
        // De-map.
        if self.modulation.differential() {
            if let Some(p) = self.prev_raw {
                let d = y * p.conj();
                let d = if self.iq_swap { d.conj() } else { d };
                self.table.demap(d, soft);
            } else {
                // No reference yet: the first symbol's bits are unknown, not guessed.
                soft.extend(std::iter::repeat_n(0.0, self.modulation.bits()));
            }
        } else if self.modulation == Modulation::Oqpsk {
            let v = if self.iq_swap {
                Complex64::new(y.im, y.re)
            } else {
                y
            };
            let i = if self.rail_flip.0 { -v.re } else { v.re };
            let q = if self.rail_flip.1 { -v.im } else { v.im };
            // Gray QPSK on independent rails: the exact max-log LLR of each rail's bit (1 ↔
            // negative), I first — the order the rails are strobed in.
            let scale = 2.0 * std::f64::consts::SQRT_2;
            soft.push((-scale * i) as f32);
            soft.push((-scale * q) as f32);
        } else {
            let v = if self.iq_swap {
                Complex64::new(y.im, y.re)
            } else {
                y
            };
            self.table
                .demap(v * Complex64::from_polar(1.0, -self.rotation), soft);
        }
        self.prev_raw = Some(y);
    }

    /// Constellation quality in [0, 1]: 1 − RMS EVM / decision radius.
    fn quality(&self) -> f64 {
        if self.symbols == 0 {
            return 0.0;
        }
        let evm = self.evm.max(0.0).sqrt();
        (1.0 - evm / self.score_radius()).clamp(0.0, 1.0)
    }

    fn update_status(&mut self) {
        let evm = self.evm.max(0.0).sqrt();
        let q = self.quality();
        self.status.lock = if self.locked {
            Lock::Locked
        } else {
            Lock::Searching
        };
        if self.symbols > 0 {
            self.status.quality = Some(q as f32);
            self.status.snr_db = (evm > 0.0).then(|| (-20.0 * evm.log10()) as f32);
        } else {
            self.status.quality = None;
            self.status.snr_db = None;
        }
        let mut w = match &self.burst {
            Some(b) => b.w(),
            None => self.coarse.w(),
        };
        if let Some(Tracker::Oqpsk(o)) = &self.tracker {
            w += o.frequency();
        }
        let x = &mut self.status.extra;
        x.set("symbol_rate_bd", self.symbol_rate);
        x.set("bits_per_symbol", self.modulation.bits() as f64);
        x.set("offset_hz", w * self.fs_res / TAU);
        match &self.burst {
            // Six extras at most: burst mode reports bursts in place of the streaming path's
            // `samples_per_symbol` and `symbols`.
            Some(b) => {
                x.set("bursts", b.bursts as f64);
                if b.dropped > 0 {
                    x.set("bursts_dropped", b.dropped as f64);
                }
            }
            None => {
                x.set("samples_per_symbol", self.k);
                x.set("symbols", self.symbols as f64);
            }
        }
        report_non_finite(&mut self.status, self.non_finite);
    }

    /// Decision radius of the constellation the EVM is scored against.
    fn score_radius(&self) -> f64 {
        if self.modulation.differential() {
            self.table.radius
        } else {
            self.track_table.radius
        }
    }
}

impl Block for Psk {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = single_input(inputs, "psk_demod", &[PortType::Iq])?;
        let fs = input.rate_hz;
        let rs = self.symbol_rate;
        let oqpsk = self.modulation == Modulation::Oqpsk;
        if fs < 2.0 * rs * (1.0 - 1e-9) {
            return Err(BlockError::Unrealisable(
                "psk_demod needs at least 2 samples per symbol".into(),
            ));
        }
        if fs / rs > MAX_SAMPLES_PER_SYMBOL {
            return Err(BlockError::Params(
                "psk_demod: too many samples per symbol; resample first".into(),
            ));
        }
        if self.pulse == Pulse::Rrc && self.rolloff <= 0.0 {
            return Err(BlockError::Params("rrc rolloff must be above 0".into()));
        }
        // Rate plan: the occupied bandwidth plus the acquisition range on both sides.
        let occupied = rs
            * match self.pulse {
                Pulse::Rrc => 1.0 + self.rolloff,
                Pulse::Rect => 2.0,
                Pulse::HalfSine => 2.4,
            };
        let want = occupied + 2.0 * self.max_offset_hz;
        let k_min = if oqpsk { 4.0 } else { 2.0 };
        // The acquisition's power: the M-th power line must stay inside the Nyquist band for
        // every offset it may find (π/4-DQPSK's absolute points are 8-PSK; OQPSK squares and
        // looks for the line pair at 2Δf ± Rs).
        let (power, side_hz) = match self.modulation {
            // Half-sine OQPSK is MSK: constant envelope, so x⁴ has no line at 4Δf, but its
            // square carries the pair at 2Δf ± Rs. With RRC pulses that pair shrinks with the
            // roll-off, while x⁴ keeps a line at 4Δf (the rails are far from Gaussian).
            Modulation::Oqpsk if self.pulse == Pulse::HalfSine => (2, rs),
            Modulation::Oqpsk => (4, 0.0),
            // Its x⁴ flips sign every symbol: a pair of lines at 4Δf ± Rs/2, far stronger
            // than x⁸'s single line.
            Modulation::Pi4Dqpsk => (4, rs / 2.0),
            _ => (1i32 << self.modulation.bits(), 0.0),
        };
        let unaliased = |k: f64| (k * rs / 2.0 - side_hz) / f64::from(power);
        let k_alias = if self.max_offset_hz > 0.0 {
            let mut k = k_min;
            while unaliased(k) * 0.9 < self.max_offset_hz && k < 64.0 {
                k += 1.0;
            }
            k
        } else {
            k_min
        };
        let k_req = (RATE_MARGIN * want / rs).ceil().max(k_alias);
        let ratio = fs / rs;
        let k = if ratio >= k_req {
            k_req
        } else if oqpsk {
            ratio
        } else if (ratio - ratio.round()).abs() < 1e-6 {
            ratio.round()
        } else {
            ratio.floor()
        };
        let resample = (fs - k * rs).abs() > 1e-9 * fs;
        let (max_res, rate_hold) = if resample {
            let mut rate = Rate::new(k * rs, want.min(0.9 * k * rs), 60.0);
            let r = rate.init(&input, 0.0, None)?;
            self.rate = Some(rate);
            r
        } else {
            self.rate = None;
            (input.max_items, 0)
        };
        self.k = k;
        self.fs_res = k * rs;
        // The acquisition range actually available at this rate: inside the passband, and
        // where the M-th power line cannot alias.
        let max_off = self
            .max_offset_hz
            .min(((self.fs_res - occupied) / 2.0).max(0.0))
            .min(0.9 * unaliased(k).max(0.0));
        let max_w = TAU * max_off / self.fs_res;
        let bits = self.modulation.bits();
        let m = 1usize << bits;
        // x⁸ buries its line deeper in noise, and RRC OQPSK's x⁴ line is weaker than QPSK's
        // (half its samples sit on a rail in transition): a longer window buys it back.
        let long = power == 8 || (oqpsk && power == 4);
        let acq_symbols = ACQ_SYMBOLS * if long { 2.0 } else { 1.0 };
        let acq_len = ((acq_symbols * k).ceil() as usize).next_power_of_two();
        let side_bins = (side_hz / self.fs_res * acq_len as f64).round() as usize;
        self.coarse = Coarse::new(max_w, power, side_bins, acq_len);
        self.segment = ((SEGMENT_SYMBOLS * k).round() as usize).max(1);
        self.reacq_after = REACQ_SYMBOLS.max(2 * (acq_len as f64 / k) as u64);
        self.tracker = Some(match self.modulation.liquid() {
            Some(name) => {
                let scheme = lq::scheme_id(lq::liquid_getopt_str2mod, name)
                    .ok_or_else(|| BlockError::Unrealisable(format!("liquid has no {name}")))?;
                let track = Symtrack::new(k as u32, self.rolloff as f32, scheme, self.bw as f32)?;
                // The tracker's absolute constellation: π/4-DQPSK alternates two QPSK sets.
                let m_abs = if self.modulation == Modulation::Pi4Dqpsk {
                    8
                } else {
                    m
                };
                self.track_table =
                    Constellation::psk(m_abs, liquid_theta0(scheme, m_abs)?, self.gray);
                Tracker::Liquid(track)
            }
            None => {
                self.track_table = Constellation::psk(4, PI / 4.0, self.gray);
                Tracker::Oqpsk(Box::new(Oqpsk::new(
                    k,
                    self.pulse,
                    self.rolloff,
                    self.bw,
                    max_res,
                )))
            }
        });
        self.table = if self.modulation.differential() {
            let phi0 = if self.modulation == Modulation::Pi4Dqpsk {
                PI / 4.0
            } else {
                0.0
            };
            Constellation::psk(m, phi0, self.gray)
        } else if self.modulation == Modulation::Oqpsk {
            Constellation::psk(4, PI / 4.0, self.gray)
        } else {
            // Coherent: de-map on liquid's own grid (where the tracker locks it), labelled the
            // textbook way.
            self.track_table
        };
        self.res = Vec::with_capacity(max_res);
        // liquid's tracker holds 2·k·m samples in its filterbanks; flush with twice that.
        self.zeros = vec![Complex32::new(0.0, 0.0); 4 * k as usize * TRACK_M as usize + 64];
        let span = max_res.max(self.segment).max(self.zeros.len());
        self.syms = vec![Complex32::new(0.0, 0.0); 2 * span];
        let per_span = |n: usize| (n as f64 / (k * (1.0 - MAX_TIMING_DEV))).ceil() as usize + 8;
        let mut max_sym = per_span(max_res);
        self.oq_out = Vec::with_capacity(per_span(span));
        let delay_symbols = if oqpsk {
            2.0 * OQPSK_SPAN + 1.0
        } else {
            LIQUID_DELAY_SYMBOLS
        };
        let mut hold = rate_hold + (delay_symbols * fs / rs).ceil() as usize;
        self.burst = None;
        self.lock_min = LOCK_MIN_SYMBOLS;
        if self.burst_mode {
            // Burst mode acquires each burst over its own samples: no streaming acquisition.
            let side = TAU * side_hz / self.fs_res;
            let acq = burst::Acquirer::new(
                power,
                side,
                max_w,
                k,
                (self.pulse == Pulse::Rrc).then_some(self.rolloff),
                oqpsk,
                acq_len,
            );
            let b = burst::Burst::new(k, acq_len, max_res, span, acq);
            max_sym = max_sym.max(per_span(b.max_samples()) + delay_symbols.ceil() as usize + 8);
            hold += (b.latency() as f64 * fs / self.fs_res).ceil() as usize;
            self.coarse = Coarse::off();
            self.reacq_after = u64::MAX;
            self.lock_min = burst::BURST_LOCK_MIN_SYMBOLS;
            self.burst = Some(Box::new(b));
        }
        self.status = Status::default();
        self.restart();
        self.update_status();
        let item_rate = rs * bits as f64;
        Ok(vec![
            PortInfo {
                ty: PortType::Soft,
                rate_hz: item_rate,
                max_items: max_sym * bits,
                hold_items: hold,
            },
            PortInfo {
                ty: PortType::Iq,
                rate_hz: rs,
                max_items: max_sym,
                hold_items: hold,
            },
            PortInfo {
                ty: PortType::Real,
                rate_hz: rs,
                max_items: max_sym,
                hold_items: hold,
            },
        ])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        let x = iq_in(&input)?;
        let m = input.meta;
        if restarts(m.flags) || self.pending_restart {
            self.restart();
            if let Some(r) = &mut self.rate {
                r.restart(m.index);
            }
            self.pending_restart = false;
        }
        // Resample to k samples per symbol and remove the coarse carrier estimate.
        let (first_res, per_res) = match &self.rate {
            Some(r) => (r.next_source(&m), r.per_item(&m)),
            None => (source_at(&m, m.index as f64), m.source_per_item),
        };
        if self.need_origin {
            self.origin = first_res;
            self.need_origin = false;
        }
        self.res.clear();
        match &mut self.rate {
            Some(r) => {
                r.scratch_in.clear();
                for &s in x {
                    r.scratch_in.push(finite_iq(s, &mut self.non_finite));
                }
                r.run();
                self.res.extend_from_slice(&r.scratch_out);
            }
            None => {
                for &s in x {
                    self.res.push(finite_iq(s, &mut self.non_finite));
                }
            }
        }
        let n_res = self.res.len();
        let bits = self.modulation.bits();
        let tapped = io.tapped(1) || io.tapped(2);
        // Track.
        let mut tracker = self
            .tracker
            .take()
            .ok_or_else(|| BlockError::Ports("psk_demod not initialised".into()))?;
        let mut soft = std::mem::take(soft_out(io.output(0)?)?);
        let mut sym_diag = std::mem::take(iq_out(io.output(1)?)?);
        let mut te_diag = std::mem::take(real_out(io.output(2)?)?);
        if let Some(mut b) = self.burst.take() {
            let call = self.run_burst(
                &mut b,
                &mut tracker,
                &mut soft,
                &mut sym_diag,
                &mut te_diag,
                tapped,
                m.flags.contains(ChunkFlags::END),
            );
            self.burst = Some(b);
            self.tracker = Some(tracker);
            let produced = soft.len();
            let per_sym = self.k * per_res;
            // A chunk with no items keeps the time map running at the input's position.
            let centre = call
                .first_centre
                .unwrap_or((first_res - self.origin) / per_res);
            let src = self.origin + centre * per_res;
            let flags = if call.burst_start {
                ChunkFlags::DISCONTINUITY
            } else {
                ChunkFlags::NONE
            };
            let out = io.output(0)?;
            set_meta(out, &m, src, per_sym / bits as f64);
            out.meta.flags |= flags;
            *soft_out(out)? = soft;
            let out = io.output(1)?;
            set_meta(out, &m, src, per_sym);
            out.meta.flags |= flags;
            *iq_out(out)? = sym_diag;
            let out = io.output(2)?;
            set_meta(out, &m, src, per_sym);
            out.meta.flags |= flags;
            *real_out(out)? = te_diag;
            self.status.items_in += x.len() as u64;
            self.status.items_out += produced as u64;
            self.update_status();
            return Ok(());
        }
        let symbols_before = self.symbols;
        let first_at = match &tracker {
            Tracker::Liquid(_) => {
                // liquid emits from its first sample (its filter starts on zeros), so output
                // symbol j is the one centred LIQUID_DELAY_SYMBOLS before j·k.
                (symbols_before as f64 - LIQUID_DELAY_SYMBOLS) * self.k
            }
            Tracker::Oqpsk(o) => o.next_symbol_at(),
        };
        // Segments end at fixed sample counts since restart. A re-acquisition decided by the
        // lock detector takes effect there, so chunking cannot move it.
        let mut i = 0;
        while i < n_res {
            let into = (self.res_count % self.segment as u64) as usize;
            let e = (i + self.segment - into).min(n_res);
            match &mut tracker {
                Tracker::Liquid(track) => {
                    for s in &mut self.res[i..e] {
                        *s = self.coarse.step(*s);
                    }
                    self.coarse.take_acquired();
                    let n = track.execute(&mut self.res[i..e], &mut self.syms);
                    for j in 0..n {
                        let s = self.syms[j];
                        let y = Complex64::new(f64::from(s.re), f64::from(s.im));
                        let diag = tapped.then_some((&mut sym_diag, &mut te_diag, 0.0));
                        self.symbol(y, &mut soft, diag);
                    }
                }
                Tracker::Oqpsk(o) => {
                    self.oq_out.clear();
                    let out = &mut self.oq_out;
                    for s in &mut self.res[i..e] {
                        *s = self.coarse.step(*s);
                        if self.coarse.take_acquired() {
                            o.reset_loops();
                        }
                        o.push(*s, |sym| out.push(sym));
                    }
                    let syms = std::mem::take(&mut self.oq_out);
                    for s in &syms {
                        let diag = tapped.then_some((&mut sym_diag, &mut te_diag, s.timing_error));
                        self.symbol(s.point, &mut soft, diag);
                    }
                    self.oq_out = syms;
                }
            }
            self.res_count += (e - i) as u64;
            if self.res_count % self.segment as u64 == 0 && self.reacq_pending {
                self.reacq_pending = false;
                self.coarse.reacquire();
            }
            i = e;
        }
        self.tracker = Some(tracker);
        let produced = soft.len();
        let per_sym = self.k * per_res;
        let src = self.origin + first_at * per_res;
        let out = io.output(0)?;
        set_meta(out, &m, src, per_sym / bits as f64);
        *soft_out(out)? = soft;
        let out = io.output(1)?;
        set_meta(out, &m, src, per_sym);
        *iq_out(out)? = sym_diag;
        let out = io.output(2)?;
        set_meta(out, &m, src, per_sym);
        *real_out(out)? = te_diag;
        self.status.items_in += x.len() as u64;
        self.status.items_out += produced as u64;
        self.update_status();
        Ok(())
    }

    fn reset(&mut self) {
        self.restart();
        self.pending_restart = true;
        self.update_status();
    }

    fn update_params(&mut self, p: &Params, _: &BuildCtx<'_>) -> Result<ParamUpdate, BlockError> {
        if !cold_equal(HOT, &self.params, p) {
            return Ok(ParamUpdate::Rebuild);
        }
        let old = self.params.clone();
        if let Err(e) = self.set_hot(p) {
            self.set_hot(&old)?;
            return Err(e);
        }
        self.params = p.clone();
        Ok(ParamUpdate::Applied)
    }

    fn status(&self) -> Status {
        self.status
    }
}

#[cfg(test)]
#[path = "psk_tests.rs"]
mod tests;
