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

    /// The recorded band the renderer band-selects, `(lo, hi)` absolute Hz: [`Self::overlap`] with
    /// each side that the window's own edge clips pulled in by half the transition (T-175). The
    /// band-select roll-off then ends inside the window. Centred on ±rate/2 it passed recorded
    /// content up to half a transition beyond the window edge, which the output rate folded onto
    /// the opposite edge (the 101.3 MHz station just past a window's upper edge read as a line at
    /// its lower edge). The complement noise fills the strip. Passing through, the whole overlap.
    pub fn served(&self) -> Option<(f64, f64)> {
        let (lo, hi) = self.overlap()?;
        if self.passthrough() {
            return Some((lo, hi));
        }
        let guard = 0.5 * self.transition * self.rate_hz;
        let w_lo = self.center_hz - self.rate_hz / 2.0;
        let w_hi = self.center_hz + self.rate_hz / 2.0;
        let lo = if lo <= w_lo + 1.0 { w_lo + guard } else { lo };
        let hi = if hi >= w_hi - 1.0 { w_hi - guard } else { hi };
        // T-231: the same guard against the recording's **own** band edge. A recording's spectrum
        // is periodic at its sample rate, so a transition straddling `rec_center ± rec_rate/2`
        // passes the alias of content at the opposite edge: the device would present recorded
        // energy at a frequency it was never tuned to (a tone 10 kHz inside one edge reading as a
        // line 10 kHz outside the other, 25 dB down). Ending the roll-off at the recorded edge
        // keeps every served sample truthful; the complement noise fills the strip. A recording
        // whose baseband filter is narrower than its rate (`rec_usable_hz`) already has that
        // margin, and passing through (above) serves the recording as captured.
        let rec_lo = self.rec_center_hz - self.rec_rate_hz / 2.0;
        let rec_hi = self.rec_center_hz + self.rec_rate_hz / 2.0;
        let (lo, hi) = (lo.max(rec_lo + guard), hi.min(rec_hi - guard));
        (hi - lo > 1.0).then_some((lo, hi))
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
        let served = plan.served();
        // T-231 device contract: the mock never presents samples attributed to a tuning it is not
        // on. The band select, roll-off included, stays inside the recorded band, so no alias of
        // the recording's opposite edge reaches the output. Checked once per retune so the harness
        // cannot lie to a test that trusts it.
        if let Some((lo, hi)) = served
            && !plan.passthrough()
        {
            let guard = 0.5 * plan.transition * plan.rate_hz;
            let rec_lo = plan.rec_center_hz - plan.rec_rate_hz / 2.0;
            let rec_hi = plan.rec_center_hz + plan.rec_rate_hz / 2.0;
            assert!(
                lo - guard >= rec_lo - 1.0 && hi + guard <= rec_hi + 1.0,
                "mock would serve {lo}..{hi} Hz (± {guard} Hz of transition) outside the recorded \
                 band {rec_lo}..{rec_hi} Hz"
            );
        }
        self.noise_only = served.is_none();
        let width = (plan.transition * plan.rate_hz / plan.rec_rate_hz).min(0.25);
        let passthrough = plan.passthrough() && pos.fract() == 0.0;
        self.dq_delay = 0;
        match served {
            Some((lo, hi)) if !passthrough => {
                let c = 0.5 * (lo + hi) - plan.rec_center_hz;
                let kernel = Kernel::new((hi - lo) / 2.0 / plan.rec_rate_hz, width);
                self.half = kernel.half;
                self.kernel = Some(kernel);
                self.dq_delay = if self.dequant.is_some() {
                    DEQUANT_N + DEQUANT_N / 2
                } else {
                    0
                };
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
        // Noise fills whatever the band select leaves of a rendered window: the uncovered part
        // and, since T-175, the guard strip inside each window edge the recording extends past.
        self.noise = match served {
            Some((lo, hi)) if self.kernel.is_some() && hi - lo < plan.rate_hz - 1.0 => {
                Some(ComplementNoise::new(
                    (lo - plan.center_hz) / plan.rate_hz,
                    (hi - plan.center_hz) / plan.rate_hz,
                    plan.transition,
                ))
            }
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
/// Rendering with it needs `1.5·N` recording samples of look-ahead (a frame and the next).
const DEQUANT_N: usize = 1024;
/// Weight of each new frame in a bin's level estimate (≈ 15 frames of memory).
const DEQUANT_ALPHA: f64 = 1.0 / 8.0;
/// Bins either side averaged into a bin's level estimate.
const DEQUANT_SPREAD: usize = 2;
/// Attack: a frame whose local level exceeds the estimate this many times replaces it at once
/// (onsets). A noise-only 5-bin mean does so with probability ≈ 9·10⁻⁴ (≈ +0.01 dB of floor).
const DEQUANT_ATTACK: f64 = 3.0;
/// Release: a frame and the next one both below the estimate by this factor replace it with
/// their mean (stops, from the first frame past them). Noise-only: ≈ 8·10⁻³ per frame, ≈ 7·10⁻⁵ for two, so no measurable floor bias.
const DEQUANT_RELEASE: f64 = 4.0;
/// Frames of the history window a restart re-seeds the level estimate from, and the weight (in
/// frames) the old estimate keeps when fewer are available.
const DEQUANT_SEED_FRAMES: usize = 8;
const DEQUANT_SEED_PRIOR: usize = 4;
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
/// over *past* frames. A floor bin (`P̂ = A + q`) serves `A`, and a bin holding an emission
/// (`S + A + q`) serves `S + A`: the rounding noise comes out and the emission's power stays. An
/// onset ≳ 3× the estimate replaces it within the frame ([`DEQUANT_ATTACK`]) and a stop to
/// ≲ 1/4 from the first frame past it, confirmed by the next ([`DEQUANT_RELEASE`]); smaller changes follow within ≈ 15 frames. A
/// restart (discarded or dropped history) re-seeds the estimate from the history window. The served PSD
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
/// `hist[i]`, final before the next frame's start and half-accumulated for `N/2` beyond it. Each
/// frame needs the next one's spectrum too (release confirmation), cached for its own turn.
#[derive(Debug)]
struct DequantState {
    design: Arc<Dequant>,
    window: Vec<f64>,
    /// Smoothed local (±[`DEQUANT_SPREAD`] bins) level per bin, from past frames.
    level: Vec<f64>,
    gain: Vec<f64>,
    /// Absolute recording index of the next frame's first sample.
    next: i64,
    clean: Vec<Complex32>,
    re: Vec<f64>,
    im: Vec<f64>,
    /// Local level of the frame being processed.
    m_cur: Vec<f64>,
    /// Spectrum and local level of the frame after it, and whether that frame was zero-padded
    /// (`None`: not computed yet).
    ahead_re: Vec<f64>,
    ahead_im: Vec<f64>,
    m_ahead: Vec<f64>,
    ahead: Option<bool>,
}

/// Windowed FFT of `hist[s..s + N]` (indices before 0 are zero); `true` if padded.
fn windowed_fft(
    hist: &[Complex32],
    s: i64,
    window: &[f64],
    re: &mut [f64],
    im: &mut [f64],
) -> bool {
    let mut padded = false;
    for (i, w) in window.iter().enumerate() {
        let j = s + i as i64;
        let z = if j >= 0 {
            hist[j as usize]
        } else {
            padded = true;
            Complex32::new(0.0, 0.0)
        };
        re[i] = f64::from(z.re) * w;
        im[i] = f64::from(z.im) * w;
    }
    fft(re, im);
    padded
}

/// Local (±[`DEQUANT_SPREAD`] bins) level of a spectrum whose window has `Σw² = w2`.
fn local_level(re: &[f64], im: &[f64], w2: f64) -> Vec<f64> {
    let p: Vec<f64> = re
        .iter()
        .zip(im)
        .map(|(r, i)| (r * r + i * i) / w2)
        .collect();
    local_mean(&p)
}

impl DequantState {
    fn new(design: Arc<Dequant>) -> Self {
        let n = DEQUANT_N;
        let level = local_mean(&design.initial);
        Self {
            window: (0..n)
                .map(|i| (std::f64::consts::PI * i as f64 / n as f64).sin())
                .collect(),
            level,
            gain: vec![1.0; n],
            design,
            next: 0,
            clean: Vec::new(),
            re: vec![0.0; n],
            im: vec![0.0; n],
            m_cur: vec![0.0; n],
            ahead_re: vec![0.0; n],
            ahead_im: vec![0.0; n],
            m_ahead: vec![0.0; n],
            ahead: None,
        }
    }

    /// Processes every frame `hist` (starting at absolute index `base − start`) holds together
    /// with the frame after it, and returns how many leading `clean` samples are final. A history
    /// that no longer lines up with `clean` (discarded input, or dropped past the next frame)
    /// restarts at `base`, with the frame straddling it zero-padded before the history, and
    /// re-seeds the level estimate.
    fn advance(&mut self, hist: &[Complex32], start: usize, base: u64) -> usize {
        let (n, h) = (DEQUANT_N as i64, DEQUANT_N as i64 / 2);
        let hist0 = base as i64 - start as i64;
        if self.next - hist0 < -h || self.clean.len() as i64 != self.next - hist0 + h {
            self.next = base as i64 - h;
            self.clean.clear();
            self.clean.resize(start, Complex32::new(0.0, 0.0));
            self.ahead = None;
            self.reseed(hist, start);
        }
        let w2 = n as f64 / 2.0;
        let q = self.design.quant_power;
        let len = DEQUANT_N;
        loop {
            let s = self.next - hist0;
            if s + n + h > hist.len() as i64 {
                break;
            }
            let cur_padded = match self.ahead.take() {
                Some(padded) => {
                    std::mem::swap(&mut self.re, &mut self.ahead_re);
                    std::mem::swap(&mut self.im, &mut self.ahead_im);
                    std::mem::swap(&mut self.m_cur, &mut self.m_ahead);
                    padded
                }
                None => {
                    let padded = windowed_fft(hist, s, &self.window, &mut self.re, &mut self.im);
                    self.m_cur = local_level(&self.re, &self.im, w2);
                    padded
                }
            };
            let ahead_padded = windowed_fft(
                hist,
                s + h,
                &self.window,
                &mut self.ahead_re,
                &mut self.ahead_im,
            );
            self.m_ahead = local_level(&self.ahead_re, &self.ahead_im, w2);
            self.ahead = Some(ahead_padded);
            // The gain follows the past frames' level, so a noise bin's gain is independent of
            // its current value; only a change beyond the attack factor (this frame) or the
            // release factor (this frame and the next) uses the new level: an onset or a stop.
            for k in 0..len {
                let mut p = self.level[k];
                if !cur_padded {
                    let (mk, ak) = (self.m_cur[k], self.m_ahead[k]);
                    if mk > DEQUANT_ATTACK * p {
                        p = mk;
                        self.level[k] = mk;
                    } else if !ahead_padded && mk * DEQUANT_RELEASE < p && ak * DEQUANT_RELEASE < p
                    {
                        p = 0.5 * (mk + ak);
                        self.level[k] = p;
                    } else {
                        self.level[k] += DEQUANT_ALPHA * (mk - p);
                    }
                }
                self.gain[k] = if p > 0.0 {
                    (1.0 - q / p).max(DEQUANT_MIN_GAIN2).sqrt()
                } else {
                    DEQUANT_MIN_GAIN2.sqrt()
                };
            }
            for k in 0..len {
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

    /// Re-seeds the level estimate from up to [`DEQUANT_SEED_FRAMES`] whole frames of `hist`
    /// around `start` (the recent past it still holds, then what lies ahead), so a restart after
    /// passing through or skipping does not start from a stale estimate. With fewer frames the
    /// old estimate keeps [`DEQUANT_SEED_PRIOR`] frames' weight.
    fn reseed(&mut self, hist: &[Complex32], start: usize) {
        let len = DEQUANT_N;
        let a = start.saturating_sub(DEQUANT_SEED_FRAMES / 2 * len);
        let b = (a + DEQUANT_SEED_FRAMES * len).min(hist.len());
        let frames = (b - a) / len;
        if frames == 0 {
            return;
        }
        let w2 = len as f64 / 2.0;
        let mut acc = vec![0f64; len];
        for f in 0..frames {
            windowed_fft(
                hist,
                (a + f * len) as i64,
                &self.window,
                &mut self.re,
                &mut self.im,
            );
            for (k, v) in acc.iter_mut().enumerate() {
                *v += (self.re[k] * self.re[k] + self.im[k] * self.im[k]) / w2;
            }
        }
        let seed = local_mean(&acc);
        let prior = DEQUANT_SEED_PRIOR.saturating_sub(frames) as f64;
        let total = frames as f64 + prior;
        for (k, s) in seed.iter().enumerate() {
            self.level[k] = (s + prior * self.level[k]) / total;
        }
    }
}

/// Mean of `x` over ±[`DEQUANT_SPREAD`] bins (circular).
fn local_mean(x: &[f64]) -> Vec<f64> {
    let len = x.len();
    let width = (2 * DEQUANT_SPREAD + 1) as f64;
    let mut acc: f64 = (0..=2 * DEQUANT_SPREAD)
        .map(|o| x[(len + o - DEQUANT_SPREAD) % len])
        .sum();
    let mut out = Vec::with_capacity(len);
    for k in 0..len {
        out.push(acc / width);
        acc += x[(k + DEQUANT_SPREAD + 1) % len] - x[(k + len - DEQUANT_SPREAD) % len];
    }
    out
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

    /// T-175: recorded content just past a window edge, inside the band-select transition, does
    /// not fold onto the opposite edge.
    #[test]
    fn content_just_past_a_window_edge_does_not_fold_to_the_other_edge() {
        // Recording 99.5..100.5 MHz; window 99.79..100.19 MHz at 400 kS/s (transition 32 kHz); a
        // tone at 100.2 MHz sits 10 kHz past the upper edge and would fold to −190 kHz.
        let x = tone(200e3, 1e6, 1 << 16, 0.25);
        let mut r = Render::new(plan(99.99e6, 400e3), 1, None);
        let mut out = Vec::new();
        r.render(&mut VecFeed(x, 0), &mut out, 20_000).unwrap();
        let alias = power_at(&out[2_000..], -190e3, 400e3);
        assert!(alias < 1e-6, "tone folded to the lower edge: {alias:e}");
    }

    /// T-231: a recording's spectrum is periodic at its sample rate, so a band-select transition
    /// that straddles the recording's **own** band edge passes the alias of content at the
    /// opposite edge, and the device presents recorded energy at a frequency it was never on.
    /// T-175 pulled the served band inside the *window* edge; the recorded band edge needs the
    /// same guard whenever the recording has no narrower baseband filter (`usable == rate`).
    #[test]
    fn content_at_the_recorded_band_edge_does_not_alias_outside_it() {
        // Recording 99.5..100.5 MHz at 1 Msps, usable == rate. A tone at 100.49 MHz sits 10 kHz
        // inside the upper edge; sampled at 1 Msps it is indistinguishable from 99.49 MHz, which
        // is 10 kHz *below* the recorded band.
        let x = tone(490e3, 1e6, 1 << 16, 0.25);
        // Window 99.2..99.6 MHz at 400 kS/s: the recorded band's lower edge (99.5 MHz) falls
        // inside the window, so the band-select transition sits astride it.
        let mut r = Render::new(plan(99.4e6, 400e3), 1, None);
        let mut out = Vec::new();
        r.render(&mut VecFeed(x, 0), &mut out, 20_000).unwrap();
        // 99.49 MHz is +90 kHz in this window: outside the recorded band, where nothing recorded
        // may appear.
        let alias = power_at(&out[2_000..], 90e3, 400e3);
        assert!(
            alias < 1e-6,
            "recorded content aliased below the recorded band: {alias:e}"
        );
    }

    /// T-231: the band select, its roll-off included, never reaches outside the recorded band, with
    /// or without a recorded baseband filter narrower than the rate. (Passing through is exempt: it
    /// serves the recording as captured, unfiltered.)
    #[test]
    fn served_never_leaves_the_recorded_band() {
        for usable in [1e6, 750e3] {
            for center in [98.5e6, 99.4e6, 99.9e6, 100.0e6, 100.6e6, 101.5e6] {
                for rate in [200e3, 400e3, 1e6, 2e6, 4e6] {
                    let p = Plan {
                        rec_usable_hz: usable,
                        ..plan(center, rate)
                    };
                    let Some((lo, hi)) = p.served() else {
                        continue;
                    };
                    if p.passthrough() {
                        continue;
                    }
                    let guard = 0.5 * p.transition * p.rate_hz;
                    let (rec_lo, rec_hi) = (
                        p.rec_center_hz - p.rec_rate_hz / 2.0,
                        p.rec_center_hz + p.rec_rate_hz / 2.0,
                    );
                    assert!(
                        lo - guard >= rec_lo - 1.0 && hi + guard <= rec_hi + 1.0,
                        "window {center} Hz at {rate} Hz (usable {usable} Hz) serves {lo}..{hi} \
                         ± {guard} Hz, outside the recorded band {rec_lo}..{rec_hi}"
                    );
                }
            }
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

    /// Mean density (per Hz) of `y` within `half` Hz of `f0` (baseband Hz): `y` shifted by `−f0`,
    /// rectangular-window FFT (`y.len()` a power of two), mean of the bins within the band.
    fn band_density(y: &[Complex32], fs: f64, f0: f64, half: f64) -> f64 {
        let n = y.len();
        let (mut re, mut im): (Vec<f64>, Vec<f64>) = y
            .iter()
            .enumerate()
            .map(|(i, z)| {
                let a = -std::f64::consts::TAU * f0 * i as f64 / fs;
                let w = z * Complex32::new(a.cos() as f32, a.sin() as f32);
                (f64::from(w.re), f64::from(w.im))
            })
            .unzip();
        fft(&mut re, &mut im);
        let bins: Vec<f64> = (0..n)
            .filter(|&k| {
                let kk = if k < n / 2 {
                    k as f64
                } else {
                    k as f64 - n as f64
                };
                (kk * fs / n as f64).abs() <= half
            })
            .map(|k| re[k] * re[k] + im[k] * im[k])
            .collect();
        bins.iter().sum::<f64>() / bins.len() as f64 / n as f64 / fs
    }

    /// T-141 review: emissions that start and stop mid-recording keep their power through the
    /// dequantiser, transients included. A weak tone (≈ 20 dB above a bin's floor, −40 kHz) and a
    /// 10 kHz noise-like emission (≈ 10 dB above the floor density, +30 kHz) are on for 8192
    /// samples of every 16 384 from sample 70 000 (after the design window) of a
    /// quantisation-limited recording, served after a retune at the recording's rate and at
    /// 2 MS/s and rounded to ci8. Served and recorded levels are compared over the same samples,
    /// summed over the bursts (the served density expects the output rate's rounding density).
    /// Bounds set before running: 0.5 dB over the first 2048 samples after each onset (tone,
    /// band) and after each stop (floor at both places); 0.2 dB over each burst's second half.
    #[test]
    fn rendered_int8_keeps_emitters_through_onsets_and_stops() {
        const QUANT: f64 = 1.0 / 6.0 / (128.0 * 128.0);
        const ONSET: usize = 70_000;
        const PERIOD: usize = 16_384;
        const ON: usize = 8_192;
        const W: usize = 2_048;
        let rec_fs = 500e3;
        let len = 1 << 18;
        let mut rng = Rng::new(11);
        let var = 10f64.powf(-45.0 / 10.0);
        let weak = tone(-40e3, rec_fs, len, 10f32.powf(-55.0 / 20.0));
        // Band-limited emission: white noise through a 10 kHz low-pass, shifted to +30 kHz.
        let taps = Kernel::new(5e3 / rec_fs, 0.01).row(0).to_vec();
        let white: Vec<Complex32> = (0..len + taps.len())
            .map(|_| rng.complex_gaussian(1.0))
            .collect();
        let mut band: Vec<Complex32> = (0..len)
            .map(|i| {
                let a = std::f64::consts::TAU * 30e3 * i as f64 / rec_fs;
                let acc: Complex32 = taps.iter().zip(&white[i..]).map(|(h, z)| z * *h).sum();
                acc * Complex32::new(a.cos() as f32, a.sin() as f32)
            })
            .collect();
        let band_var = 10.0 * var * 10e3 / rec_fs;
        let gain = (band_var / band.iter().map(|z| f64::from(z.norm_sqr())).sum::<f64>()
            * len as f64)
            .sqrt() as f32;
        for z in &mut band {
            *z *= gain;
        }
        let on = |i: usize| i >= ONSET && (i - ONSET) % PERIOD < ON;
        let x: Vec<Complex32> = (0..len)
            .map(|i| {
                let e = if on(i) {
                    weak[i] + band[i]
                } else {
                    Complex32::new(0.0, 0.0)
                };
                ci8(e + rng.complex_gaussian(var))
            })
            .collect();
        let bursts: Vec<usize> = (0..)
            .map(|j| ONSET + j * PERIOD)
            .take_while(|s| s + ON + W <= len - 4096)
            .collect();
        let floor = estimate_floor_power(&x).unwrap();
        let dq = Dequant::design(&x, QUANT).map(Arc::new);
        assert!(dq.is_some());
        let names = [
            "onset tone",
            "onset band",
            "stop floor -40 kHz",
            "stop floor +30 kHz",
            "settled tone",
            "settled band",
        ];
        let bounds = [0.5, 0.5, 0.5, 0.5, 0.2, 0.2];
        for (center, rate) in [(100.125e6, rec_fs), (100e6, 2e6)] {
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
            let y: Vec<Complex32> = out.iter().map(|z| ci8(*z)).collect();
            let ratio = (rate / rec_fs) as usize;
            let off = 100e6 - center;
            // Recorded density as the output rate's rounding would serve it.
            let rounding = QUANT / rate - QUANT / rec_fs;
            let mut sums = [[0f64; 2]; 6];
            for &s in &bursts {
                let seg = |a: usize, n: usize| (&y[a * ratio..(a + n) * ratio], &x[a..a + n]);
                let (ys, xs) = seg(s, W);
                sums[0][0] += power_at(ys, -40e3 + off, rate);
                sums[0][1] += power_at(xs, -40e3, rec_fs);
                sums[1][0] += band_density(ys, rate, 30e3 + off, 5e3);
                sums[1][1] += band_density(xs, rec_fs, 30e3, 5e3) + rounding;
                let (ys, xs) = seg(s + ON, W);
                sums[2][0] += band_density(ys, rate, -40e3 + off, 5e3);
                sums[2][1] += band_density(xs, rec_fs, -40e3, 5e3) + rounding;
                sums[3][0] += band_density(ys, rate, 30e3 + off, 5e3);
                sums[3][1] += band_density(xs, rec_fs, 30e3, 5e3) + rounding;
                let (ys, xs) = seg(s + ON / 2, ON / 2);
                sums[4][0] += power_at(ys, -40e3 + off, rate);
                sums[4][1] += power_at(xs, -40e3, rec_fs);
                sums[5][0] += band_density(ys, rate, 30e3 + off, 5e3);
                sums[5][1] += band_density(xs, rec_fs, 30e3, 5e3) + rounding;
            }
            let mut ok = true;
            let mut report = String::new();
            for i in 0..6 {
                let e = 10.0 * (sums[i][0] / sums[i][1]).log10();
                report.push_str(&format!("{} {e:+.3} dB; ", names[i]));
                ok &= e.abs() <= bounds[i];
            }
            eprintln!("T-141 dequant transients {center} Hz / {rate} S/s: {report}");
            assert!(
                ok,
                "{center} Hz / {rate} S/s ({} bursts): {report}",
                bursts.len()
            );
        }
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
