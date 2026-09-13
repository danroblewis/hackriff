//! Shared DSP kernels for spike S1 (identical maths in both implementations).
//!
//! Not optimised product DSP: just enough real work (FFT, DDC, FM) that the
//! CPU and latency numbers mean something.

#![allow(dead_code)]

use num_complex::Complex32;
use std::f32::consts::TAU;

/// HackRF int8 interleaved -> complex float, appended to `dst` (cleared first).
pub fn i8_to_c32(src: &[i8], dst: &mut Vec<Complex32>) {
    dst.clear();
    dst.extend(
        src.chunks_exact(2)
            .map(|p| Complex32::new(p[0] as f32 / 128.0, p[1] as f32 / 128.0)),
    );
}

/// Blackman-windowed sinc low-pass for decimation by `decim`
/// (passband edge 0.4 * output rate, ntaps = 4*decim + 1).
pub fn lowpass_taps(decim: usize) -> Vec<f32> {
    let ntaps = 4 * decim + 1;
    let fc = 0.4 / decim as f32; // cycles/sample, one-sided cutoff
    let m = (ntaps - 1) as f32;
    let mut taps: Vec<f32> = (0..ntaps)
        .map(|n| {
            let x = n as f32 - m / 2.0;
            let sinc = if x == 0.0 {
                2.0 * fc
            } else {
                (TAU * fc * x).sin() / (std::f32::consts::PI * x)
            };
            let w = 0.42 - 0.5 * (TAU * n as f32 / m).cos() + 0.08 * (2.0 * TAU * n as f32 / m).cos();
            sinc * w
        })
        .collect();
    let g: f32 = taps.iter().sum();
    taps.iter_mut().for_each(|t| *t /= g);
    taps
}

/// Frequency-translating decimating FIR (GNU Radio / FutureSDR XlatingFir
/// structure): complex band-pass taps centred on `offset_hz`, decimate, then
/// rotate the decimated output back to baseband.
pub struct XlatingDecimator {
    taps_rev: Vec<Complex32>,
    decim: usize,
    hist: Vec<Complex32>,
    pos: usize,
    rot: Complex32,
    rot_step: Complex32,
}

impl XlatingDecimator {
    pub fn new(offset_hz: f64, decim: usize, fs: f64) -> Self {
        let lp = lowpass_taps(decim);
        let w = (std::f64::consts::TAU * offset_hz / fs) as f32;
        let mut bp: Vec<Complex32> = lp
            .iter()
            .enumerate()
            .map(|(i, &t)| Complex32::from_polar(1.0, w * i as f32) * t)
            .collect();
        bp.reverse(); // convolution as a dot product over the window
        Self {
            taps_rev: bp,
            decim,
            hist: Vec::with_capacity(1 << 18),
            pos: 0,
            rot: Complex32::new(1.0, 0.0),
            rot_step: Complex32::from_polar(1.0, -w * decim as f32),
        }
    }

    pub fn out_rate(&self, fs: f64) -> f64 {
        fs / self.decim as f64
    }

    pub fn process(&mut self, input: &[Complex32], out: &mut Vec<Complex32>) {
        self.hist.extend_from_slice(input);
        let n = self.taps_rev.len();
        while self.pos + n <= self.hist.len() {
            let y = dot4(&self.taps_rev, &self.hist[self.pos..self.pos + n]);
            out.push(y * self.rot);
            self.rot *= self.rot_step;
            self.pos += self.decim;
        }
        // renormalise rotator, drop consumed history
        self.rot /= self.rot.norm();
        self.hist.drain(..self.pos);
        self.pos = 0;
    }
}

/// Complex dot product with 4 independent accumulation lanes so LLVM can
/// vectorise without fast-math.
#[inline]
fn dot4(a: &[Complex32], b: &[Complex32]) -> Complex32 {
    let mut re = [0.0f32; 4];
    let mut im = [0.0f32; 4];
    let ca = a.chunks_exact(4);
    let cb = b.chunks_exact(4);
    let (ra, rb) = (ca.remainder(), cb.remainder());
    for (x, y) in ca.zip(cb) {
        for k in 0..4 {
            re[k] += x[k].re * y[k].re - x[k].im * y[k].im;
            im[k] += x[k].re * y[k].im + x[k].im * y[k].re;
        }
    }
    let mut s = Complex32::new(re.iter().sum(), im.iter().sum());
    for (x, y) in ra.iter().zip(rb) {
        s += x * y;
    }
    s
}

/// Quadrature FM discriminator, output in Hz of instantaneous frequency.
pub struct FmDiscriminator {
    prev: Complex32,
    gain: f32,
}

