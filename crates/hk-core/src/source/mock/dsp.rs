//! Signal model of the mock SDR ([`super`]): band selection, frequency shift and arbitrary-ratio
//! resampling of the recorded IQ, calibrated noise fill for uncovered spectrum, and the floor
//! estimate that calibrates it. Plain Rust, no allocation per sample once warm.
//!
//! One output sample at stream time `t` (in recording samples) is
//!
//! ```text
//! y(t) = e^{j2π (c − δ) t/fs_rec} · Σ_m x[m] e^{−j2π c m/fs_rec} · h(t − m)  +  noise
//! ```
//!
//! with `δ` the retune offset from the recording centre, `c` the centre of the recorded band that
//! overlaps the requested window (both Hz), and `h` a Kaiser-windowed sinc low-pass (≈ 60 dB) of
//! half the overlap width, tabulated in polyphase form. The input mix centres the overlap at DC,
//! the low-pass selects it (so nothing outside the recording wraps in), and the output mix moves
//! it to its offset from the new centre: absolute frequencies are preserved. Uncovered spectrum
//! gets complex white Gaussian noise at the recording's floor PSD, band-stopped by the complement
//! of the same pass band (`noise[n − D] − (h_band * noise)[n]`), so the floor stays level across
//! the coverage edge apart from a ≈ 3 dB dip over the transition band.

use std::sync::Arc;

use num_complex::Complex32;

use crate::source::SourceError;

/// Appends recording samples to a buffer.
pub(crate) trait Feed {
    /// Appends at least one sample to `dst`, or returns `false` at the end of the input.
    fn fill(&mut self, dst: &mut Vec<Complex32>) -> Result<bool, SourceError>;
}

/// Deterministic xorshift64* noise with Box–Muller Gaussians.
#[derive(Clone, Debug)]
pub(crate) struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn uniform(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let v = self.0.wrapping_mul(0x2545_f491_4f6c_dd1d);
        ((v >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    /// Complex Gaussian with `E|z|² = variance`.
    pub fn complex_gaussian(&mut self, variance: f64) -> Complex32 {
        let (u1, u2) = (self.uniform(), self.uniform());
        let r = (-variance * u1.ln()).sqrt();
        let a = std::f64::consts::TAU * u2;
        Complex32::new((r * a.cos()) as f32, (r * a.sin()) as f32)
    }
}

/// Modified Bessel function of the first kind, order 0 (series).
fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, mut k) = (1.0, 1.0, 1.0);
    while term > 1e-12 * sum {
        term *= (x / (2.0 * k)).powi(2);
        sum += term;
        k += 1.0;
    }
    sum
}

/// Kaiser β for ≈ 60 dB stop-band attenuation.
const KAISER_BETA: f64 = 5.65;

fn kaiser(x: f64) -> f64 {
    if x.abs() >= 1.0 {
        return 0.0;
    }
    bessel_i0(KAISER_BETA * (1.0 - x * x).sqrt()) / bessel_i0(KAISER_BETA)
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        let p = std::f64::consts::PI * x;
        p.sin() / p
    }
}

/// Taps each side for a Kaiser low-pass with normalised transition width `width` (cycles/sample).
fn half_taps(width: f64) -> usize {
    ((1.81 / width.max(1e-6)).ceil() as usize).clamp(2, 8192)
}

/// Polyphase windowed-sinc low-pass: one row of `2·half` taps per fractional delay phase, each
/// row normalised to unit DC gain.
#[derive(Clone, Debug)]
struct Kernel {
    half: usize,
    phases: usize,
    taps: Vec<f32>,
}

impl Kernel {
    /// Cut-off `fc` (normalised, ≤ 0.5), transition `width`.
    fn new(fc: f64, width: f64) -> Self {
        let fc = fc.clamp(1e-6, 0.5);
        let half = half_taps(width);
        let phases = ((1024.0 * 2.0 * fc).ceil() as usize)
            .next_power_of_two()
            .clamp(16, 1024);
        let n = 2 * half;
        let mut taps = vec![0f32; phases * n];
        for p in 0..phases {
            let frac = p as f64 / phases as f64;
            let row = &mut taps[p * n..(p + 1) * n];
            let mut sum = 0.0;
            let vals: Vec<f64> = (0..n)
                .map(|j| {
                    let tau = frac + half as f64 - 1.0 - j as f64;
                    let v = 2.0 * fc * sinc(2.0 * fc * tau) * kaiser(tau / half as f64);
                    sum += v;
                    v
                })
                .collect();
            for (t, v) in row.iter_mut().zip(vals) {
                *t = (v / sum) as f32;
            }
        }
        Self { half, phases, taps }
    }

    /// The phase row for fractional delay `frac` in [0, 1) and whether it rounded up to the next
    /// integer sample.
    fn phase(&self, frac: f64) -> (usize, bool) {
        let p = (frac * self.phases as f64).round() as usize;
        if p >= self.phases {
            (0, true)
        } else {
            (p, false)
        }
    }

    /// The taps of phase row `p`.
    fn row(&self, p: usize) -> &[f32] {
        let n = 2 * self.half;
        &self.taps[p * n..(p + 1) * n]
    }
}

/// Band-stop complement of a complex band-pass FIR, run on generated noise.
#[derive(Clone, Debug)]
struct ComplementNoise {
    taps: Vec<Complex32>,
    ring: Vec<Complex32>,
    at: usize,
}

