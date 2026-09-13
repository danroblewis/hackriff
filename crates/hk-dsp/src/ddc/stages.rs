//! DDC stages: the xlating decimating FIR and the polyphase resampler.

use std::f64::consts::PI;

use num_complex::Complex32;

use super::plan::{ResampleKind, ResamplePlan};
use crate::filter::history::History;
use crate::filter::kernels::{dot_complex, dot_real};
use crate::filter::{FirDesign, Nco};
use crate::stft::IqSample;

/// Stage 1: complex band-pass taps at the channel centre, integer decimation, and an NCO
/// rotation of each output evaluated at its absolute source index.
///
/// With `s` the newest `L` samples (`s[L−1] = x[n]`) and `ω = 2π·f0/fs`:
/// `y = e^{−jωn} · Σ_i h[i]·e^{jω(L−1−i)}·s[i]`, which equals the input mixed down by `f0`
/// (phase referenced to sample index 0 of the stream), low-passed by `h` and sampled at `n`;
/// it represents source index `n − (L−1)/2`. The `e^{−jωn}` term comes from an exact `u64`
/// [`Nco`], so it is exact at any stream index; the taps use the same quantised frequency.
pub(crate) struct Xlating {
    taps: Vec<Complex32>,
    len: usize,
    decimation: usize,
    nco: Nco,
    history: History,
    countdown: usize,
    start: u64,
    count: u64,
}

impl Xlating {
    pub(crate) fn new(
        design: &FirDesign,
        decimation: usize,
        center_offset_hz: f64,
        fs: f64,
    ) -> Self {
        let len = design.len();
        let nco = Nco::new(center_offset_hz / fs);
        let r = nco.cycles_per_sample();
        let taps = design
            .taps
            .iter()
            .enumerate()
            .map(|(i, &h)| {
                let ph = 2.0 * PI * r * (len - 1 - i) as f64;
                Complex32::new(
                    (f64::from(h) * ph.cos()) as f32,
                    (f64::from(h) * ph.sin()) as f32,
                )
            })
            .collect();
        Self {
            taps,
            len,
            decimation,
            nco,
            history: History::new(len, len.max(decimation)),
            countdown: len,
            start: 0,
            count: 0,
        }
    }

    pub(crate) fn restart(&mut self, start: u64) {
        self.history.clear();
        self.countdown = self.len;
        self.start = start;
        self.count = 0;
    }

    /// Source index represented by stage-1 output `a` (counted from the last restart).
    pub(crate) fn source_index_of(&self, a: f64) -> f64 {
        self.start as f64 + (self.len - 1) as f64 / 2.0 + a * self.decimation as f64
    }

    /// Outputs since the last restart.
    pub(crate) fn count(&self) -> u64 {
        self.count
    }

    #[inline]
    pub(crate) fn push<T: IqSample>(&mut self, samples: &[T], mut emit: impl FnMut(Complex32)) {
        let mut pos = 0;
        while pos < samples.len() {
            let take = self.countdown.min(samples.len() - pos);
            self.history.push(&samples[pos..pos + take]);
            pos += take;
            self.countdown -= take;
            if self.countdown == 0 {
                let n = self.start + (self.len - 1) as u64 + self.count * self.decimation as u64;
                let y = dot_complex(&self.taps, self.history.window());
                let rot = self.nco.rotator(n);
                self.count += 1;
                self.countdown = self.decimation;
                emit(y * rot);
            }
        }
    }
}

