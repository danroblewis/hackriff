//! `psk_demod` **burst mode** (T-875; ADR-0015 §10's M-14 residue; ADR-0011 §9.2 "burst
//! boundaries"). With `burst: true` the block decodes each burst **from its first symbol**, where
//! the streaming path (`burst: false`) spends its first 512–1 024 symbols filling the carrier
//! acquisition window and so loses the start of every packet shorter than that.
//!
//! # How a burst is acquired from its start: look-ahead, then feed-forward
//!
//! The block holds the resampled input in a FIFO and runs three stages over it:
//!
//! 1. **Find the burst (blind, energy).** Power over `BLOCK_SYMBOLS`-symbol blocks, averaged over
//!    two blocks, against a noise floor tracked between bursts. A rise of `ON_DB` opens a burst;
//!    falling below the geometric mean of the burst's level and the floor for `LOW_BLOCKS` blocks
//!    ends it; a further rise of `ON_DB` over the burst's own level ends it and opens the next.
//!    Both edges are refined to the symbol by a slot-power scan and widened by `MARGIN_SLOTS`, so
//!    a start is never cut: a consumer sees at most two symbols of noise before the first real
//!    one. The stream start opens a candidate too (a capture may begin on a burst), whose level
//!    stands in for the unknown floor.
//! 2. **Confirm and measure it (blind, feed-forward, over the burst's own samples).** Over the
//!    burst — up to `acq_len` samples, the streaming window, or the whole burst if shorter — the
//!    textbook estimators, each a closed form rather than a loop that needs time to settle:
//!    * **carrier frequency**: the M-th-power spectral line (the streaming [`super::Coarse`]
//!      estimator), Hann-windowed over the burst itself, zero-padded, and refined by a
//!      golden-section search on the line's DTFT;
//!    * **carrier phase**: the argument of that line at the burst centre (M-fold ambiguous, like
//!      every blind PSK receiver: `rotation_deg` resolves it);
//!    * **symbol timing**: Oerder–Meyr, the argument of `|x|²`'s spectral line at the symbol
//!      rate (OQPSK: `x²`'s pair at ±Rs, whose product also gives the carrier phase);
//!    * **amplitude**: the burst's mean power.
//!
//!    The line's **coherence** (its magnitude over the window's total magnitude) is the blind
//!    PSK-presence test: noise and non-PSK energy fail it and emit nothing.
//! 3. **Track it from the first sample.** The tracker is reset and fed from the burst start,
//!    through a windowed-sinc fractional delay that puts liquid's strobes (measured: exactly
//!    `(j − 9)·k` after a reset, `liquid_strobes_nine_symbols_behind_the_feed_after_a_reset`) on the estimated symbol centres,
//!    with the carrier de-rotated at the estimated frequency and phase and the amplitude
//!    normalised. Every loop therefore starts converged, and the first symbols decode. The
//!    tracker lags the energy detector by `LOOKAHEAD_BLOCKS`, so a symbol is emitted only once
//!    it is known to lie inside the burst; at the burst's end it is flushed with zeros.
//!
//! # Output
//!
//! Only symbols inside a burst are emitted; between bursts the outputs carry no items. **The
//! first item of each burst is the first item of an output chunk and carries `DISCONTINUITY`**
//! (ADR-0011 §9.2), and every item keeps its absolute capture time. Since a chunk's flags and
//! time map apply from its first item, one output chunk never holds two bursts: when a second
//! burst is ready in a call that already emitted one, the block stops there and resumes on the
//! next call (the FIFO holds the rest). A sustained rate of more bursts than calls overruns the
//! FIFO; the burst in hand is then dropped and **counted** (`bursts_dropped`), never silently
//! merged. Apart from that overload case the output is chunking-invariant (ADR-0011 §1.6).
//!
//! # Measured: the shortest burst decoded whole, before and after
//!
//! `burst_mode_shortest_burst_report`: one burst of `n` symbols between 300 symbols of noise, at
//! the streaming test's Es/N0, 600 Hz carrier offset (0.125·Rs), 1.1 rad, 0.37-symbol timing
//! offset, 48 kS/s resampled to the tracker's rate; bits matched by the output's time map, best
//! rotation. "Whole" = every bit from the first (differential modes: bar the first symbol, which
//! has no reference; OQPSK: bar one bit of rail ambiguity).
//!
//! | mode @ Es/N0 | streaming path (`burst: false`) | burst mode |
//! |---|---|---|
//! | BPSK @ 9 dB | none ≤ 2 048 (first 187 symbols lost) | **8** |
//! | DBPSK @ 11 dB | 64 (differential detection rides out 0.125·Rs) | **8** |
//! | QPSK @ 13 dB | none (first 186 symbols lost) | **8** |
//! | DQPSK @ 15 dB | none (first 99 lost) | **8** |
//! | π/4-DQPSK @ 15 dB | none (first ~110 lost) | **8** |
//! | 8PSK @ 19 dB | none (first ~670 lost) | **8** |
//! | D8PSK @ 21 dB | none (first ~445 lost) | **8** |
//! | OQPSK half-sine @ 12 dB | none (first ~158 lost) | **8** |
//! | OQPSK RRC @ 13 dB | none (first ~427 lost) | **64** |
//!
//! 8 symbols is `MIN_BURST_SYMBOLS`. RRC OQPSK is the one exception: its raw x⁴ line is weak
//! (coherence ≈ 0.3), its second-order line at symbol rate weaker still, so a burst shorter than
//! 64 symbols does not pass the PSK test and emits nothing. Below about 20 symbols the line test
//! cannot separate PSK from noise at all (`RISE_ACCEPT`): a rise is then taken on its energy,
//! and a 12-symbol burst of pure noise is emitted as junk about half the time
//! (`burst_mode_false_accept_report`); from 24 symbols up, 1 in 640.
//!
//! # Known limits
//!
//! * **One burst per output chunk** (above): a stream whose bursts outnumber its `process`
//!   calls overruns and counts `bursts_dropped`, and so does a second burst still waiting when
//!   `END` arrives (one call holding a whole capture with two bursts in it).
//! * The resampler's first output lands a filter span into a stream (about 4 symbols at 48 kS/s
//!   → 14.4 kS/s), on either path: a capture cut exactly at a burst's first sample loses those.
//! * `lock` reads `searching` between bursts; `quality` and `snr_db` hold the last burst's.
//! * A burst is acquired once, over its first `acq_len` samples; a longer one is then tracked
//!   like the streaming path (no re-acquisition inside a burst).
//! * Per-burst cost is a fixed search (up to 4 carrier candidates × 8 timing phases over at most
//!   `STAGE2_SYMBOLS`), independent of the burst's length beyond that; not yet measured in a
//!   release build.
//!
//! # Latency
//!
//! A burst's first items leave when its acquisition lands: at its end, or after `acq_len`
//! samples of a longer one, which then streams with the detector's look-ahead. `init` adds that
//! to `hold_items`.
//!
//! Every algorithm is a published textbook one; nothing is derived from GPL decoder source
//! (ADR-0010), and no known-signal preamble or database lookup is used: the burst is found and
//! measured from its own samples.