impl ComplementNoise {
    /// Pass band `[lo, hi]`, normalised to the output rate.
    fn new(lo: f64, hi: f64, width: f64) -> Self {
        let half = half_taps(width);
        let n = 2 * half + 1;
        let fc = ((hi - lo) / 2.0).clamp(1e-6, 0.5);
        let mid = (hi + lo) / 2.0;
        let raw: Vec<f64> = (0..n)
            .map(|j| {
                let tau = j as f64 - half as f64;
                2.0 * fc * sinc(2.0 * fc * tau) * kaiser(tau / (half as f64 + 1.0))
            })
            .collect();
        let sum: f64 = raw.iter().sum();
        let taps = raw
            .iter()
            .enumerate()
            .map(|(j, v)| {
                let ph = std::f64::consts::TAU * mid * (j as f64 - half as f64);
                Complex32::new((v / sum * ph.cos()) as f32, (v / sum * ph.sin()) as f32)
            })
            .collect();
        Self {
            taps,
            ring: vec![Complex32::new(0.0, 0.0); n],
            at: 0,
        }
    }

    fn next(&mut self, z: Complex32) -> Complex32 {
        let n = self.ring.len();
        self.ring[self.at] = z;
        let mut acc = Complex32::new(0.0, 0.0);
        for (j, h) in self.taps.iter().enumerate() {
            acc += h * self.ring[(self.at + n - j) % n];
        }
        let delayed = self.ring[(self.at + n - n / 2) % n];
        self.at = (self.at + 1) % n;
        delayed - acc
    }
}

/// What a window serves, relative to the recording (Hz).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Plan {
    /// Recording centre and rate.
    pub rec_center_hz: f64,
    pub rec_rate_hz: f64,
    /// Recorded bandwidth a retuned window may use (the recording's baseband filter, ≤ its rate).
    pub rec_usable_hz: f64,
    /// Requested centre and rate.
    pub center_hz: f64,
    pub rate_hz: f64,
    /// Floor power of the recording, full-scale² per sample at the recording rate.
    pub floor_power: f64,
    /// Rounding noise of the recording's sample format, full-scale² per sample at the recording
    /// rate (part of `floor_power`; 0 for float recordings). The device's own output rounding adds
    /// it back at the tuned rate, so the noise fill leaves it out ([`Dequant`]).
    pub quant_power: f64,
    /// Transition width as a fraction of the output rate.
    pub transition: f64,
}

/// Coverage of a window by the recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Coverage {
    /// The whole window lies inside the recorded band.
    Recorded,
    /// Part of the window is recorded; the rest is calibrated noise.
    Partial,
    /// Nothing recorded overlaps the window: calibrated noise only.
    Noise,
}

impl Plan {
    /// The recorded band served in the window, `(lo, hi)` absolute Hz, if any: the whole recording
    /// when tuned exactly as recorded, else its usable (filter) bandwidth.
    pub fn overlap(&self) -> Option<(f64, f64)> {
        let half = if self.passthrough() {
            self.rec_rate_hz / 2.0
        } else {
            self.rec_usable_hz.min(self.rec_rate_hz) / 2.0
        };
        let lo = (self.rec_center_hz - half).max(self.center_hz - self.rate_hz / 2.0);
        let hi = (self.rec_center_hz + half).min(self.center_hz + self.rate_hz / 2.0);
        (hi - lo > 1.0).then_some((lo, hi))
    }

    /// How much of the window is recorded (1 Hz tolerance).
    pub fn coverage(&self) -> Coverage {
        match self.overlap() {
            None => Coverage::Noise,
            Some((lo, hi)) if hi - lo >= self.rate_hz - 1.0 => Coverage::Recorded,
            Some(_) => Coverage::Partial,
        }
    }

    fn passthrough(&self) -> bool {
        self.center_hz == self.rec_center_hz && self.rate_hz == self.rec_rate_hz
    }
}

/// Streaming renderer of one [`Plan`] (retargetable without losing stream time).
pub(crate) struct Render {
    plan: Plan,
    /// Recording samples per output sample.
    ratio: f64,
    kernel: Option<Kernel>,
    half: usize,
    /// Stream time of the next output sample, in recording samples: `pos0 + k · ratio`.
    pos0: f64,
    k: u64,
    /// Recording samples from absolute index `base` (at `hist[start]`).
    hist: Vec<Complex32>,
    mixed: Vec<Complex32>,
    start: usize,
    base: u64,
    /// Input-mix frequency (cycles per recording sample) and its reference index.
    in_step: f64,
    m0: u64,
    out_phase: f64,
    out_step: f64,
    noise: Option<ComplementNoise>,
    noise_only: bool,
    noise_var: f64,
    rng: Rng,
    /// The recording's rounding-noise remover, applied to rendered (not passed-through) IQ.
    dequant: Option<DequantState>,
    /// Look-ahead of `dequant` in force (0 when passing through or without one).
    dq_delay: usize,
}