impl FmDiscriminator {
    pub fn new(fs_out: f64) -> Self {
        Self {
            prev: Complex32::new(1.0, 0.0),
            gain: (fs_out / std::f64::consts::TAU) as f32,
        }
    }
    #[inline]
    pub fn step(&mut self, x: Complex32) -> f32 {
        let d = x * self.prev.conj();
        self.prev = x;
        d.im.atan2(d.re) * self.gain
    }
    pub fn process(&mut self, input: &[Complex32], out: &mut Vec<f32>) {
        out.extend(input.iter().map(|&x| self.step(x)));
    }
}

/// Running statistics of the demodulated signal (sanity: mean ~ 0 Hz means
/// the DDC centred the carrier; rms ~ dev/sqrt(2) means FM demod worked).
#[derive(Default, Clone, Copy, Debug)]
pub struct DemodStats {
    pub n: u64,
    pub sum: f64,
    pub sumsq: f64,
}

impl DemodStats {
    pub fn push(&mut self, xs: &[f32]) {
        for &x in xs {
            self.sum += x as f64;
            self.sumsq += (x as f64) * (x as f64);
        }
        self.n += xs.len() as u64;
    }
    pub fn mean(&self) -> f64 {
        if self.n == 0 { 0.0 } else { self.sum / self.n as f64 }
    }
    pub fn rms_ac(&self) -> f64 {
        if self.n == 0 {
            return 0.0;
        }
        let m = self.mean();
        (self.sumsq / self.n as f64 - m * m).max(0.0).sqrt()
    }
}

/// Accumulates |X|^2 of successive FFT frames (fft output, unshifted).
pub struct PowerAccum {
    pub len: usize,
    pub psd: Vec<f64>,
    pub frames: u64,
}

impl PowerAccum {
    pub fn new(len: usize) -> Self {
        Self { len, psd: vec![0.0; len], frames: 0 }
    }
    /// `bins.len()` must be a multiple of `len`.
    pub fn push(&mut self, bins: &[Complex32]) {
        for frame in bins.chunks_exact(self.len) {
            for (p, x) in self.psd.iter_mut().zip(frame) {
                *p += x.norm_sqr() as f64;
            }
            self.frames += 1;
        }
    }
    /// Top-`k` local peaks as (offset Hz, dB rel. median).
    pub fn peaks(&self, k: usize, fs: f64) -> Vec<(f64, f64)> {
        let mut sorted = self.psd.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = sorted[sorted.len() / 2].max(1e-30);
        let n = self.len;
        let mut idx: Vec<usize> = (0..n)
            .filter(|&i| {
                let l = self.psd[(i + n - 1) % n];
                let r = self.psd[(i + 1) % n];
                self.psd[i] >= l && self.psd[i] >= r
            })
            .collect();
        idx.sort_by(|&a, &b| self.psd[b].partial_cmp(&self.psd[a]).unwrap());
        idx.truncate(k);
        let mut v: Vec<(f64, f64)> = idx
            .into_iter()
            .map(|i| {
                let bin = if i < n / 2 { i as f64 } else { i as f64 - n as f64 };
                (bin * fs / n as f64, 10.0 * (self.psd[i] / median).log10())
            })
            .collect();
        v.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        v
    }
}

/// Chain specifications cycled by both implementations: different centre and
/// output rate each time.
#[derive(Clone, Copy, Debug)]
pub struct ChainSpec {
    pub name: &'static str,
    pub offset_hz: f64,
    pub decim: usize,
    pub expect_rms_hz: f64,
}

pub fn chain_specs() -> [ChainSpec; 2] {
    let c = crate::synth::carriers();
    [
        ChainSpec {
            name: "nbfm@-1.5MHz/50kSps",
            offset_hz: c[1].offset_hz,
            decim: 400,
            expect_rms_hz: c[1].dev_hz / 2f64.sqrt(),
        },
        ChainSpec {
            name: "wbfm@+2.5MHz/250kSps",
            offset_hz: c[2].offset_hz,
            decim: 80,
            expect_rms_hz: c[2].dev_hz / 2f64.sqrt(),
        },
    ]
}

/// CPU time consumed by the calling thread (seconds).
pub fn thread_cpu_s() -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 * 1e-9
}

/// CPU time (user + sys) consumed by the whole process (seconds).
pub fn process_cpu_s() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 * 1e-6;
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

/// Latency percentiles helper: returns (p50, p99, max) of a sample set.
pub fn pctl(v: &[f64]) -> (f64, f64, f64) {
    if v.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |q: f64| s[((s.len() - 1) as f64 * q).round() as usize];
    (at(0.5), at(0.99), *s.last().unwrap())
}