use std::f64::consts::{PI, TAU};

use hk_dsp::{CpuFft, FftBackend};
use num_complex::{Complex32, Complex64};

use super::{Psk, Tracker};

/// Energy detector block, symbols.
pub(super) const BLOCK_SYMBOLS: f64 = 4.0;
/// Power rise that opens a burst (over the floor, or over the open burst's level), dB.
const ON_DB: f64 = 6.0;
/// Slot power over the floor that marks a burst's refined edges, dB.
const EDGE_DB: f64 = 3.0;
/// Consecutive low detector windows that end a burst.
const LOW_BLOCKS: u32 = 2;
/// Symbols of margin added before a refined start and after a refined end.
const MARGIN_SLOTS: u64 = 2;
/// Fewest symbols a burst must span to be acquired.
const MIN_BURST_SYMBOLS: f64 = 8.0;
/// Detector blocks the tracker lags the detector by: at least the refinement look-back, so an
/// emitted symbol is always inside the burst's final extent.
const LOOKAHEAD_BLOCKS: u64 = 5;
/// Half-length of the fractional-delay filter, taps.
const FRAC_HALF: usize = 6;
/// A candidate is taken for PSK when its line coherence beats noise's typical peak
/// ([`Fit::threshold`]) by this margin. A candidate the stream start opened has no energy
/// evidence behind it; a rise does. Measured (`burst_mode_false_accept_report`), 40 seeds × 4
/// modes × 4 lengths of noise bursts (24–400 symbols, 10 dB up): 1 of 640 accepted at 1.4;
/// noise-only streams: none.
const MARGIN_RESTART: f64 = 1.6;
const MARGIN_RISE: f64 = 1.4;
/// Coherence that always confirms a rise, however short: below about 20 symbols no blind line
/// test separates PSK from noise, and a rise is taken on its energy.
const RISE_ACCEPT: f64 = 0.7;
/// Symbols a burst must run before `lock` may read locked (the streaming path waits 64).
pub(super) const BURST_LOCK_MIN_SYMBOLS: u64 = 16;
/// liquid's symbol `j` after a reset is centred `LIQUID_DELAY` symbols before feed sample `j·k`
/// (pinned by `liquid_strobes_nine_symbols_behind_the_feed_after_a_reset`: 9 symbols, no fractional
/// offset).
const LIQUID_DELAY: f64 = 9.0;

fn db(v: f64) -> f64 {
    10f64.powf(v / 10.0)
}

// ------------------------------------------------------------------------------ estimation

/// What the feed-forward estimators found over one burst window.
#[derive(Clone, Copy, Debug)]
pub(super) struct Fit {
    /// Carrier offset, rad/sample.
    pub w: f64,
    /// Line coherence in [0, 1] (see [`Acquirer::fit`]).
    pub coherence: f64,
    /// Independent hypotheses searched, samples (or symbols) in the line, and the noise
    /// magnitudes' `E|z|²/(E|z|)²` times the window's `n·Σw²/(Σw)²`: see [`Fit::threshold`].
    pub hypotheses: f64,
    pub n: usize,
    pub ratio: f64,
    /// Carrier phase at the window centre, rad, where the mode allows one (mod 2π/M): the
    /// de-rotation that puts the constellation's point 0 at angle 0.
    pub phase: Option<f64>,
    /// First symbol centre after the window start, samples, in `[0, k)` (OQPSK: `[0, k/2)`).
    pub tau: f64,
    /// Mean power over the window.
    pub power: f64,
}

impl Fit {
    /// `margin` times the coherence noise alone typically peaks at: `|S|²/(Σ|z|)²` of noise is
    /// `ratio/n` times an exponential per hypothesis, and the largest of `B` is about
    /// `ln B + γ`. (The hypotheses are correlated — a zero-padded FFT, a refined peak,
    /// neighbouring timing phases — so the tail is heavier than independent exponentials
    /// would give, and the margins below are measured, not derived.)
    pub fn threshold(&self, margin: f64) -> f64 {
        margin
            * (self.ratio * (self.hypotheses.max(1.0).ln() + 0.577) / self.n.max(1) as f64).sqrt()
    }
}

/// Raw-rate line candidates tried at symbol rate.
const CANDIDATES: usize = 4;
/// Matched-filter half-length of the symbol-rate stage, symbols.
const MF_SPAN: f64 = 6.0;
/// Timing phases the symbol-rate stage tries per carrier candidate.
const TIMING_PHASES: usize = 8;
/// Most symbols the symbol-rate stage uses (from the burst start): bounds its cost per burst,
/// and the start is where the seeds matter.
const STAGE2_SYMBOLS: f64 = 256.0;
/// Most symbols OQPSK's timing and phase seeds are taken over.
const OQPSK_SEED_SYMBOLS: f64 = 512.0;

/// Golden-section search for the maximum of `f` on `[lo, hi]`.
fn golden(mut lo: f64, mut hi: f64, mut f: impl FnMut(f64) -> f64) -> f64 {
    let g = (5f64.sqrt() - 1.0) / 2.0;
    let mut a = hi - g * (hi - lo);
    let mut b = lo + g * (hi - lo);
    let (mut fa, mut fb) = (f(a), f(b));
    for _ in 0..24 {
        if fa > fb {
            hi = b;
            b = a;
            fb = fa;
            a = hi - g * (hi - lo);
            fa = f(a);
        } else {
            lo = a;
            a = b;
            fa = fb;
            b = lo + g * (hi - lo);
            fb = f(b);
        }
    }
    0.5 * (lo + hi)
}

/// DTFT of `z` at `theta`, referenced to its centre.
fn dtft(z: &[Complex64], theta: f64) -> Complex64 {
    let c = (z.len() as f64 - 1.0) / 2.0;
    let step = Complex64::from_polar(1.0, -theta);
    let mut rot = Complex64::from_polar(1.0, theta * c);
    let mut acc = Complex64::new(0.0, 0.0);
    for &v in z {
        acc += v * rot;
        rot *= step;
    }
    acc
}

/// `x^m / |x|^(m−1)`: the M-th power line of an M-PSK point, at the point's magnitude.
fn mth(x: Complex64, m: i32) -> Complex64 {
    let mag = x.norm();
    if mag > 0.0 {
        x.powi(m) / mag.powi(m - 1)
    } else {
        Complex64::new(0.0, 0.0)
    }
}