impl Render {
    pub fn new(plan: Plan, seed: u64, dequant: Option<Arc<Dequant>>) -> Self {
        let mut r = Self {
            dequant: dequant.map(DequantState::new),
            dq_delay: 0,
            plan,
            ratio: 1.0,
            kernel: None,
            half: 1,
            pos0: 0.0,
            k: 0,
            hist: Vec::new(),
            mixed: Vec::new(),
            start: 0,
            base: 0,
            in_step: 0.0,
            m0: 0,
            out_phase: 0.0,
            out_step: 0.0,
            noise: None,
            noise_only: false,
            noise_var: 0.0,
            rng: Rng::new(seed),
        };
        r.retarget(plan);
        r
    }

    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// Stream time of the next output sample, as a recording-sample index.
    pub fn pos(&self) -> f64 {
        self.pos0 + self.k as f64 * self.ratio
    }

    /// Switches to `plan` at the current stream time.
    pub fn retarget(&mut self, plan: Plan) {
        let pos = self.pos();
        self.plan = plan;
        self.pos0 = pos;
        self.k = 0;
        self.ratio = plan.rec_rate_hz / plan.rate_hz;
        // T-141: the fill is the floor without the recording's rounding noise; the output rounding
        // adds the device's own, as a radio's ADC would.
        self.noise_var =
            (plan.floor_power - plan.quant_power).max(0.0) * plan.rate_hz / plan.rec_rate_hz;
        self.noise_only = plan.coverage() == Coverage::Noise;
        let width = (plan.transition * plan.rate_hz / plan.rec_rate_hz).min(0.25);
        let passthrough = plan.passthrough() && pos.fract() == 0.0;
        self.dq_delay = 0;
        match plan.overlap() {
            Some((lo, hi)) if !passthrough => {
                let c = 0.5 * (lo + hi) - plan.rec_center_hz;
                let kernel = Kernel::new((hi - lo) / 2.0 / plan.rec_rate_hz, width);
                self.half = kernel.half;
                self.kernel = Some(kernel);
                self.dq_delay = if self.dequant.is_some() { DEQUANT_N } else { 0 };
                self.in_step = c / plan.rec_rate_hz;
                let delta = plan.center_hz - plan.rec_center_hz;
                self.out_step = (c - delta) / plan.rate_hz;
                self.m0 = self.base;
                self.out_phase = (self.in_step * (pos - self.m0 as f64)).rem_euclid(1.0);
            }
            _ => {
                self.kernel = None;
                self.half = 1;
                self.in_step = 0.0;
                self.out_step = 0.0;
                self.out_phase = 0.0;
            }
        }
        self.noise = match plan.coverage() {
            Coverage::Partial => plan.overlap().map(|(lo, hi)| {
                ComplementNoise::new(
                    (lo - plan.center_hz) / plan.rate_hz,
                    (hi - plan.center_hz) / plan.rate_hz,
                    plan.transition,
                )
            }),
            _ => None,
        };
        // Re-mix the history under the new input mix.
        self.mixed.truncate(self.start);
    }

    /// Skips `n` output samples: stream time advances, their input is never convolved.
    pub fn skip(&mut self, n: u64) {
        self.k += n;
        self.out_phase = (self.out_phase + self.out_step * n as f64).rem_euclid(1.0);
    }

    /// Makes recording samples `lo..=hi` available; `false` if the input ends first.
    fn ensure(&mut self, feed: &mut dyn Feed, lo: u64, hi: u64) -> Result<bool, SourceError> {
        // Drop what lies before `lo` (less the dequantiser's look-back).
        let d = self.dq_delay;
        let keep = lo.saturating_sub(d as u64);
        let avail = (self.hist.len() - self.start) as u64;
        let drop = keep.saturating_sub(self.base).min(avail);
        self.start += drop as usize;
        self.base += drop;
        if self.start > 65_536 && self.start > self.hist.len() / 2 {
            self.hist.drain(..self.start);
            let m = self.start.min(self.mixed.len());
            self.mixed.drain(..m);
            if let Some(dq) = self.dequant.as_mut() {
                let c = self.start.min(dq.clean.len());
                dq.clean.drain(..c);
            }
            self.start = 0;
        }
        while self.base < keep {
            // The history is empty and the window starts further on: discard input.
            let mut scratch = std::mem::take(&mut self.hist);
            scratch.clear();
            if !feed.fill(&mut scratch)? {
                self.hist = scratch;
                return Ok(false);
            }
            let skip = ((keep - self.base) as usize).min(scratch.len());
            self.base += skip as u64;
            scratch.drain(..skip);
            self.hist = scratch;
            self.mixed.clear();
            if let Some(dq) = self.dequant.as_mut() {
                dq.clean.clear();
            }
            self.start = 0;
        }
        while self.base + ((self.hist.len() - self.start) as u64) <= hi + d as u64 {
            if !feed.fill(&mut self.hist)? {
                return Ok(false);
            }
        }
        // Mix new samples (dequantised first, when rendering with a dequantiser).
        if self.kernel.is_some() {
            if self.mixed.len() < self.start {
                self.mixed.resize(self.start, Complex32::new(0.0, 0.0));
            }
            let first = self.mixed.len();
            let dq = match self.dequant.as_mut() {
                Some(dq) if d > 0 => {
                    let done = dq.advance(&self.hist, self.start, self.base);
                    Some((&dq.clean, done))
                }
                _ => None,
            };
            let end = dq.map_or(self.hist.len(), |(_, done)| done).max(first);
            let idx0 = self.base + (first - self.start) as u64;
            let mut phase = (-self.in_step * (idx0 as f64 - self.m0 as f64)).rem_euclid(1.0);
            for i in first..end {
                let z = match dq {
                    Some((clean, _)) => clean[i],
                    None => self.hist[i],
                };
                let a = std::f64::consts::TAU * phase;
                self.mixed
                    .push(z * Complex32::new(a.cos() as f32, a.sin() as f32));
                phase -= self.in_step;
                if !(0.0..1.0).contains(&phase) {
                    phase = phase.rem_euclid(1.0);
                }
            }
        }
        Ok(true)
    }