/// Stage 2: polyphase resampler (integer, rational or fractional).
///
/// With `P` branches and `T` taps per branch, prototype `h` (length `P·T`, gain `P`) at rate
/// `P·fs1`: the output at interpolated position `m = n·P + φ` is
/// `Σ_j h[φ + (T−1−j)·P] · s[j]` over the newest `T` inputs (`s[T−1] = x[n]`). It represents
/// input position `n + φ/P − (P·T − 1)/(2P)`. Fractional mode interpolates linearly between
/// branch tables `⌊μP⌋` and `⌊μP⌋+1` for a real `μ ∈ [0, 1)`.
///
/// The extra branch table `P` (used only as the upper interpolation neighbour in fractional
/// mode) should be branch 0 advanced by one input sample; it is built from `h[P + (T−1−j)·P]`,
/// so its last coefficient `h[P·T]` (beyond the prototype) is 0 and the `h[0]` term that the
/// shifted branch 0 would carry is dropped. `h[0]` is the outermost Kaiser-windowed tap
/// (≈ 1e−4 of the peak or less at 60 dB), so the effect is negligible.
pub(crate) struct Polyphase {
    kind: ResampleKind,
    phases: usize,
    taps_per_phase: usize,
    /// `(P + 1) × T` branch tables, each reversed for the window order.
    tables: Vec<f32>,
    scratch: Vec<f32>,
    history: History,
    received: u64,
    n_hi: u64,
    phase: usize,
    mu: f64,
    delay: f64,
}

impl Polyphase {
    pub(crate) fn new(plan: &ResamplePlan) -> Self {
        let p = plan.kind.phases();
        let t = plan.taps_per_phase;
        let h = &plan.design.taps;
        let mut tables = vec![0.0f32; (p + 1) * t];
        for phi in 0..=p {
            for j in 0..t {
                let idx = phi + (t - 1 - j) * p;
                tables[phi * t + j] = h.get(idx).copied().unwrap_or(0.0);
            }
        }
        Self {
            kind: plan.kind,
            phases: p,
            taps_per_phase: t,
            tables,
            scratch: vec![0.0; t],
            history: History::new(t, 1),
            received: 0,
            n_hi: t as u64 - 1,
            phase: 0,
            mu: 0.0,
            delay: (plan.design.len() as f64 - 1.0) / (2.0 * p as f64),
        }
    }

    pub(crate) fn restart(&mut self) {
        self.history.clear();
        self.received = 0;
        self.n_hi = self.taps_per_phase as u64 - 1;
        self.phase = 0;
        self.mu = 0.0;
    }

    /// Input position (stage-1 output index) the next output represents.
    pub(crate) fn next_position(&self) -> f64 {
        let frac = match self.kind {
            ResampleKind::Fractional { .. } => self.mu,
            _ => self.phase as f64 / self.phases as f64,
        };
        self.n_hi as f64 + frac - self.delay
    }

    #[inline]
    pub(crate) fn push_one(&mut self, x: Complex32, emit: &mut impl FnMut(Complex32)) {
        self.history.push_one(x);
        self.received += 1;
        let t = self.taps_per_phase;
        while self.n_hi + 1 == self.received {
            let window = self.history.window();
            match self.kind {
                ResampleKind::Integer { decimation } => {
                    emit(dot_real(&self.tables[..t], window));
                    self.n_hi += decimation as u64;
                }
                ResampleKind::Rational { up, down } => {
                    let tab = &self.tables[self.phase * t..(self.phase + 1) * t];
                    emit(dot_real(tab, window));
                    self.phase += down;
                    self.n_hi += (self.phase / up) as u64;
                    self.phase %= up;
                }
                ResampleKind::Fractional { ratio, .. } => {
                    let pos = self.mu * self.phases as f64;
                    let i0 = (pos as usize).min(self.phases - 1);
                    let a = (pos - i0 as f64) as f32;
                    let (t0, t1) = self.tables[i0 * t..(i0 + 2) * t].split_at(t);
                    for ((s, &u), &v) in self.scratch.iter_mut().zip(t0).zip(t1) {
                        *s = u + a * (v - u);
                    }
                    emit(dot_real(&self.scratch, window));
                    self.mu += ratio;
                    let adv = self.mu.floor();
                    self.mu -= adv;
                    self.n_hi += adv as u64;
                }
            }
        }
    }
}