/// The burst estimators. Holds its FFTs and scratch, so a fit never allocates.
pub(super) struct Acquirer {
    power: i32,
    /// Half the spacing of a line pair, rad/sample (0: a single line).
    side: f64,
    max_w: f64,
    k: f64,
    oqpsk: bool,
    /// RRC roll-off for the symbol-rate stage (liquid modes); `None`: raw-rate estimates only
    /// (OQPSK, whose native receiver's fast timing loop finishes the job).
    rolloff: Option<f64>,
    n_fft: usize,
    fft: CpuFft,
    buf: Vec<Complex32>,
    z: Vec<Complex64>,
    xr: Vec<Complex64>,
    taps: Vec<f64>,
    sym: Vec<Complex64>,
    n2: usize,
    fft2: CpuFft,
    buf2: Vec<Complex32>,
}

impl Acquirer {
    pub(super) fn new(
        power: i32,
        side: f64,
        max_w: f64,
        k: f64,
        rolloff: Option<f64>,
        oqpsk: bool,
        n: usize,
    ) -> Self {
        let max_syms = (n as f64 / k).ceil() as usize + 2;
        let n2 = (4 * max_syms.min(STAGE2_SYMBOLS as usize)).next_power_of_two();
        let half = (MF_SPAN * k).ceil() as usize + 1;
        Self {
            power,
            side,
            max_w,
            k,
            oqpsk,
            // OQPSK's second-order line at symbol rate is weak by construction (each rail is in
            // transition at the other's strobe), so it keeps the raw-rate estimates. The stage
            // needs constant matched-filter phases: an integer k (liquid's own requirement).
            rolloff: rolloff.filter(|_| !oqpsk && (k - k.round()).abs() < 1e-9),
            n_fft: n,
            fft: CpuFft::new(n),
            buf: vec![Complex32::new(0.0, 0.0); n],
            z: Vec::with_capacity(n),
            xr: Vec::with_capacity(n),
            taps: Vec::with_capacity(2 * half + 1),
            sym: Vec::with_capacity(max_syms),
            n2,
            fft2: CpuFft::new(n2),
            buf2: vec![Complex32::new(0.0, 0.0); n2],
        }
    }

    fn score(&self, theta: f64) -> f64 {
        if self.side == 0.0 {
            dtft(&self.z, theta).norm_sqr()
        } else {
            dtft(&self.z, theta - self.side).norm_sqr()
                + dtft(&self.z, theta + self.side).norm_sqr()
        }
    }

    /// Fits the carrier and the symbol timing over `x` (at most `n_fft` samples).
    ///
    /// 1. **Timing** (Oerder–Meyr) from `|x|²`, which does not depend on the carrier.
    /// 2. **Raw-rate carrier**: the M-th-power line (a pair for π/4-DQPSK and half-sine OQPSK)
    ///    of the Hann-windowed burst, zero-padded to `n_fft`; the best `CANDIDATES` peaks.
    /// 3. **Symbol-rate carrier** (liquid modes): for each candidate, de-rotate, matched-filter
    ///    and sample at the estimated centres, where an RRC burst is free of ISI, and fit the
    ///    M-th-power line of those symbols over the full ±π; the most coherent candidate wins.
    ///    That line is far cleaner than the raw one (8PSK: coherence ≈ 0.8 against ≈ 0.22 at
    ///    21 dB), which is what lets short 8PSK bursts be found at all.
    ///
    /// **Coherence** is `|S(θ̂)| / Σ w|z|` for the winning line `S` (a pair: root-sum-square):
    /// 1 for a pure line; noise reaches about [`noise_coherence`].
    pub(super) fn fit(&mut self, x: &[Complex32]) -> Fit {
        let n = x.len().min(self.n_fft);
        let x = &x[..n];
        let m = self.power;
        let hann = |i: usize| 0.5 - 0.5 * (TAU * (i as f64 + 0.5) / n as f64).cos();
        // The M-th power, magnitude-normalised, windowed.
        self.z.clear();
        let mut total = 0.0;
        let mut power = 0.0;
        for (i, v) in x.iter().enumerate() {
            let s = Complex64::new(f64::from(v.re), f64::from(v.im));
            power += s.norm_sqr();
            let zz = mth(s, m);
            let w = hann(i);
            total += w * zz.norm();
            self.z.push(zz * w);
        }
        let power = power / n.max(1) as f64;
        // Raw-rate candidates: the zero-padded FFT's best lines inside the reachable range.
        for (b, v) in self.buf.iter_mut().enumerate() {
            *v = self.z.get(b).map_or(Complex32::new(0.0, 0.0), |z| {
                Complex32::new(z.re as f32, z.im as f32)
            });
        }
        self.fft.forward(&mut self.buf);
        let nf = self.n_fft as isize;
        let d = (self.side / TAU * self.n_fft as f64).round() as isize;
        let pw = |b: isize| f64::from(self.buf[b.rem_euclid(nf) as usize].norm_sqr());
        let sc = |b: isize| if d == 0 { pw(b) } else { pw(b - d) + pw(b + d) };
        let reach = ((f64::from(m) * self.max_w / TAU * self.n_fft as f64).ceil() as isize)
            .min(nf / 2 - 1 - d)
            .max(0);
        // A Hann mainlobe over n samples spans ±2 bins of n, so ±2·n_fft/n padded bins.
        let lobe = (2.0 * self.n_fft as f64 / n.max(1) as f64).ceil() as isize;
        let mut cands = [(f64::NEG_INFINITY, 0isize); CANDIDATES];
        for slot in 0..CANDIDATES {
            let mut best = (f64::NEG_INFINITY, 0isize);
            for b in -reach..=reach {
                if cands[..slot].iter().any(|c| (c.1 - b).abs() <= lobe) {
                    continue;
                }
                let s = sc(b);
                if s > best.0 {
                    best = (s, b);
                }
            }
            cands[slot] = best;
        }
        let lim = f64::from(m) * self.max_w;
        // Parabolic interpolation of each candidate's peak.
        let theta_of = |b: isize| {
            let (l, c, r) = (sc(b - 1), sc(b), sc(b + 1));
            let den = l - 2.0 * c + r;
            let frac = if den.abs() > 0.0 {
                (0.5 * (l - r) / den).clamp(-0.5, 0.5)
            } else {
                0.0
            };
            (TAU * (b as f64 + frac) / self.n_fft as f64).clamp(-lim, lim)
        };
        let mut thetas = [None; CANDIDATES];
        for (t, c) in thetas.iter_mut().zip(&cands) {
            *t = c.0.is_finite().then(|| theta_of(c.1));
        }
        // Timing: Oerder–Meyr (OQPSK: from x² after the carrier, below).
        let c = (n as f64 - 1.0) / 2.0;
        let mut x1 = Complex64::new(0.0, 0.0);
        for (i, v) in x.iter().enumerate() {
            let p = f64::from(v.norm_sqr()) * hann(i);
            x1 += Complex64::from_polar(p, -TAU * i as f64 / self.k);
        }
        let tau_om = (-x1.arg() * self.k / TAU).rem_euclid(self.k);
        if let Some(beta) = self.rolloff {
            let (w, coherence, phase, tau, syms, tried) =
                self.symbol_rate(x, &thetas, tau_om, beta);
            return Fit {
                w: w.clamp(-self.max_w, self.max_w),
                coherence,
                hypotheses: tried as f64,
                n: syms,
                ratio: 4.0 / PI,
                phase: Some(phase),
                tau,
                power,
            };
        }
        // Raw-rate estimate only: golden-section refinement of the best line's DTFT.
        let mut theta = thetas[0].unwrap_or(0.0);
        if reach > 0 {
            let span = TAU / self.n_fft as f64;
            theta = golden((theta - span).max(-lim), (theta + span).min(lim), |t| {
                self.score(t)
            });
        } else {
            theta = 0.0;
        }
        let w = theta / f64::from(m);
        let coherence = if total > 0.0 {
            self.score(theta).sqrt() / total
        } else {
            0.0
        };
        let bins = ((2 * reach + 1) as f64 * n as f64 / self.n_fft as f64).max(2.0);
        let (tau, phase) = if self.oqpsk {
            // x² after the carrier: lines at ±Rs with phases 2φ ∓ 2πτ/k. Over at most the
            // burst's first `OQPSK_SEED_SYMBOLS`: a phase from a longer window is carried back
            // across it on the frequency estimate (measured: 15 bit errors at the start of a
            // 1 024-symbol RRC burst at 13 dB with the whole window, none at 512).
            let n1 = n.min((OQPSK_SEED_SYMBOLS * self.k).ceil() as usize).max(1);
            let c1 = (n1 as f64 - 1.0) / 2.0;
            let hann1 = |i: usize| 0.5 - 0.5 * (TAU * (i as f64 + 0.5) / n1 as f64).cos();
            let (mut a, mut b) = (Complex64::new(0.0, 0.0), Complex64::new(0.0, 0.0));
            for (i, v) in x[..n1].iter().enumerate() {
                let s = Complex64::new(f64::from(v.re), f64::from(v.im))
                    * Complex64::from_polar(1.0, -w * (i as f64 - c1));
                let q = s * s * hann1(i);
                let e = Complex64::from_polar(1.0, -TAU * i as f64 / self.k);
                a += q * e;
                b += q * e.conj();
            }
            let tau = ((b * a.conj()).arg() * self.k / (2.0 * TAU)).rem_euclid(self.k / 2.0);
            // The phase must be known mod π **for the rail at tau**, not just mod π/2 (A·B's
            // e^{j4φ}): its I strobe reads the real part, so a quadrant off starts the Costas
            // loop on the other rail and the timing loop slips half a symbol to follow, after the
            // chunk's first item was stamped. A = e^{j(2φ − 2πτ/k)} gives it, with τ chosen. φ
            // is at the sub-window's centre; carried to the window centre.
            let phi = 0.5 * (a.arg() + TAU * tau / self.k);
            (tau, Some(phi + w * (c - c1)))
        } else {
            let phase = (self.side == 0.0).then(|| dtft(&self.z, theta).arg() / f64::from(m));
            (tau_om, phase)
        };
        Fit {
            w,
            coherence,
            hypotheses: bins,
            n,
            ratio: 1.5 * 4.0 / PI,
            phase,
            tau,
            power,
        }
    }