    /// Appends up to `n` output samples to `out` (normalised full scale, before gain); fewer only
    /// when the input ends.
    pub fn render(
        &mut self,
        feed: &mut dyn Feed,
        out: &mut Vec<Complex32>,
        n: usize,
    ) -> Result<usize, SourceError> {
        let mut made = 0;
        while made < n {
            let pos = self.pos();
            let mut i0 = pos.floor() as u64;
            let frac = pos - i0 as f64;
            let mut y = if self.noise_only {
                // Stream time still consumes the recording, as a radio's would.
                if !self.ensure(feed, i0, i0)? {
                    break;
                }
                Complex32::new(0.0, 0.0)
            } else if let Some((p, carry)) = self.kernel.as_ref().map(|k| k.phase(frac)) {
                if carry {
                    i0 += 1;
                }
                let lo = (i0 + 1).saturating_sub(self.half as u64);
                let hi = i0 + self.half as u64;
                if !self.ensure(feed, lo, hi)? {
                    break;
                }
                // Taps before index 0 (stream start) see silence.
                let skip = (self.half as u64 - 1).saturating_sub(i0) as usize;
                let at = self.start + (lo - self.base) as usize;
                let row = self.kernel.as_ref().map_or(&[][..], |k| k.row(p));
                let mut acc = Complex32::new(0.0, 0.0);
                for (h, z) in row[skip..].iter().zip(&self.mixed[at..]) {
                    acc += z * *h;
                }
                let a = std::f64::consts::TAU * self.out_phase;
                acc * Complex32::new(a.cos() as f32, a.sin() as f32)
            } else {
                if !self.ensure(feed, i0, i0)? {
                    break;
                }
                self.hist[self.start + (i0 - self.base) as usize]
            };
            if self.noise_only {
                y = self.rng.complex_gaussian(self.noise_var);
            } else if let Some(noise) = self.noise.as_mut() {
                y += noise.next(self.rng.complex_gaussian(self.noise_var));
            }
            out.push(y);
            self.out_phase = (self.out_phase + self.out_step).rem_euclid(1.0);
            self.k += 1;
            made += 1;
        }
        Ok(made)
    }
}

/// In-place radix-2 FFT (length a power of two).
fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -std::f64::consts::TAU / len as f64;
        let (wr, wi) = (ang.cos(), ang.sin());
        for s in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0, 0.0);
            for k in 0..len / 2 {
                let (a, b) = (s + k, s + k + len / 2);
                let tr = re[b] * cr - im[b] * ci;
                let ti = re[b] * ci + im[b] * cr;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
        }
        len <<= 1;
    }
}

/// Welch periodogram of `x` (Hann, 1024 bins, up to 64 segments), FFT bin order, each bin scaled
/// to the full-band power per sample a white spectrum at its level would have (full scale²), and
/// the segment count. `None` with fewer than 64 samples.
fn welch_bins(x: &[Complex32]) -> Option<(Vec<f64>, usize)> {
    let n = 1024.min(x.len().checked_next_power_of_two()? / 2).max(64);
    if x.len() < n {
        return None;
    }
    let segs = (x.len() / n).min(64);
    let w: Vec<f64> = (0..n)
        .map(|i| 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / n as f64).cos())
        .collect();
    let w2: f64 = w.iter().map(|v| v * v).sum();
    let mut avg = vec![0f64; n];
    let (mut re, mut im) = (vec![0f64; n], vec![0f64; n]);
    for s in 0..segs {
        for i in 0..n {
            let z = x[s * n + i];
            re[i] = f64::from(z.re) * w[i];
            im[i] = f64::from(z.im) * w[i];
        }
        fft(&mut re, &mut im);
        for i in 0..n {
            avg[i] += (re[i] * re[i] + im[i] * im[i]) / segs as f64;
        }
    }
    for v in &mut avg {
        *v /= w2;
    }
    Some((avg, segs))
}

/// The recording's noise floor as full-band power per sample (full scale²): Welch periodogram
/// ([`welch_bins`]), median of the central half of the band without the DC bins, so strong
/// emissions and the anti-alias roll-off at the edges do not bias it. `None` with fewer than 64
/// samples.
pub(crate) fn estimate_floor_power(x: &[Complex32]) -> Option<f64> {
    let (bins, segs) = welch_bins(x)?;
    let n = bins.len();
    // Central half: bins within ±n/4 of DC, excluding ±2 around DC.
    let mut central: Vec<f64> = (0..n)
        .filter(|&i| {
            let k = if i < n / 2 {
                i as i64
            } else {
                i as i64 - n as i64
            };
            k.unsigned_abs() > 2 && k.unsigned_abs() < (n / 4) as u64
        })
        .map(|i| bins[i])
        .collect();
    central.sort_by(f64::total_cmp);
    let median = central[central.len() / 2];
    // The median of a K-average of exponentials is ≈ (1 − 1/(3K)) of the mean.
    let bias = 1.0 - 1.0 / (3.0 * segs as f64);
    Some(median / bias)
}