    /// De-rotates the window by `w_c` (centred) into `xr`, far enough for `STAGE2_SYMBOLS`.
    fn derotate(&mut self, x: &[Complex32], w_c: f64) {
        let n = x.len();
        let c = (n as f64 - 1.0) / 2.0;
        let need = (((STAGE2_SYMBOLS + 1.0) * self.k + MF_SPAN * self.k).ceil() as usize + 2)
            .saturating_add(self.k.ceil() as usize)
            .min(n);
        self.xr.clear();
        for (i, v) in x[..need].iter().enumerate() {
            let s = Complex64::new(f64::from(v.re), f64::from(v.im));
            self.xr
                .push(s * Complex64::from_polar(1.0, -w_c * (i as f64 - c)));
        }
    }

    /// Matched-filters `xr` and samples it at `tau + m·k` (at most `STAGE2_SYMBOLS` symbols
    /// inside the window of `n` samples), leaving each symbol's M-th power in `sym`. Returns
    /// Σ|z|.
    fn mf_symbols(&mut self, n: usize, tau: f64, beta: f64) -> f64 {
        let m = self.power;
        let k = self.k.round() as isize;
        // RRC taps at the (constant, k being an integer) fractional phase of tau.
        let half = (MF_SPAN * self.k).ceil() as isize;
        let i0 = tau.floor() as isize;
        let f = tau - i0 as f64;
        self.taps.clear();
        for l in -half..=half {
            self.taps.push(super::rrc((f - l as f64) / self.k, beta));
        }
        self.sym.clear();
        let mut total = 0.0;
        let mut i = i0;
        let mut odd = false;
        while i < n as isize && (self.sym.len() as f64) < STAGE2_SYMBOLS {
            let mut y = Complex64::new(0.0, 0.0);
            for (t, &h) in self.taps.iter().enumerate() {
                let j = i + t as isize - half;
                if let Some(v) = usize::try_from(j).ok().and_then(|j| self.xr.get(j)) {
                    y += *v * h;
                }
            }
            let mut z = mth(y, m);
            if self.side != 0.0 && odd {
                // π/4-DQPSK: the absolute points alternate between two QPSK sets, so x⁴
                // alternates sign.
                z = -z;
            }
            total += z.norm();
            self.sym.push(z);
            odd = !odd;
            i += k;
        }
        total
    }

    /// The strongest line of `sym` over the full ±π: (ν rad/symbol, |S(ν)|).
    fn line_search(&mut self) -> (f64, f64) {
        for (b, v) in self.buf2.iter_mut().enumerate() {
            *v = self.sym.get(b).map_or(Complex32::new(0.0, 0.0), |z| {
                Complex32::new(z.re as f32, z.im as f32)
            });
        }
        self.fft2.forward(&mut self.buf2);
        let mut best = (f32::NEG_INFINITY, 0usize);
        for (b, v) in self.buf2.iter().enumerate() {
            if v.norm_sqr() > best.0 {
                best = (v.norm_sqr(), b);
            }
        }
        let nb = self.n2 as f64;
        let b = best.1 as f64;
        let theta = TAU * if b > nb / 2.0 { b - nb } else { b } / nb;
        let span = TAU / nb;
        let sym = &self.sym;
        let nu = golden(theta - span, theta + span, |t| dtft(sym, t).norm_sqr());
        (nu, dtft(sym, nu).norm())
    }