/// STFT length of the dequantiser (bins; periodic sqrt-Hann analysis and synthesis, hop `N/2`).
/// It is also the look-ahead, in recording samples, that rendering with it needs.
const DEQUANT_N: usize = 1024;
/// Weight of each new frame in a bin's level estimate (≈ 15 frames of memory).
const DEQUANT_ALPHA: f64 = 1.0 / 8.0;
/// Bins either side averaged into a bin's level estimate.
const DEQUANT_SPREAD: usize = 2;
/// Floor of the power gain where the recording holds hardly more than rounding noise.
const DEQUANT_MIN_GAIN2: f64 = 0.05;

/// T-141: what the mock needs to take a recording's own rounding noise out of IQ it re-renders.
///
/// Recorded IQ is `RF + q_rec`, with `q_rec` the capture's rounding noise (white, `quant_power`
/// per sample). A retuned or resampled window is filtered and rounded to int8 again, which adds
/// the device's rounding noise a second time: a quantisation-limited recording's floor read
/// ≈ 1 dB high after a retune at its own rate (6 dB when rounding noise dominates). The
/// dequantiser ([`DequantState`]) is a short-time spectral subtraction: each STFT bin gets the
/// power gain `G² = 1 − quant_power / P̂`, `P̂` the bin's level (±[`DEQUANT_SPREAD`] bins) averaged
/// over *past* frames only. A floor bin (`P̂ = A + q`) serves `A`, and a bin holding an emission
/// (`S + A + q`) serves `S + A`: the rounding noise comes out, the emission's power stays, and
/// the level estimate follows emissions that start or stop within ≈ 15 frames. The served PSD
/// `G²·P + q_out` then equals the recording's where the output is rounded at the recording's
/// rate and gain, and a wider rate or higher gain shows the smaller rounding density a radio
/// would.
#[derive(Debug)]
pub(crate) struct Dequant {
    quant_power: f64,
    /// Level estimate to start from (Welch of the recording's first samples, FFT bin order).
    initial: Vec<f64>,
}

impl Dequant {
    /// The dequantiser for a recording whose samples start `x`, or `None` when its rounding noise
    /// changes no level there by more than ≈ 0.01 dB (or `x` is too short to design from).
    pub fn design(x: &[Complex32], quant_power: f64) -> Option<Self> {
        if quant_power <= 0.0 {
            return None;
        }
        let (bins, _) = welch_bins(x)?;
        if bins.len() != DEQUANT_N || bins.iter().all(|p| quant_power < 2e-3 * p) {
            return None;
        }
        Some(Self {
            quant_power,
            initial: bins,
        })
    }
}

/// Streaming state of a [`Dequant`] over [`Render`]'s history: `clean[i]` is the dequantised
/// `hist[i]`, final before the next frame's start and half-accumulated for `N/2` beyond it.
#[derive(Debug)]
struct DequantState {
    design: Arc<Dequant>,
    window: Vec<f64>,
    level: Vec<f64>,
    gain: Vec<f64>,
    /// Absolute recording index of the next frame's first sample.
    next: i64,
    clean: Vec<Complex32>,
    re: Vec<f64>,
    im: Vec<f64>,
}

impl DequantState {
    fn new(design: Arc<Dequant>) -> Self {
        let n = DEQUANT_N;
        Self {
            window: (0..n)
                .map(|i| (std::f64::consts::PI * i as f64 / n as f64).sin())
                .collect(),
            level: design.initial.clone(),
            gain: vec![1.0; n],
            design,
            next: 0,
            clean: Vec::new(),
            re: vec![0.0; n],
            im: vec![0.0; n],
        }
    }