    /// The symbol-rate stage: a joint search over the raw carrier candidates `thetas` (M-th
    /// power domain, rad/sample) and `TIMING_PHASES` timing phases around the Oerder–Meyr
    /// `tau0`, then a golden refinement of the timing. Returns (carrier rad/sample, coherence,
    /// phase at the window centre, timing, symbols used, hypotheses tried).
    fn symbol_rate(
        &mut self,
        x: &[Complex32],
        thetas: &[Option<f64>],
        tau0: f64,
        beta: f64,
    ) -> (f64, f64, f64, f64, usize, usize) {
        let n = x.len();
        let c = (n as f64 - 1.0) / 2.0;
        let m = f64::from(self.power);
        let k = self.k;
        let mut best: Option<(f64, f64, f64, f64)> = None; // (coherence, w_c, tau, nu)
        let mut tried = 0;
        for theta in thetas.iter().flatten() {
            let w_c = theta / m;
            self.derotate(x, w_c);
            for p in 0..TIMING_PHASES {
                let tau = (tau0 + k * p as f64 / TIMING_PHASES as f64).rem_euclid(k);
                let total = self.mf_symbols(n, tau, beta);
                if self.sym.len() < 4 || total <= 0.0 {
                    continue;
                }
                tried += self.sym.len();
                let (nu, mag) = self.line_search();
                let coh = mag / total;
                if best.is_none_or(|b| coh > b.0) {
                    best = Some((coh, w_c, tau, nu));
                }
            }
        }
        let Some((_, w_c, tau_g, nu_g)) = best else {
            return (0.0, 0.0, 0.0, tau0, 0, 1);
        };
        // Refine the timing between the grid's neighbours, at the found line.
        self.derotate(x, w_c);
        let step = k / TIMING_PHASES as f64;
        let tau = golden(tau_g - step / 2.0, tau_g + step / 2.0, |t| {
            let total = self.mf_symbols(n, t.rem_euclid(k), beta);
            if total > 0.0 {
                dtft(&self.sym, nu_g).norm() / total
            } else {
                0.0
            }
        })
        .rem_euclid(k);
        let total = self.mf_symbols(n, tau, beta);
        let syms = self.sym.len();
        if syms < 4 || total <= 0.0 {
            return (w_c, 0.0, 0.0, tau, syms, tried.max(1));
        }
        let (nu, mag) = self.line_search();
        let coherence = mag / total;
        let s2 = dtft(&self.sym, nu);
        let dw = nu / (m * k);
        // Phase of the middle symbol used, then back to the window centre along the carrier.
        let t_mc = tau + (syms as f64 - 1.0) / 2.0 * k;
        let phase = s2.arg() / m - dw * (t_mc - c);
        (w_c + dw, coherence, phase, tau, syms, tried.max(1))
    }
}

/// Windowed-sinc taps for a fractional advance `mu` in [0, 1): `y[i] = Σ h[t]·x[i + t − H + 1]`
/// is `x(i + mu)`.
fn frac_taps(mu: f64) -> [f32; 2 * FRAC_HALF] {
    let mut h = [0f32; 2 * FRAC_HALF];
    let half = FRAC_HALF as f64;
    let mut sum = 0.0;
    let mut raw = [0f64; 2 * FRAC_HALF];
    for (t, r) in raw.iter_mut().enumerate() {
        let u = t as f64 - (half - 1.0) - mu;
        let sinc = if u.abs() < 1e-12 {
            1.0
        } else {
            (PI * u).sin() / (PI * u)
        };
        // Blackman over (−H, H).
        let a = PI * u / half;
        let w = 0.42 + 0.5 * a.cos() + 0.08 * (2.0 * a).cos();
        *r = sinc * w;
        sum += *r;
    }
    for (o, r) in h.iter_mut().zip(raw) {
        *o = (r / sum) as f32;
    }
    h
}

// ------------------------------------------------------------------------------- the state

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Found, window filling.
    Collecting,
    /// Acquired: the tracker is being fed.
    Tracking,
    /// Not PSK (failed the line test, or too short): waited out, nothing emitted.
    Rejected,
}

/// One burst candidate, positions in resampled samples since restart.
#[derive(Clone, Copy, Debug)]
struct Seg {
    start: u64,
    kind: Kind,
    /// Opened by the stream start (floor unknown) rather than by a rise.
    from_restart: bool,
    level: f64,
    blocks: u32,
    low_run: u32,
    end: Option<u64>,
}

/// Burst-mode state (see the module docs). Owned by [`Psk`] when `burst: true`.
pub(super) struct Burst {
    slot: u64,
    blk: u64,
    acq_len: u64,
    min_len: u64,
    lookahead: u64,
    cap: usize,
    fifo: Vec<Complex32>,
    /// Resampled index of `fifo[0]`.
    base: u64,
    /// Next detector block start.
    det: u64,
    prev_p: Option<f64>,
    floor: Option<f64>,
    seg: Option<Seg>,
    /// A burst found while another was open: (start, level), opened when the other closes.
    pending: Option<(u64, f64)>,
    /// End of the last closed candidate: a new start never reaches back before it.
    last_end: u64,
    // The open burst's feed.
    taps: [f32; 2 * FRAC_HALF],
    feed_pos: u64,
    nco: f64,
    w: f64,
    gain: f32,
    /// Resampled position of tracker symbol 0's centre.
    first_centre: f64,
    /// Resampled position of the tracker's feed sample 0.
    feed_origin: f64,
    track_syms: u64,
    emitted: bool,
    acq: Acquirer,
    feed: Vec<Complex32>,
    pub(super) bursts: u64,
    pub(super) dropped: u64,
}

/// The three outputs: soft bits, the `symbols` and `timing_error` diagnostics.
type Outs<'a> = (&'a mut Vec<f32>, &'a mut Vec<Complex32>, &'a mut Vec<f32>);

/// What one `process` call emitted.
#[derive(Default)]
pub(super) struct CallOut {
    /// Resampled position of the first emitted item's centre.
    pub first_centre: Option<f64>,
    /// The first item is a burst's first item.
    pub burst_start: bool,
}