    /// Processes every frame `hist` (starting at absolute index `base − start`) holds, and returns
    /// how many leading `clean` samples are final. A history that no longer lines up with `clean`
    /// (discarded input, or dropped past the next frame) restarts at `base`, with the frame
    /// straddling it zero-padded before the history.
    fn advance(&mut self, hist: &[Complex32], start: usize, base: u64) -> usize {
        let (n, h) = (DEQUANT_N as i64, DEQUANT_N as i64 / 2);
        let hist0 = base as i64 - start as i64;
        if self.next - hist0 < -h || self.clean.len() as i64 != self.next - hist0 + h {
            self.next = base as i64 - h;
            self.clean.clear();
            self.clean.resize(start, Complex32::new(0.0, 0.0));
        }
        let w2 = n as f64 / 2.0;
        let q = self.design.quant_power;
        let len = DEQUANT_N;
        loop {
            let s = self.next - hist0;
            if s + n > hist.len() as i64 {
                break;
            }
            let mut padded = false;
            for i in 0..len {
                let j = s + i as i64;
                let z = if j >= 0 {
                    hist[j as usize]
                } else {
                    padded = true;
                    Complex32::new(0.0, 0.0)
                };
                self.re[i] = f64::from(z.re) * self.window[i];
                self.im[i] = f64::from(z.im) * self.window[i];
            }
            fft(&mut self.re, &mut self.im);
            // Gains from past frames only, so a bin's gain is independent of its current value.
            let mut acc: f64 = (0..=2 * DEQUANT_SPREAD)
                .map(|o| self.level[(len + o - DEQUANT_SPREAD) % len])
                .sum();
            for k in 0..len {
                let p = acc / (2 * DEQUANT_SPREAD + 1) as f64;
                self.gain[k] = if p > 0.0 {
                    (1.0 - q / p).max(DEQUANT_MIN_GAIN2).sqrt()
                } else {
                    DEQUANT_MIN_GAIN2.sqrt()
                };
                acc += self.level[(k + DEQUANT_SPREAD + 1) % len]
                    - self.level[(k + len - DEQUANT_SPREAD) % len];
            }
            for k in 0..len {
                if !padded {
                    let p = (self.re[k] * self.re[k] + self.im[k] * self.im[k]) / w2;
                    self.level[k] += DEQUANT_ALPHA * (p - self.level[k]);
                }
                // Gain, and conjugate for the inverse transform.
                self.re[k] *= self.gain[k];
                self.im[k] *= -self.gain[k];
            }
            fft(&mut self.re, &mut self.im);
            // Overlap-add: sqrt-Hann² at hop N/2 sums to one.
            for i in 0..len {
                let w = self.window[i] / n as f64;
                let y = Complex32::new((self.re[i] * w) as f32, (-self.im[i] * w) as f32);
                let j = s + i as i64;
                if (i as i64) < h {
                    if j >= 0 {
                        self.clean[j as usize] += y;
                    }
                } else {
                    self.clean.push(y);
                }
            }
            self.next += h;
        }
        (self.next - hist0).max(0) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct VecFeed(Vec<Complex32>, usize);

    impl Feed for VecFeed {
        fn fill(&mut self, dst: &mut Vec<Complex32>) -> Result<bool, SourceError> {
            if self.1 >= self.0.len() {
                return Ok(false);
            }
            let end = (self.1 + 1000).min(self.0.len());
            dst.extend_from_slice(&self.0[self.1..end]);
            self.1 = end;
            Ok(true)
        }
    }

    fn tone(f: f64, fs: f64, n: usize, amp: f32) -> Vec<Complex32> {
        (0..n)
            .map(|i| {
                let a = std::f64::consts::TAU * f * i as f64 / fs;
                Complex32::new(amp * a.cos() as f32, amp * a.sin() as f32)
            })
            .collect()
    }

    fn power_at(y: &[Complex32], f: f64, fs: f64) -> f64 {
        let mut acc = num_complex::Complex64::new(0.0, 0.0);
        for (i, z) in y.iter().enumerate() {
            let a = -std::f64::consts::TAU * f * i as f64 / fs;
            acc += num_complex::Complex64::new(f64::from(z.re), f64::from(z.im))
                * num_complex::Complex64::from_polar(1.0, a);
        }
        (acc.norm() / y.len() as f64).powi(2)
    }

    fn plan(center: f64, rate: f64) -> Plan {
        Plan {
            rec_center_hz: 100e6,
            rec_rate_hz: 1e6,
            rec_usable_hz: 1e6,
            center_hz: center,
            rate_hz: rate,
            floor_power: 0.0,
            quant_power: 0.0,
            transition: 0.08,
        }
    }

    #[test]
    fn passthrough_is_exact_and_retune_keeps_absolute_frequency() {
        let x = tone(100e3, 1e6, 40_000, 0.5);
        let mut r = Render::new(plan(100e6, 1e6), 1, None);
        let mut out = Vec::new();
        r.render(&mut VecFeed(x.clone(), 0), &mut out, 20_000)
            .unwrap();
        assert_eq!(&out[..], &x[..20_000]);

        // Retuned +50 kHz, same rate: the tone sits at +50 kHz from the new centre.
        let mut r = Render::new(plan(100.05e6, 1e6), 1, None);
        let mut out = Vec::new();
        r.render(&mut VecFeed(x.clone(), 0), &mut out, 30_000)
            .unwrap();
        let y = &out[1000..];
        assert!(
            (power_at(y, 50e3, 1e6) - 0.25).abs() < 0.01,
            "{}",
            power_at(y, 50e3, 1e6)
        );
        assert!(power_at(y, 100e3, 1e6) < 1e-6);

        // Decimated to 400 kS/s around 100.05 MHz: still +50 kHz.
        let mut r = Render::new(plan(100.05e6, 400e3), 1, None);
        let mut out = Vec::new();
        r.render(&mut VecFeed(x, 0), &mut out, 12_000).unwrap();
        let y = &out[500..];
        assert!((power_at(y, 50e3, 400e3) - 0.25).abs() < 0.01);
    }

    /// Median noise density (per Hz) over `|f| < band` without `±guard` around `tone`: Hann
    /// Welch periodogram, `n` bins.
    fn noise_density(y: &[Complex32], fs: f64, n: usize, band: f64, tone: f64, guard: f64) -> f64 {
        let w: Vec<f64> = (0..n)
            .map(|i| 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / n as f64).cos())
            .collect();
        let w2: f64 = w.iter().map(|v| v * v).sum();
        let segs = y.len() / n;
        let mut avg = vec![0f64; n];
        let (mut re, mut im) = (vec![0f64; n], vec![0f64; n]);
        for s in 0..segs {
            for i in 0..n {
                re[i] = f64::from(y[s * n + i].re) * w[i];
                im[i] = f64::from(y[s * n + i].im) * w[i];
            }
            fft(&mut re, &mut im);
            for i in 0..n {
                avg[i] += (re[i] * re[i] + im[i] * im[i]) / segs as f64;
            }
        }
        let mut sel: Vec<f64> = (0..n)
            .filter_map(|i| {
                let k = if i < n / 2 {
                    i as f64
                } else {
                    i as f64 - n as f64
                };
                let f = k * fs / n as f64;
                (f.abs() < band && (f - tone).abs() > guard && k.abs() > 2.0).then_some(avg[i])
            })
            .collect();
        sel.sort_by(f64::total_cmp);
        let mean: f64 = sel.iter().sum::<f64>() / sel.len() as f64;
        mean / w2 / fs
    }