impl Burst {
    /// `k` samples per symbol, the streaming acquisition window (`acq_len`), the largest input
    /// chunk after resampling (`max_res`) and the estimator's configuration.
    /// `feed_cap` bounds one tracker call (the caller's symbol scratch holds twice that).
    pub(super) fn new(
        k: f64,
        acq_len: usize,
        max_res: usize,
        feed_cap: usize,
        acq: Acquirer,
    ) -> Self {
        let slot = k.round().max(1.0) as u64;
        let blk = (BLOCK_SYMBOLS * k).round().max(1.0) as u64;
        let lookahead = LOOKAHEAD_BLOCKS * blk + FRAC_HALF as u64 + 1;
        let cap = 2 * acq_len + 8 * max_res.max(blk as usize) + 16 * blk as usize;
        Self {
            slot,
            blk,
            acq_len: acq_len as u64,
            min_len: (MIN_BURST_SYMBOLS * k).ceil() as u64,
            lookahead,
            cap,
            fifo: Vec::with_capacity(cap),
            base: 0,
            det: 0,
            prev_p: None,
            floor: None,
            seg: None,
            pending: None,
            last_end: 0,
            taps: frac_taps(0.0),
            feed_pos: 0,
            nco: 0.0,
            w: 0.0,
            gain: 1.0,
            first_centre: 0.0,
            feed_origin: 0.0,
            track_syms: 0,
            emitted: false,
            acq,
            feed: Vec::with_capacity(feed_cap.max(1)),
            bursts: 0,
            dropped: 0,
        }
    }

    /// Most resampled samples one call can process (the FIFO's bound): sizes the outputs.
    pub(super) fn max_samples(&self) -> usize {
        self.cap
    }

    /// Resampled samples a burst's first item waits for (acquisition plus look-ahead).
    pub(super) fn latency(&self) -> usize {
        (self.acq_len + self.lookahead) as usize
    }

    /// Drops everything; the stream start opens a candidate.
    pub(super) fn restart(&mut self) {
        self.fifo.clear();
        self.base = 0;
        self.det = 0;
        self.prev_p = None;
        self.floor = None;
        self.pending = None;
        self.last_end = 0;
        self.seg = Some(Seg {
            start: 0,
            kind: Kind::Collecting,
            from_restart: true,
            level: 0.0,
            blocks: 0,
            low_run: 0,
            end: None,
        });
    }

    /// The carrier offset being removed, rad/sample.
    pub(super) fn w(&self) -> f64 {
        self.w
    }

    fn end_pos(&self) -> u64 {
        self.base + self.fifo.len() as u64
    }

    fn at(&self, i: u64) -> Complex32 {
        i.checked_sub(self.base)
            .and_then(|j| self.fifo.get(j as usize))
            .copied()
            .unwrap_or(Complex32::new(0.0, 0.0))
    }

    fn slot_power(&self, s: u64) -> f64 {
        (s..s + self.slot)
            .map(|i| f64::from(self.at(i).norm_sqr()))
            .sum::<f64>()
            / self.slot as f64
    }

    /// Oldest sample still needed.
    fn keep_from(&self) -> u64 {
        let margin = self.slot + FRAC_HALF as u64 + 2;
        let mut keep = self
            .det
            .saturating_sub((LOOKAHEAD_BLOCKS + 2) * self.blk + margin);
        if let Some(s) = &self.seg {
            let from = if s.kind == Kind::Tracking {
                self.feed_pos
            } else {
                s.start
            };
            keep = keep.min(from.saturating_sub(margin));
        }
        if let Some((s, _)) = self.pending {
            keep = keep.min(s.saturating_sub(margin));
        }
        keep
    }

    /// Appends resampled samples. On overrun (more bursts than calls, sustained) the burst in
    /// hand is dropped and counted, and detection restarts at the new data.
    pub(super) fn push(&mut self, x: &[Complex32]) {
        let keep = self.keep_from().max(self.base);
        let drop = (keep - self.base) as usize;
        if drop > 0 && (drop >= self.blk as usize * 8 || self.fifo.len() + x.len() > self.cap) {
            self.fifo.drain(..drop.min(self.fifo.len()));
            self.base = keep;
        }
        if self.fifo.len() + x.len() > self.cap {
            let open = self.seg.is_some_and(|s| s.kind != Kind::Rejected);
            self.dropped += u64::from(open) + u64::from(self.pending.is_some());
            let end = self.end_pos();
            self.fifo.clear();
            self.base = end;
            self.det = end;
            self.prev_p = None;
            self.seg = None;
            self.pending = None;
        }
        self.fifo.extend_from_slice(x);
    }

    /// First slot at or after `lo`, before `hi`, where two consecutive slots exceed `thr`.
    fn rise_at(&self, lo: u64, hi: u64, thr: f64) -> Option<u64> {
        let mut s = lo;
        let mut prev = false;
        while s + self.slot <= hi {
            let up = self.slot_power(s) > thr;
            if up && prev {
                return Some(s - self.slot);
            }
            prev = up;
            s += self.slot;
        }
        None
    }

    /// End of the last slot after `lo`, before `hi`, where two consecutive slots exceed `thr`.
    fn fall_at(&self, lo: u64, hi: u64, thr: f64) -> Option<u64> {
        let mut s = hi.saturating_sub(self.slot);
        let mut next = false;
        while s >= lo && s + self.slot <= hi {
            let up = self.slot_power(s) > thr;
            if up && next {
                return Some(s + 2 * self.slot);
            }
            next = up;
            if s < self.slot {
                break;
            }
            s -= self.slot;
        }
        None
    }

    /// Opens a candidate at `start`. Its level comes from the first block after the rise (the
    /// detection window only partly covers the burst); `level` stands in until then.
    fn open(&mut self, start: u64, level: f64) {
        self.seg = Some(Seg {
            start,
            kind: Kind::Collecting,
            from_restart: false,
            level,
            blocks: 0,
            low_run: 0,
            end: None,
        });
    }

    /// Closes the open candidate and opens the pending one, if any.
    fn close(&mut self) {
        if let Some(e) = self.seg.and_then(|s| s.end) {
            self.last_end = e;
        }
        self.seg = None;
        if let Some((s, l)) = self.pending.take() {
            self.open(s, l);
        }
    }