    /// Rounds to ci8 codes and back, as the mock's output at the recording's gain.
    fn ci8(z: Complex32) -> Complex32 {
        let q = |x: f32| (x * 128.0).round().clamp(-128.0, 127.0) / 128.0;
        Complex32::new(q(z.re), q(z.im))
    }

    /// Mean noise density (per Hz) of `y` within 60 kHz of `f0` (baseband Hz).
    fn density_near(y: &[Complex32], fs: f64, n: usize, f0: f64) -> f64 {
        let shifted: Vec<Complex32> = y
            .iter()
            .enumerate()
            .map(|(i, z)| {
                let a = -std::f64::consts::TAU * f0 * i as f64 / fs;
                z * Complex32::new(a.cos() as f32, a.sin() as f32)
            })
            .collect();
        noise_density(&shifted, fs, n, 60e3, f64::INFINITY, 0.0)
    }

    /// T-141: served through the mock's int8 rounding, a recording's floor PSD and a tone keep
    /// their levels (within 0.2 dB) after a retune at the recording's rate and across rates, where
    /// the floor follows the rounding density a radio at the new rate has. A quantisation-limited
    /// recording (≈ 0.5 code rms per component, where rounding twice read ≈ +0.9 dB) and one well
    /// above its rounding noise (≈ 4 codes).
    #[test]
    fn rendered_int8_keeps_floor_psd_and_tone_across_retunes_and_rates() {
        const QUANT: f64 = 1.0 / 6.0 / (128.0 * 128.0);
        let rec_fs = 500e3;
        for (noise_dbfs, seed) in [(-45.0, 5u64), (-27.0, 6)] {
            let mut rng = Rng::new(seed);
            let var = 10f64.powf(noise_dbfs / 10.0);
            // The tone sits in the flat pass band at every rate, clear of the floor measured
            // within ±60 kHz of the recording's centre.
            let x: Vec<Complex32> = tone(100e3, rec_fs, 1 << 17, 0.05)
                .into_iter()
                .map(|z| ci8(z + rng.complex_gaussian(var)))
                .collect();
            let floor = estimate_floor_power(&x).unwrap();
            let dq = Dequant::design(&x, QUANT).map(Arc::new);
            assert!(
                dq.is_some(),
                "{noise_dbfs} dBFS: rounding noise is not negligible"
            );
            let d_rec = density_near(&x, rec_fs, 512, 0.0);
            let tone_rec = power_at(&x, 100e3, rec_fs);
            let thermal = d_rec - QUANT / rec_fs;
            for (center, rate, n) in [
                (100.125e6, rec_fs, 512),
                (100.05e6, rec_fs, 512),
                (100e6, 2e6, 2048),
            ] {
                let plan = Plan {
                    rec_center_hz: 100e6,
                    rec_rate_hz: rec_fs,
                    rec_usable_hz: 0.75 * rec_fs,
                    center_hz: center,
                    rate_hz: rate,
                    floor_power: floor,
                    quant_power: QUANT,
                    transition: 0.08,
                };
                let mut r = Render::new(plan, 3, dq.clone());
                let mut out = Vec::new();
                let want = ((x.len() - 2000) as f64 * rate / rec_fs) as usize;
                r.render(&mut VecFeed(x.clone(), 0), &mut out, want)
                    .unwrap();
                let y: Vec<Complex32> = out[(rate / 50.0) as usize..]
                    .iter()
                    .map(|z| ci8(*z))
                    .collect();
                // The recording's centre in the output baseband.
                let off = 100e6 - center;
                let floor_err =
                    10.0 * (density_near(&y, rate, n, off) / (thermal + QUANT / rate)).log10();
                let tone_err = 10.0 * (power_at(&y, 100e3 + off, rate) / tone_rec).log10();
                assert!(
                    floor_err.abs() <= 0.2 && tone_err.abs() <= 0.2,
                    "{noise_dbfs} dBFS at {center} Hz / {rate} S/s: floor {floor_err:+.3} dB, \
                     tone {tone_err:+.3} dB"
                );
            }
        }
    }