    /// Runs the energy detector over one block. Returns false when the block is not yet in.
    fn detect(&mut self) -> bool {
        let b0 = self.det;
        if b0 + self.blk > self.end_pos() {
            return false;
        }
        let p = (b0..b0 + self.blk)
            .map(|i| f64::from(self.at(i).norm_sqr()))
            .sum::<f64>()
            / self.blk as f64;
        let s = self.prev_p.map_or(p, |q| 0.5 * (p + q));
        self.prev_p = Some(p);
        self.det = b0 + self.blk;
        let hi = self.det;
        let look = |lo: u64| b0.saturating_sub(3 * self.blk).max(lo).max(self.last_end);
        match self.seg {
            None => match self.floor {
                None => self.floor = Some(s),
                Some(f) if s > f * db(ON_DB) => {
                    let lo = look(self.base);
                    let thr = f * db(EDGE_DB);
                    let start = self
                        .rise_at(lo, hi, thr)
                        .map_or(b0.saturating_sub(self.blk), |r| {
                            r.saturating_sub(MARGIN_SLOTS * self.slot)
                        })
                        .max(lo);
                    self.open(start, s);
                }
                Some(f) => {
                    let a = if s < f { 0.5 } else { 1.0 / 32.0 };
                    self.floor = Some(f + (s - f) * a);
                }
            },
            Some(mut seg) => {
                if seg.blocks == 0 {
                    seg.level = s;
                }
                let thr = self
                    .floor
                    .map_or(seg.level / db(ON_DB), |f| (seg.level * f).sqrt());
                if seg.blocks >= 2 && s > seg.level * db(ON_DB) {
                    // A stronger burst: this one ends where that one starts.
                    let lo = look(seg.start + 1);
                    let rthr = (seg.level * s).sqrt();
                    let start = self
                        .rise_at(lo, hi, rthr)
                        .map_or(b0.saturating_sub(self.blk), |r| {
                            r.saturating_sub(MARGIN_SLOTS * self.slot)
                        })
                        .max(lo);
                    seg.end = Some(start);
                    self.pending = Some((start, s));
                } else if s < thr {
                    seg.low_run += 1;
                    if seg.low_run >= LOW_BLOCKS {
                        let lo = b0.saturating_sub(4 * self.blk).max(seg.start);
                        let edge = self
                            .floor
                            .map_or(seg.level / db(ON_DB), |f| f * db(EDGE_DB));
                        let end = self
                            .fall_at(lo, hi, edge)
                            .map_or(lo, |e| (e + MARGIN_SLOTS * self.slot).min(hi));
                        seg.end = Some(end.max(seg.start));
                    }
                } else {
                    seg.low_run = 0;
                    // A block well above the level is a burst ramping in, not the level: left
                    // out, so the rise it leads to is still seen.
                    if s < seg.level * db(EDGE_DB) {
                        let n = f64::from(seg.blocks.min(16)) + 1.0;
                        seg.level += (s - seg.level) / n;
                    }
                }
                seg.blocks += 1;
                self.seg = Some(seg);
            }
        }
        true
    }
}

// ------------------------------------------------------------------------ the block's driver

impl Psk {
    /// Burst-mode `process` body: the resampled input is in `self.res`. `end`: the input's
    /// last chunk (`END`), so a burst still open is closed at the data's end and flushed.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn run_burst(
        &mut self,
        b: &mut Burst,
        tracker: &mut Tracker,
        soft: &mut Vec<f32>,
        sym_diag: &mut Vec<Complex32>,
        te_diag: &mut Vec<f32>,
        tapped: bool,
        end: bool,
    ) -> CallOut {
        b.push(&self.res);
        if end {
            self.flush_rate(b);
        }
        let mut call = CallOut::default();
        let mut out = (soft, sym_diag, te_diag);
        self.pump(b, tracker, &mut out, tapped, &mut call);
        if end {
            // Close what is open at the data's end and run it, and whatever that uncovers,
            // until a burst has to wait for a chunk that will not come.
            while let Some(mut seg) = b.seg.filter(|s| s.end.is_none()) {
                seg.end = Some(b.end_pos().max(seg.start));
                b.seg = Some(seg);
                self.pump(b, tracker, &mut out, tapped, &mut call);
            }
            // What one output chunk cannot carry is lost with the stream: counted.
            let open = b.seg.is_some_and(|s| s.kind != Kind::Rejected);
            b.dropped += u64::from(open) + u64::from(b.pending.is_some());
            b.seg = None;
            b.pending = None;
        }
        if b.seg.is_none_or(|s| s.kind != Kind::Tracking) {
            self.locked = false;
        }
        call
    }

    /// At the stream's end, pushes the resampler's delayed tail out with zeros, so a burst that
    /// runs to the end of a capture keeps its last symbols.
    fn flush_rate(&mut self, b: &mut Burst) {
        let Some(r) = &mut self.rate else { return };
        let Some(span) = r.kernel.as_ref().map(|k| k.span_samples()) else {
            return;
        };
        let cap = r.scratch_in.capacity().max(1);
        let mut left = span + 1;
        while left > 0 {
            let n = left.min(cap);
            r.scratch_in.clear();
            r.scratch_in.resize(n, Complex32::new(0.0, 0.0));
            r.run();
            b.push(&r.scratch_out);
            left -= n;
        }
    }

    /// Runs the detector and the open candidate as far as the FIFO and the one-burst-per-chunk
    /// rule allow.
    fn pump(
        &mut self,
        b: &mut Burst,
        tracker: &mut Tracker,
        out: &mut Outs<'_>,
        tapped: bool,
        call: &mut CallOut,
    ) {
        loop {
            if let Some(seg) = b.seg {
                match seg.kind {
                    Kind::Collecting => {
                        let ready = seg.end.is_some() || b.det >= seg.start + b.acq_len;
                        if ready {
                            if call.first_centre.is_some() {
                                // One burst per output chunk: this one opens the next.
                                return;
                            }
                            self.acquire(b, tracker);
                            continue;
                        }
                    }
                    Kind::Tracking => {
                        let limit = seg.end.unwrap_or(b.det.saturating_sub(b.lookahead));
                        self.feed(b, tracker, limit, out, tapped, call);
                        if seg.end.is_some() {
                            self.flush(b, tracker, out, tapped, call);
                            self.locked = false;
                            b.close();
                            continue;
                        }
                    }
                    Kind::Rejected => {
                        if seg.end.is_some() {
                            b.close();
                            continue;
                        }
                    }
                }
            }
            if !b.detect() {
                return;
            }
        }
    }

    /// Acquires the open candidate over its own samples, or rejects it.
    fn acquire(&mut self, b: &mut Burst, tracker: &mut Tracker) {
        let Some(mut seg) = b.seg else { return };
        let a = seg.start;
        let e = seg.end.unwrap_or(u64::MAX).min(a + b.acq_len);
        let reject = |b: &mut Burst, mut seg: Seg| {
            if seg.from_restart {
                // The stream began on noise (or on something that is not PSK): its level is
                // the floor.
                b.floor = Some(seg.level.max(f64::MIN_POSITIVE));
                b.seg = None;
                if let Some((s, l)) = b.pending.take() {
                    b.open(s, l);
                }
            } else {
                seg.kind = Kind::Rejected;
                b.seg = Some(seg);
            }
        };
        if e <= a || e - a < b.min_len {
            reject(b, seg);
            return;
        }
        let lo = (a - b.base) as usize;
        let hi = (e - b.base) as usize;
        let fit = b.acq.fit(&b.fifo[lo..hi]);
        if seg.from_restart && seg.blocks == 0 {
            seg.level = fit.power;
        }
        let need = if seg.from_restart {
            fit.threshold(MARGIN_RESTART)
        } else {
            fit.threshold(MARGIN_RISE).min(RISE_ACCEPT)
        };
        if fit.coherence < need || fit.power <= 0.0 {
            reject(b, seg);
            return;
        }
        // Feed from s0 + mu so that liquid's strobes, at (j − LIQUID_DELAY)·k after the feed
        // start, land on the estimated centres a + tau + n·k. OQPSK's own interpolator is
        // seeded instead.
        let k = self.k;
        let native = matches!(tracker, Tracker::Oqpsk(_));
        let lead = if native {
            0.0
        } else {
            fit.tau.rem_euclid(k) - k
        };
        let feed0 = a as f64 + lead;
        let feed0 = if feed0 < 0.0 { feed0 + k } else { feed0 };
        let s0 = feed0.floor();
        let mu = feed0 - s0;
        b.taps = frac_taps(mu);
        b.feed_pos = s0 as u64;
        b.w = fit.w;
        // De-rotation phase at the feed start, from the phase at the window centre.
        let centre = a as f64 + (e - a - 1) as f64 / 2.0;
        let theta0 = self.track_table.points[0].arg();
        b.nco = match fit.phase {
            Some(ph) if !native => ph - theta0 - fit.w * (centre - feed0),
            Some(ph) => ph - fit.w * (centre - feed0),
            None => 0.0,
        };
        b.gain = (1.0 / fit.power.sqrt()) as f32;
        match tracker {
            Tracker::Liquid(t) => {
                t.reset(&mut self.zeros, &mut self.syms);
                b.first_centre = feed0 - LIQUID_DELAY * k;
            }
            Tracker::Oqpsk(o) => {
                o.restart();
                // I strobe on the first estimated centre. The feed starts in the margin before
                // the burst, so the matched filter's zero history is the (quiet) pre-burst; the
                // interpolator needs only one sample behind the strobe.
                let mut first = fit.tau + (a as f64 - feed0);
                while first + o.mf_delay < 1.0 {
                    first += k;
                }
                o.next = first + o.mf_delay;
                b.first_centre = feed0 + first;
            }
        }
        b.feed_origin = feed0;
        b.track_syms = 0;
        b.emitted = false;
        b.bursts += 1;
        // Fresh symbol statistics for this burst.
        self.prev_raw = None;
        self.prev_decision = None;
        self.power = 0.0;
        self.evm = 0.0;
        self.symbols = 0;
        self.locked = false;
        seg.kind = Kind::Tracking;
        b.seg = Some(seg);
    }

    /// Feeds the tracker up to resampled position `limit` and emits the symbols inside the burst.
    fn feed(
        &mut self,
        b: &mut Burst,
        tracker: &mut Tracker,
        limit: u64,
        out: &mut Outs<'_>,
        tapped: bool,
        call: &mut CallOut,
    ) {
        let Some(seg) = b.seg else { return };
        while b.feed_pos < limit {
            let n = ((limit - b.feed_pos) as usize).min(b.feed.capacity());
            b.feed.clear();
            for i in 0..n as u64 {
                let p = b.feed_pos + i;
                let mut y = Complex32::new(0.0, 0.0);
                for (t, &h) in b.taps.iter().enumerate() {
                    let j = (p + t as u64).checked_sub(FRAC_HALF as u64 - 1);
                    if let Some(j) = j {
                        y += b.at(j) * h;
                    }
                }
                let (s, c) = (-b.nco).sin_cos();
                let y = Complex64::new(f64::from(y.re), f64::from(y.im)) * Complex64::new(c, s);
                b.nco = (b.nco + b.w).rem_euclid(TAU);
                b.feed
                    .push(Complex32::new(y.re as f32, y.im as f32) * b.gain);
            }
            b.feed_pos += n as u64;
            self.track(b, tracker, seg, out, tapped, call);
        }
    }

    /// Pushes zeros through the tracker until every symbol before the burst's end is out.
    fn flush(
        &mut self,
        b: &mut Burst,
        tracker: &mut Tracker,
        out: &mut Outs<'_>,
        tapped: bool,
        call: &mut CallOut,
    ) {
        let Some(seg) = b.seg else { return };
        let delay = match tracker {
            Tracker::Liquid(_) => LIQUID_DELAY,
            Tracker::Oqpsk(o) => o.mf_delay / self.k + 1.0,
        };
        let mut left = ((delay + 2.0) * self.k).ceil() as usize;
        while left > 0 {
            let n = left.min(b.feed.capacity());
            b.feed.clear();
            b.feed.resize(n, Complex32::new(0.0, 0.0));
            left -= n;
            self.track(b, tracker, seg, out, tapped, call);
        }
    }

    /// Runs `b.feed` through the tracker, emitting symbols centred inside `seg`.
    #[allow(clippy::too_many_arguments)]
    fn track(
        &mut self,
        b: &mut Burst,
        tracker: &mut Tracker,
        seg: Seg,
        out: &mut Outs<'_>,
        tapped: bool,
        call: &mut CallOut,
    ) {
        let (soft, sym_diag, te_diag) = (&mut *out.0, &mut *out.1, &mut *out.2);
        let lo = seg.start as f64;
        let hi = seg.end.map_or(f64::INFINITY, |e| e as f64);
        let k = self.k;
        // `at`: the symbol's own strobe (OQPSK's interpolator knows it), else its count.
        let mut emit = |this: &mut Self, b: &mut Burst, y: Complex64, te: f64, at: Option<f64>| {
            let centre = at.map_or(b.first_centre + b.track_syms as f64 * k, |a| {
                b.feed_origin + a
            });
            b.track_syms += 1;
            if centre < lo || centre >= hi {
                return;
            }
            if call.first_centre.is_none() {
                call.first_centre = Some(centre);
                call.burst_start = !b.emitted;
            }
            b.emitted = true;
            let diag = tapped.then_some((&mut *sym_diag, &mut *te_diag, te));
            this.symbol(y, soft, diag);
        };
        match tracker {
            Tracker::Liquid(t) => {
                let n = t.execute(&mut b.feed, &mut self.syms);
                for j in 0..n {
                    let s = self.syms[j];
                    let y = Complex64::new(f64::from(s.re), f64::from(s.im));
                    emit(self, b, y, 0.0, None);
                }
            }
            Tracker::Oqpsk(o) => {
                self.oq_out.clear();
                let out = &mut self.oq_out;
                for &s in &b.feed {
                    o.push(s, |sym| out.push(sym));
                }
                let syms = std::mem::take(&mut self.oq_out);
                for s in &syms {
                    emit(self, b, s.point, s.timing_error, Some(s.at));
                }
                self.oq_out = syms;
            }
        }
    }
}