    /// T-141: emissions that start after the recording's first samples keep their power through
    /// the dequantiser (within 0.2 dB): a weak tone (≈ 20 dB above a bin's floor) and a 10 kHz
    /// noise-like emission (≈ 10 dB above the floor density) appearing at sample 70 000 of a
    /// quantisation-limited recording, served after a retune at the recording's rate and at
    /// 2 MS/s. A dequantiser fixed from the first samples would take ≈ 1.2 dB off both.
    #[test]
    fn rendered_int8_keeps_emitters_that_appear_mid_recording() {
        const QUANT: f64 = 1.0 / 6.0 / (128.0 * 128.0);
        const ONSET: usize = 70_000;
        let rec_fs = 500e3;
        let len = 1 << 18;
        let mut rng = Rng::new(11);
        let var = 10f64.powf(-45.0 / 10.0);
        let weak = tone(-40e3, rec_fs, len, 10f32.powf(-55.0 / 20.0));
        // Band-limited emission: white noise through a 10 kHz low-pass, shifted to +30 kHz.
        let lp = Kernel::new(5e3 / rec_fs, 0.01);
        let taps = lp.row(0).to_vec();
        let white: Vec<Complex32> = (0..len + taps.len())
            .map(|_| rng.complex_gaussian(1.0))
            .collect();
        let band_var = 10.0 * var * 10e3 / rec_fs;
        let mut band: Vec<Complex32> = (0..len)
            .map(|i| {
                let a = std::f64::consts::TAU * 30e3 * i as f64 / rec_fs;
                let acc: Complex32 = taps.iter().zip(&white[i..]).map(|(h, z)| z * *h).sum();
                acc * Complex32::new(a.cos() as f32, a.sin() as f32)
            })
            .collect();
        let gain = (band_var / band.iter().map(|z| f64::from(z.norm_sqr())).sum::<f64>()
            * len as f64)
            .sqrt() as f32;
        for z in &mut band {
            *z *= gain;
        }
        let x: Vec<Complex32> = (0..len)
            .map(|i| {
                let e = if i >= ONSET { weak[i] + band[i] } else { Complex32::new(0.0, 0.0) };
                ci8(e + rng.complex_gaussian(var))
            })
            .collect();
        let floor = estimate_floor_power(&x).unwrap();
        let dq = Dequant::design(&x, QUANT).map(Arc::new);
        assert!(dq.is_some());
        let from = ONSET + 10_000;
        let rec = &x[from..];
        let thermal = density_near(&x[..ONSET], rec_fs, 512, -150e3) - QUANT / rec_fs;
        // Emission density above the floor over ±5 kHz of +30 kHz (thermal + one rounding).
        let excess = |y: &[Complex32], fs: f64, f0: f64, n: usize| {
            density_near_band(y, fs, n, f0, 5e3) - (thermal + QUANT / fs)
        };
        for (center, rate, n) in [(100.125e6, rec_fs, 2048), (100e6, 2e6, 8192)] {
            let plan = Plan {
                rec_center_hz: 100e6,
                rec_rate_hz: rec_fs,
                rec_usable_hz: 0.75 * rec_fs,
                center_hz: center,
                rate_hz: rate,
                floor_power: floor,
                quant_power: QUANT,
                transition: 0.08,
            };
            let mut r = Render::new(plan, 3, dq.clone());
            let mut out = Vec::new();
            let want = ((len - 2000) as f64 * rate / rec_fs) as usize;
            r.render(&mut VecFeed(x.clone(), 0), &mut out, want)
                .unwrap();
            let k0 = (from as f64 * rate / rec_fs) as usize;
            let k1 = k0 + ((len - 2000 - from) as f64 * rate / rec_fs) as usize;
            let y: Vec<Complex32> = out[k0..k1].iter().map(|z| ci8(*z)).collect();
            let r_len = ((k1 - k0) as f64 * rec_fs / rate) as usize;
            let off = 100e6 - center;
            let tone_err = 10.0
                * (power_at(&y, -40e3 + off, rate) / power_at(&rec[..r_len], -40e3, rec_fs))
                    .log10();
            let band_err =
                10.0 * (excess(&y, rate, 30e3 + off, n) / excess(&rec[..r_len], rec_fs, 30e3, 2048))
                    .log10();
            assert!(
                tone_err.abs() <= 0.2 && band_err.abs() <= 0.2,
                "{center} Hz / {rate} S/s: weak tone {tone_err:+.3} dB, band emission \
                 {band_err:+.3} dB"
            );
        }
    }

    /// Mean density (per Hz) of `y` within `half` Hz of `f0` (baseband Hz).
    fn density_near_band(y: &[Complex32], fs: f64, n: usize, f0: f64, half: f64) -> f64 {
        let shifted: Vec<Complex32> = y
            .iter()
            .enumerate()
            .map(|(i, z)| {
                let a = -std::f64::consts::TAU * f0 * i as f64 / fs;
                z * Complex32::new(a.cos() as f32, a.sin() as f32)
            })
            .collect();
        noise_density(&shifted, fs, n, half, f64::INFINITY, 0.0)
    }

    #[test]
    fn floor_estimate_matches_white_noise_under_a_tone() {
        let mut rng = Rng::new(7);
        let x: Vec<Complex32> = tone(123e3, 1e6, 65_536, 0.3)
            .into_iter()
            .map(|z| z + rng.complex_gaussian(1e-3))
            .collect();
        let p = estimate_floor_power(&x).unwrap();
        assert!((p / 1e-3 - 1.0).abs() < 0.1, "{p}");
    }
}
