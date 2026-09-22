//! Compute-provider conformance suite (T-041 / T-046, ADR-0007).
//!
//! Every compute provider — CPU reference, CPU multi-threaded, Accelerate, wgpu GPU, and later
//! CUDA (T-026) — must pass the same checks before [`crate::compute::Compute`] may select it.
//! The suites are generic over the provider seams:
//!
//! - [`fft_suite`] over [`FftBackend`];
//! - [`spectral_suite`] over [`SpectralBackend`] (driven through [`StftProcessor`], so stream
//!   semantics are checked too);
//! - [`pfb_suite`] over [`PfbBackend`].
//!
//! Each check compares the provider with the CPU reference ([`CpuFft`], [`StftProcessor::new`],
//! [`Pfb`]) under the one set of [`TOLERANCES`], and adds absolute checks (tone gain, Parseval,
//! noise level, adjacent-channel rejection, phase continuity). A factory may refuse a size it does
//! not support by returning `Err`; the suite records that as *unsupported* (the registry then
//! falls back to the CPU), but a panic, a wrong length or a wrong answer fails.
//!
//! Test binaries run the suites: the CPU providers in the default `cargo test`, feature-gated
//! providers when compiled in (skipping with a logged reason when no device is present).

use std::fmt::Write as _;
use std::panic::{AssertUnwindSafe, catch_unwind};

use hk_core::{BlockHeader, Discontinuity, ProvenanceHandle};
use hk_model::{ClockSource, Provenance, SampleTime, Timestamp, TimestampMethod, Tune};
use num_complex::{Complex, Complex32};

use crate::channelizer::{ChannelTime, Pfb, PfbBackend, PfbConfig};
use crate::compute::SpectralBackend;
use crate::fft::{CpuFft, FftBackend};
use crate::stft::{InputInfo, SpectrumFrame, StftConfig, StftProcessor};
use crate::synth::{self, Rng};
use crate::welch::WelchConfig;
use crate::window::WindowKind;

/// Numeric parity and absolute bounds, defined once for every provider.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tolerances {
    /// Complex outputs (FFT bins, PFB channel samples): `max_k |got − ref| / max(|ref_k|,
    /// rms(ref))`.
    pub complex_rel: f64,
    /// Power (PSD, holds): max per-bin `|10·log10(got/ref)|`, dB.
    pub power_db: f64,
    /// Spectral kurtosis: max per-bin `|got − ref|`.
    pub sk_abs: f64,
    /// Absolute gain checks (bin-centred tone, channel-centre tone), dB.
    pub gain_db: f64,
    /// Absolute noise-level check (mean PSD vs `σ²/fs`), dB.
    pub noise_db: f64,
    /// Adjacent-channel rejection: a channel-centre tone in any other channel, dB relative to
    /// the tone (the prototype is designed for 60 dB).
    pub adjacent_db: f64,
    /// Phase continuity: max deviation of the per-sample phase step, radians.
    pub phase_rad: f64,
    /// Parseval: relative error of `Σ|X|²/N` against `Σ|x|²`.
    pub parseval_rel: f64,
    /// Power comparisons treat reference values below this fraction of the frame's mean PSD
    /// as that floor (−30 dB): a min-hold bin that fell to 1e-6 of the mean would otherwise turn
    /// single-precision round-off into a large relative error.
    pub power_floor_rel: f64,
}

/// The tolerances every provider is held to.
pub const TOLERANCES: Tolerances = Tolerances {
    complex_rel: 1e-4,
    power_db: 0.01,
    sk_abs: 1e-3,
    gain_db: 0.05,
    noise_db: 0.3,
    adjacent_db: -58.0,
    phase_rad: 1e-3,
    parseval_rel: 1e-5,
    power_floor_rel: 1e-3,
};

/// Makes an FFT of a length, or refuses it (unsupported).
pub type FftFactory<'a> = &'a dyn Fn(usize) -> Result<Box<dyn FftBackend>, String>;
/// Makes a spectral backend for an STFT config, or refuses it (unsupported).
pub type SpectralFactory<'a> = &'a dyn Fn(&StftConfig) -> Result<Box<dyn SpectralBackend>, String>;
/// Makes a PFB for a config, or refuses it (unsupported).
pub type PfbFactory<'a> = &'a dyn Fn(&PfbConfig) -> Result<Box<dyn PfbBackend>, String>;

/// One check's outcome: `Ok(detail)` or `Err(failure)`.
#[derive(Clone, Debug)]
pub struct Check {
    /// Check name (stable; listed in ADR-0007).
    pub name: &'static str,
    /// Measured detail on success, the failure otherwise.
    pub result: Result<String, String>,
}

/// A suite run for one provider.
#[derive(Clone, Debug)]
pub struct Report {
    /// `fft`, `spectral` or `pfb`.
    pub suite: &'static str,
    /// Provider label.
    pub provider: String,
    /// Checks in order.
    pub checks: Vec<Check>,
}

impl Report {
    /// Every check passed.
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|c| c.result.is_ok())
    }

    /// A table of the checks.
    pub fn render(&self) -> String {
        let mut s = format!("conformance {} / {}\n", self.suite, self.provider);
        for c in &self.checks {
            let (mark, text) = match &c.result {
                Ok(d) => ("PASS", d.as_str()),
                Err(e) => ("FAIL", e.as_str()),
            };
            let _ = writeln!(s, "  {mark} {:<34} {text}", c.name);
        }
        s
    }

    /// Prints the table and panics if any check failed.
    pub fn assert_passed(&self) {
        let table = self.render();
        eprint!("{table}");
        assert!(self.passed(), "provider failed conformance:\n{table}");
    }
}

fn run(name: &'static str, f: impl FnOnce() -> Result<String, String>) -> Check {
    let result = match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(p) => {
            let msg = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "non-string panic".into());
            Err(format!("panicked: {msg}"))
        }
    };
    Check { name, result }
}

macro_rules! ensure {
    ($cond:expr, $($fmt:tt)*) => {
        // Bound first: a NaN comparison is false, so a NaN measurement fails the check.
        let ok: bool = $cond;
        if !ok {
            return Err(format!($($fmt)*));
        }
    };
}

// ---------------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------------

/// A synthetic provenance record (library code: no JSON).
pub fn provenance(center_hz: f64, sample_rate_hz: f64, lna_db: f64) -> ProvenanceHandle {
    ProvenanceHandle::new(Provenance {
        device_id: "synthetic:hk-dsp-conformance".into(),
        tune: Tune {
            center_hz,
            sample_rate_hz,
            lna_db,
            vga_db: 20.0,
            amp_on: false,
            bandwidth_hz: sample_rate_hz * 0.75,
        },
        overload: false,
        quantisation_limited: false,
        noise_sigma_lsb: None,
        temperature_c: None,
        antenna_port: None,
        bias_tee: hk_model::BiasTee::Unknown,
        clock_source: ClockSource::Internal,
        clock_locked: true,
        calibration_state_ref: None,
        spur_mask_ref: None,
        timestamp_method: TimestampMethod::Synthetic,
        timestamp_error_budget_ns: Some(0),
        capture_artefacts: Vec::new(),
    })
}

fn header(first: u64, prov: &ProvenanceHandle) -> BlockHeader {
    let fs = prov.tune.sample_rate_hz;
    BlockHeader {
        time: SampleTime {
            sample_index: first,
            host_time: Timestamp::from_unix_nanos((first as f64 * 1e9 / fs).round() as i64),
        },
        provenance: prov.clone(),
        discontinuity: Discontinuity::NONE,
        dropped_before: 0,
    }
}

/// `max_k |got − want| / max(|want_k|, rms(want))`.
pub fn complex_rel(got: &[Complex32], want: &[Complex32]) -> f64 {
    assert_eq!(got.len(), want.len(), "length mismatch");
    if want.is_empty() {
        return 0.0;
    }
    let rms = (want.iter().map(|w| f64::from(w.norm_sqr())).sum::<f64>() / want.len() as f64)
        .sqrt()
        .max(f64::MIN_POSITIVE);
    got.iter()
        .zip(want)
        .map(|(g, w)| {
            let d = f64::from((g - w).norm());
            d / f64::from(w.norm()).max(rms)
        })
        .fold(0.0, f64::max)
}

/// Max per-bin `|10·log10(got/want)|`; both zero counts as equal.
pub fn power_db_err(got: &[f32], want: &[f32]) -> f64 {
    assert_eq!(got.len(), want.len(), "length mismatch");
    got.iter()
        .zip(want)
        .map(|(&g, &w)| {
            if g == w {
                0.0
            } else if g <= 0.0 || w <= 0.0 || !g.is_finite() || !w.is_finite() {
                f64::INFINITY
            } else {
                (10.0 * (f64::from(g) / f64::from(w)).log10()).abs()
            }
        })
        .fold(0.0, f64::max)
}

/// [`power_db_err`] with reference values below `floor` compared at the floor:
/// `10·log10(1 + |got − want| / max(want, floor))`.
pub fn power_db_err_floor(got: &[f32], want: &[f32], floor: f32) -> f64 {
    assert_eq!(got.len(), want.len(), "length mismatch");
    got.iter()
        .zip(want)
        .map(|(&g, &w)| {
            if g == w {
                0.0
            } else if !g.is_finite() || !w.is_finite() {
                f64::INFINITY
            } else {
                let base = f64::from(w.max(floor)).max(f64::MIN_POSITIVE);
                10.0 * (1.0 + f64::from((g - w).abs()) / base).log10()
            }
        })
        .fold(0.0, f64::max)
}

fn bitwise_c(a: &[Complex32], b: &[Complex32]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.re.to_bits() == y.re.to_bits() && x.im.to_bits() == y.im.to_bits())
}

fn tone_at_bin(n: usize, bin: f64, amp: f32) -> Vec<Complex32> {
    (0..n)
        .map(|i| {
            let ph = 2.0 * std::f64::consts::PI * bin * i as f64 / n as f64;
            Complex32::new(amp * ph.cos() as f32, amp * ph.sin() as f32)
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// FFT suite
// ---------------------------------------------------------------------------------------------

fn reference_fft(x: &[Complex32]) -> Vec<Complex32> {
    let mut r = x.to_vec();
    CpuFft::new(x.len()).forward(&mut r);
    r
}

fn provider_fft(make: FftFactory<'_>, x: &[Complex32]) -> Result<Vec<Complex32>, String> {
    let mut fft = make(x.len())?;
    ensure!(
        fft.len() == x.len(),
        "len() {} != requested {}",
        fft.len(),
        x.len()
    );
    let mut g = x.to_vec();
    fft.forward(&mut g);
    Ok(g)
}

/// Runs the FFT checks for one provider.
pub fn fft_suite(provider: &str, make: FftFactory<'_>) -> Report {
    let t = TOLERANCES;
    let checks = vec![
        run("fft_tones_at_known_bins", || {
            let mut worst = 0.0f64;
            for n in [16usize, 64, 1024, 4096] {
                for bin in [0, 1, n / 3, n / 2, n - 1] {
                    let amp = 0.5f32;
                    let x = tone_at_bin(n, bin as f64, amp);
                    let g = provider_fft(make, &x)
                        .map_err(|e| format!("N={n}: power-of-two sizes are required: {e}"))?;
                    let peak = f64::from(g[bin].norm());
                    let want = n as f64 * f64::from(amp);
                    ensure!(
                        ((peak - want) / want).abs() <= t.complex_rel,
                        "N={n} bin {bin}: |X| = {peak}, want {want}"
                    );
                    let leak = g
                        .iter()
                        .enumerate()
                        .filter(|&(k, _)| k != bin)
                        .map(|(_, v)| f64::from(v.norm()))
                        .fold(0.0, f64::max);
                    ensure!(
                        leak <= t.complex_rel * want,
                        "N={n} bin {bin}: leakage {leak:e} into other bins"
                    );
                    worst = worst.max(complex_rel(&g, &reference_fft(&x)));
                }
            }
            ensure!(worst <= t.complex_rel, "parity vs rustfft {worst:e}");
            Ok(format!(
                "bins 0, 1, N/3, N/2 (Nyquist), N-1; parity {worst:.1e}"
            ))
        }),
        run("fft_parseval_noise", || {
            let n = 4096;
            let x = synth::complex_noise(&mut Rng::new(11), n, 0.1);
            let g = provider_fft(make, &x)?;
            let ex: f64 = x.iter().map(|v| f64::from(v.norm_sqr())).sum();
            let eg: f64 = g.iter().map(|v| f64::from(v.norm_sqr())).sum::<f64>() / n as f64;
            let rel = ((eg - ex) / ex).abs();
            ensure!(rel <= t.parseval_rel, "Σ|X|²/N vs Σ|x|²: {rel:e}");
            Ok(format!("rel {rel:.1e}"))
        }),
        run("fft_parity_random", || {
            let mut worst = 0.0f64;
            for n in [256usize, 1024, 4096, 16384] {
                let x = synth::complex_noise(&mut Rng::new(n as u64), n, 1.0);
                let g = provider_fft(make, &x)?;
                worst = worst.max(complex_rel(&g, &reference_fft(&x)));
            }
            ensure!(worst <= t.complex_rel, "parity vs rustfft {worst:e}");
            Ok(format!("N 256..16384, parity {worst:.1e}"))
        }),
        run("fft_odd_and_unsupported_sizes", || {
            let (mut ok, mut refused) = (Vec::new(), Vec::new());
            let mut worst = 0.0f64;
            for n in [4usize, 5, 7, 12, 100, 255, 800, 1000, 1600, 3000] {
                match make(n) {
                    Err(_) => refused.push(n),
                    Ok(mut fft) => {
                        ensure!(fft.len() == n, "N={n}: len() {}", fft.len());
                        let x = synth::complex_noise(&mut Rng::new(n as u64 + 7), n, 1.0);
                        let mut g = x.clone();
                        fft.forward(&mut g);
                        worst = worst.max(complex_rel(&g, &reference_fft(&x)));
                        ok.push(n);
                    }
                }
            }
            ensure!(worst <= t.complex_rel, "parity vs rustfft {worst:e}");
            Ok(format!(
                "supported {ok:?} (parity {worst:.1e}); refused cleanly {refused:?}"
            ))
        }),
        run("fft_batch_equivalence", || {
            let n = 1024;
            let b = 7;
            let x = synth::complex_noise(&mut Rng::new(3), n * b, 1.0);
            let mut fft = make(n)?;
            let mut batch = x.clone();
            fft.forward_batch(&mut batch);
            let mut single = x.clone();
            for seg in single.chunks_exact_mut(n) {
                fft.forward(seg);
            }
            let rel = complex_rel(&batch, &single);
            ensure!(rel <= t.complex_rel, "batch vs single {rel:e}");
            let mut reference = x;
            for seg in reference.chunks_exact_mut(n) {
                CpuFft::new(n).forward(seg);
            }
            let parity = complex_rel(&batch, &reference);
            ensure!(
                parity <= t.complex_rel,
                "batch parity vs rustfft {parity:e}"
            );
            Ok(format!(
                "{b} segments: batch vs single {rel:.1e} ({}), parity {parity:.1e}",
                if bitwise_c(&batch, &single) {
                    "bitwise"
                } else {
                    "within tolerance"
                }
            ))
        }),
        run("fft_determinism", || {
            let n = 4096;
            let x = synth::complex_noise(&mut Rng::new(5), n, 1.0);
            let a = provider_fft(make, &x)?;
            let b = provider_fft(make, &x)?;
            ensure!(bitwise_c(&a, &b), "two runs differ");
            Ok("bitwise identical across runs".into())
        }),
    ];
    Report {
        suite: "fft",
        provider: provider.to_string(),
        checks,
    }
}

// ---------------------------------------------------------------------------------------------
// Spectral (STFT) suite
// ---------------------------------------------------------------------------------------------

/// One pushed chunk of a test stream.
enum Chunk {
    F32(BlockHeader, Vec<Complex32>),
    I8(BlockHeader, Vec<Complex<i8>>),
}

impl Chunk {
    fn info(&self) -> InputInfo<'_> {
        match self {
            Chunk::F32(h, _) | Chunk::I8(h, _) => InputInfo::from(h),
        }
    }
}

fn run_stft(p: &mut StftProcessor, stream: &[Chunk]) -> Vec<SpectrumFrame> {
    let mut frames = Vec::new();
    for c in stream {
        match c {
            Chunk::F32(_, x) => p.push(c.info(), x, |f| frames.push(f.clone())),
            Chunk::I8(_, x) => p.push(c.info(), x, |f| frames.push(f.clone())),
        };
    }
    p.flush(|f| frames.push(f.clone()));
    frames
}

/// Contiguous tone + noise at `fs`, chopped into `sizes` (cycled), `total` samples.
fn tone_noise_stream(fs: f64, total: usize, sizes: &[usize], seed: u64) -> Vec<Chunk> {
    // One handle per rate for the whole run, so separately built streams compare equal.
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Vec<(u64, ProvenanceHandle)>>> =
        std::sync::OnceLock::new();
    let prov = {
        let mut cache = CACHE
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match cache.iter().find(|(bits, _)| *bits == fs.to_bits()) {
            Some((_, p)) => p.clone(),
            None => {
                let p = provenance(100e6, fs, 16.0);
                cache.push((fs.to_bits(), p.clone()));
                p
            }
        }
    };
    let mut rng = Rng::new(seed);
    let mut out = Vec::new();
    let mut index = 0u64;
    let mut k = 0;
    while (index as usize) < total {
        let len = sizes[k % sizes.len()].min(total - index as usize);
        let mut x = synth::complex_noise(&mut rng, len, 1e-3);
        synth::add_into(&mut x, &synth::tone(index, len, fs * 0.123, fs, 0.2, 0.3));
        synth::add_into(&mut x, &synth::tone(index, len, -fs * 0.31, fs, 1e-4, 1.0));
        out.push(Chunk::F32(header(index, &prov), x));
        index += len as u64;
        k += 1;
    }
    out
}

fn compare_frames(got: &[SpectrumFrame], want: &[SpectrumFrame]) -> Result<(f64, f64), String> {
    let t = TOLERANCES;
    ensure!(
        got.len() == want.len(),
        "frame count {} != reference {}",
        got.len(),
        want.len()
    );
    ensure!(!want.is_empty(), "stream produced no frames");
    let (mut worst_db, mut worst_sk) = (0.0f64, 0.0f64);
    for (i, (g, w)) in got.iter().zip(want).enumerate() {
        ensure!(
            g.seq == w.seq
                && g.t == w.t
                && g.sample_count == w.sample_count
                && g.provenance == w.provenance
                && g.provenance_changed == w.provenance_changed
                && g.discontinuity == w.discontinuity
                && g.dropped_samples == w.dropped_samples,
            "frame {i}: metadata differs (t {:?} vs {:?}, flags {:?} vs {:?})",
            g.t,
            w.t,
            g.discontinuity,
            w.discontinuity
        );
        let (gs, ws) = (&g.spectrum, &w.spectrum);
        ensure!(
            gs.resolution == ws.resolution
                && gs.f_center_hz == ws.f_center_hz
                && gs.sample_rate_hz == ws.sample_rate_hz,
            "frame {i}: resolution/centre differs"
        );
        let mean = ws.psd.iter().map(|&v| f64::from(v)).sum::<f64>() / ws.psd.len() as f64;
        let floor = (mean * t.power_floor_rel) as f32;
        worst_db = worst_db
            .max(power_db_err_floor(&gs.psd, &ws.psd, floor))
            .max(power_db_err_floor(&gs.max_hold, &ws.max_hold, floor))
            .max(power_db_err_floor(&gs.min_hold, &ws.min_hold, floor));
        for (a, b) in gs.sk.iter().zip(&ws.sk) {
            if !(a.is_nan() && b.is_nan()) {
                worst_sk = worst_sk.max(f64::from((a - b).abs()));
            }
        }
    }
    ensure!(
        worst_db <= t.power_db,
        "power error {worst_db:.2e} dB > {}",
        t.power_db
    );
    ensure!(
        worst_sk <= t.sk_abs,
        "SK error {worst_sk:.2e} > {}",
        t.sk_abs
    );
    Ok((worst_db, worst_sk))
}

fn provider_stft(make: SpectralFactory<'_>, config: StftConfig) -> Result<StftProcessor, String> {
    let backend = make(&config)?;
    ensure!(
        backend.fft_len() == config.welch.fft_len,
        "fft_len() {} != {}",
        backend.fft_len(),
        config.welch.fft_len
    );
    StftProcessor::with_spectral(config, backend).map_err(|e| e.to_string())
}

/// Runs the spectral (STFT) checks for one provider.
pub fn spectral_suite(provider: &str, make: SpectralFactory<'_>) -> Report {
    let t = TOLERANCES;
    let fs = 1e6;
    let checks = vec![
        run("spectral_window_and_overlap", || {
            let configs = [
                (WindowKind::Hann, 1024usize, 512usize, 4usize),
                (WindowKind::Hann, 4096, 2048, 8),
                (WindowKind::Hann, 2048, 1536, 4),
                (WindowKind::BlackmanHarris, 256, 0, 3),
                (WindowKind::FlatTop, 1000, 700, 5),
            ];
            let stream = tone_noise_stream(fs, 60_000, &[3000, 1, 4999, 12_000, 17], 1);
            let (mut worst_db, mut worst_sk) = (0.0f64, 0.0f64);
            let mut refused = Vec::new();
            for (window, n, overlap, k) in configs {
                let config = StftConfig::new(
                    WelchConfig {
                        window,
                        overlap,
                        ..WelchConfig::new(n)
                    },
                    k,
                );
                let mut p = match provider_stft(make, config) {
                    Ok(p) => p,
                    Err(e) if !n.is_power_of_two() => {
                        refused.push(format!("{n}: {e}"));
                        continue;
                    }
                    Err(e) => return Err(format!("N={n}: power-of-two sizes are required: {e}")),
                };
                let got = run_stft(&mut p, &stream);
                let want = run_stft(&mut StftProcessor::new(config).unwrap(), &stream);
                let (db, sk) = compare_frames(&got, &want)
                    .map_err(|e| format!("{window:?} N={n} overlap={overlap}: {e}"))?;
                worst_db = worst_db.max(db);
                worst_sk = worst_sk.max(sk);
            }
            Ok(format!(
                "Hann 50/75 %, BH 0 %, flat-top 1000/700: {worst_db:.1e} dB, SK {worst_sk:.1e}{}",
                if refused.is_empty() {
                    String::new()
                } else {
                    format!("; refused {refused:?}")
                }
            ))
        }),
        run("spectral_tone_and_noise_calibration", || {
            let n = 1024;
            let config = StftConfig::new(
                WelchConfig {
                    overlap: 0,
                    ..WelchConfig::new(n)
                },
                16,
            );
            let bin_hz = fs / n as f64;
            let total = n * 32;
            let prov = provenance(100e6, fs, 16.0);
            let mut x = synth::complex_noise(&mut Rng::new(2), total, 1e-2);
            synth::add_into(&mut x, &synth::tone(0, total, 100.0 * bin_hz, fs, 1.0, 0.0));
            let stream = [Chunk::F32(header(0, &prov), x)];
            let mut p = provider_stft(make, config)?;
            let frames = run_stft(&mut p, &stream);
            ensure!(frames.len() == 2, "{} frames", frames.len());
            let s = &frames[1].spectrum;
            let tone_bin = n / 2 + 100;
            let rbw = s.resolution.rbw_hz;
            let tone_db = 10.0 * (f64::from(s.psd[tone_bin]) * rbw).log10();
            ensure!(
                tone_db.abs() <= t.gain_db + 10.0 * (1.0 + 1e-2 * 1.5 / n as f64).log10(),
                "full-scale bin-centred tone reads {tone_db:.4} dBFS/bin"
            );
            let noise: Vec<f64> = s
                .psd
                .iter()
                .enumerate()
                .filter(|&(i, _)| i.abs_diff(tone_bin) > 4 && i.abs_diff(n / 2) > 1)
                .map(|(_, &v)| f64::from(v))
                .collect();
            let mean = noise.iter().sum::<f64>() / noise.len() as f64;
            let err = 10.0 * (mean / (1e-2 / fs)).log10();
            ensure!(err.abs() <= t.noise_db, "noise PSD off by {err:.3} dB");
            Ok(format!("tone {tone_db:+.4} dBFS/bin, noise {err:+.3} dB"))
        }),
        run("spectral_edge_bins", || {
            let n = 256;
            let config = StftConfig::new(
                WelchConfig {
                    overlap: 0,
                    window: WindowKind::FlatTop,
                    ..WelchConfig::new(n)
                },
                2,
            );
            let prov = provenance(100e6, fs, 16.0);
            let bin_hz = fs / n as f64;
            for (offset_bins, want_bin) in [
                (-(n as f64) / 2.0, 0usize),
                (0.0, n / 2),
                (n as f64 / 2.0 - 1.0, n - 1),
            ] {
                let x = synth::tone(0, 2 * n, offset_bins * bin_hz, fs, 0.5, 0.0);
                let mut p = provider_stft(make, config)?;
                let frames = run_stft(&mut p, &[Chunk::F32(header(0, &prov), x)]);
                ensure!(frames.len() == 1, "{} frames", frames.len());
                let psd = &frames[0].spectrum.psd;
                let argmax = (0..n).max_by(|&a, &b| psd[a].total_cmp(&psd[b])).unwrap();
                ensure!(
                    argmax == want_bin,
                    "tone at {offset_bins} bins peaks in bin {argmax}, want {want_bin}"
                );
            }
            Ok("-fs/2 -> bin 0, DC -> N/2, +fs/2-Δ -> N-1".into())
        }),
        run("spectral_stream_semantics", || {
            let config = StftConfig::new(WelchConfig::new(512), 4);
            let a = provenance(100e6, fs, 16.0);
            let retuned = provenance(101e6, fs, 16.0);
            let gain = provenance(101e6, fs, 24.0);
            let rate = provenance(101e6, 2e6, 24.0);
            let mut rng = Rng::new(9);
            let mut stream = Vec::new();
            let mut index = 0u64;
            let sizes = [1usize, 700, 3000, 5, 2048, 9999, 64];
            for step in 0..42 {
                let len = sizes[step % sizes.len()];
                let prov = match step {
                    0..=9 => &a,
                    10..=19 => &retuned,
                    20..=29 => &gain,
                    _ => &rate,
                };
                let mut h = header(index, prov);
                if step == 6 {
                    h.time.sample_index += 300;
                    index += 300;
                }
                if step == 15 {
                    h.dropped_before = 40;
                }
                let x = synth::complex_noise(&mut rng, len, 1e-2);
                stream.push(if step % 3 == 0 {
                    Chunk::I8(h, synth::quantize_ci8(&x).0)
                } else {
                    Chunk::F32(h, x)
                });
                index += len as u64;
            }
            let mut p = provider_stft(make, config)?;
            let got = run_stft(&mut p, &stream);
            let want = run_stft(&mut StftProcessor::new(config).unwrap(), &stream);
            let (db, sk) = compare_frames(&got, &want)?;
            let stats = (p.stats(), StftProcessor::new(config).unwrap());
            let mut r = stats.1;
            run_stft(&mut r, &stream);
            ensure!(
                stats.0 == r.stats(),
                "stats {:?} vs {:?}",
                stats.0,
                r.stats()
            );
            Ok(format!(
                "{} frames; gap, drop, retune, gain, rate change, ci8: {db:.1e} dB, SK {sk:.1e}",
                got.len()
            ))
        }),
        run("spectral_batch_vs_single_segment", || {
            let config = StftConfig::new(WelchConfig::new(256), 4);
            let big = tone_noise_stream(fs, 32_768, &[32_768], 4);
            let small = tone_noise_stream(fs, 32_768, &[128], 4);
            let mut p = provider_stft(make, config)?;
            let a = run_stft(&mut p, &big);
            let mut p = provider_stft(make, config)?;
            let b = run_stft(&mut p, &small);
            let (db, sk) = compare_frames(&a, &b)?;
            let want = run_stft(&mut StftProcessor::new(config).unwrap(), &big);
            compare_frames(&a, &want)?;
            Ok(format!(
                "one 32768-sample push vs 128-sample pushes: {db:.1e} dB, SK {sk:.1e}"
            ))
        }),
        run("spectral_determinism", || {
            let config = StftConfig::new(WelchConfig::new(2048), 8);
            let stream = tone_noise_stream(fs, 50_000, &[6000, 333], 6);
            let a = run_stft(&mut provider_stft(make, config)?, &stream);
            let b = run_stft(&mut provider_stft(make, config)?, &stream);
            ensure!(a == b, "two runs differ");
            Ok(format!("{} frames bitwise identical", a.len()))
        }),
    ];
    Report {
        suite: "spectral",
        provider: provider.to_string(),
        checks,
    }
}

// ---------------------------------------------------------------------------------------------
// PFB suite
// ---------------------------------------------------------------------------------------------

/// A non-empty PFB output block, owned.
#[derive(Clone, Debug)]
struct Block {
    time: ChannelTime,
    rate: f64,
    flags: Discontinuity,
    dropped: u64,
    frames: usize,
    data: Vec<Complex32>,
}

fn keep(out: &crate::channelizer::PfbOutput<'_>, blocks: &mut Vec<Block>) {
    if out.frames > 0 {
        blocks.push(Block {
            time: out.header.time,
            rate: out.header.sample_rate_hz,
            flags: out.header.discontinuity,
            dropped: out.header.dropped_before,
            frames: out.frames,
            data: out.samples().to_vec(),
        });
    }
}

fn run_pfb(p: &mut dyn PfbBackend, stream: &[Chunk]) -> Vec<Block> {
    let mut blocks = Vec::new();
    for c in stream {
        match c {
            Chunk::F32(_, x) => keep(&p.process_c32(c.info(), x), &mut blocks),
            Chunk::I8(_, x) => keep(&p.process_ci8(c.info(), x), &mut blocks),
        }
    }
    while let Some(out) = p.flush() {
        keep(&out, &mut blocks);
    }
    blocks
}

fn compare_blocks(got: &[Block], want: &[Block]) -> Result<f64, String> {
    ensure!(
        got.len() == want.len(),
        "non-empty block count {} != reference {}",
        got.len(),
        want.len()
    );
    ensure!(!want.is_empty(), "stream produced no output");
    let mut worst = 0.0f64;
    for (i, (g, w)) in got.iter().zip(want).enumerate() {
        ensure!(
            g.time == w.time
                && g.rate == w.rate
                && g.flags == w.flags
                && g.dropped == w.dropped
                && g.frames == w.frames,
            "block {i}: header differs ({:?}/{:?} vs {:?}/{:?})",
            g.time,
            g.flags,
            w.time,
            w.flags
        );
        worst = worst.max(complex_rel(&g.data, &w.data));
    }
    ensure!(
        worst <= TOLERANCES.complex_rel,
        "sample error {worst:.2e} > {}",
        TOLERANCES.complex_rel
    );
    Ok(worst)
}

fn provider_pfb(make: PfbFactory<'_>, config: &PfbConfig) -> Result<Box<dyn PfbBackend>, String> {
    let p = make(config)?;
    ensure!(p.config() == config, "provider changed the config");
    Ok(p)
}

fn reference_pfb(config: &PfbConfig) -> Box<dyn PfbBackend> {
    Box::new(Pfb::new(config.clone()).expect("valid reference config"))
}

/// One channel's samples across blocks (single active channel configs only).
fn channel_series(blocks: &[Block]) -> Vec<Complex32> {
    blocks.iter().flat_map(|b| b.data.iter().copied()).collect()
}

/// Runs the PFB checks for one provider.
pub fn pfb_suite(provider: &str, make: PfbFactory<'_>) -> Report {
    let t = TOLERANCES;
    let fs = 1e6;
    let checks = vec![
        run("pfb_parity_reference", || {
            let configs = [
                PfbConfig::new(64),
                PfbConfig {
                    active: Some(vec![0, 1, 100, 128, 255]),
                    ..PfbConfig::new(256)
                },
                PfbConfig {
                    raster_offset_hz: 1234.5,
                    ..PfbConfig::new(100)
                },
                PfbConfig {
                    raster_offset_hz: -625.0,
                    active: Some(vec![399, 400, 401, 7]),
                    ..PfbConfig::new(800)
                },
            ];
            let stream = tone_noise_stream(fs, 120_000, &[20_000, 1, 777, 31_000, 4096], 21);
            let mut worst = 0.0f64;
            let mut refused = Vec::new();
            for config in &configs {
                let mut p = match provider_pfb(make, config) {
                    Ok(p) => p,
                    Err(e) => {
                        refused.push(format!("M={}: {e}", config.channels));
                        continue;
                    }
                };
                let got = run_pfb(p.as_mut(), &stream);
                let want = run_pfb(reference_pfb(config).as_mut(), &stream);
                worst = worst.max(
                    compare_blocks(&got, &want)
                        .map_err(|e| format!("M={}: {e}", config.channels))?,
                );
            }
            ensure!(
                refused.len() < configs.len(),
                "every config refused: {refused:?}"
            );
            Ok(format!(
                "M 64 / 256 subset / 100 raster / 800 raster subset: {worst:.1e}{}",
                if refused.is_empty() {
                    String::new()
                } else {
                    format!("; refused {refused:?}")
                }
            ))
        }),
        run("pfb_channel_centre_and_gain", || {
            let m = 64;
            let c = 40;
            let config = PfbConfig::new(m);
            let power = 0.25;
            let phase = 0.7;
            let offset = config.channel_offset_hz(c, fs);
            let total = 40_000;
            let prov = provenance(100e6, fs, 16.0);
            let x = synth::tone(0, total, offset, fs, power, phase);
            let stream = [Chunk::F32(header(0, &prov), x)];
            let mut p = provider_pfb(make, &config)?;
            let blocks = run_pfb(p.as_mut(), &stream);
            ensure!(blocks.len() == 1, "{} blocks", blocks.len());
            let b = &blocks[0];
            let ch: Vec<Complex32> = (0..b.frames).map(|f| b.data[f * m + c]).collect();
            let mean_power =
                ch.iter().map(|v| f64::from(v.norm_sqr())).sum::<f64>() / ch.len() as f64;
            let gain_db = 10.0 * (mean_power / power).log10();
            ensure!(gain_db.abs() <= t.gain_db, "centre gain {gain_db:.4} dB");
            let mut worst_phase = 0.0f64;
            for v in &ch {
                let d = (f64::from(v.arg()) - phase + std::f64::consts::PI)
                    .rem_euclid(2.0 * std::f64::consts::PI)
                    - std::f64::consts::PI;
                worst_phase = worst_phase.max(d.abs());
            }
            ensure!(
                worst_phase <= t.phase_rad,
                "centre tone phase deviates {worst_phase:.2e} rad (not at DC)"
            );
            Ok(format!(
                "gain {gain_db:+.4} dB, output at DC with tone phase (±{worst_phase:.1e} rad)"
            ))
        }),
        run("pfb_adjacent_channel_rejection", || {
            let m = 64;
            let c = 20;
            let config = PfbConfig::new(m);
            let offset = config.channel_offset_hz(c, fs);
            let prov = provenance(100e6, fs, 16.0);
            let x = synth::tone(0, 40_000, offset, fs, 1.0, 0.0);
            let mut p = provider_pfb(make, &config)?;
            let blocks = run_pfb(p.as_mut(), &[Chunk::F32(header(0, &prov), x)]);
            ensure!(blocks.len() == 1, "{} blocks", blocks.len());
            let b = &blocks[0];
            let chan_power = |j: usize| {
                (0..b.frames)
                    .map(|f| f64::from(b.data[f * m + j].norm_sqr()))
                    .sum::<f64>()
                    / b.frames as f64
            };
            let tone = chan_power(c);
            let mut worst = f64::NEG_INFINITY;
            let mut worst_ch = 0;
            for j in (0..m).filter(|&j| j != c) {
                let rel = 10.0 * (chan_power(j) / tone).log10();
                if rel > worst {
                    worst = rel;
                    worst_ch = j;
                }
            }
            ensure!(
                worst <= t.adjacent_db,
                "channel {worst_ch} holds {worst:.1} dB of the tone"
            );
            Ok(format!("worst other channel {worst_ch}: {worst:.1} dB"))
        }),
        run("pfb_phase_continuity_across_blocks", || {
            let m = 128;
            let c = 70;
            let config = PfbConfig {
                active: Some(vec![c]),
                raster_offset_hz: 250.0,
                ..PfbConfig::new(m)
            };
            let spacing = config.channel_spacing_hz(fs);
            let frac = 0.3;
            let offset = config.channel_offset_hz(c, fs) + frac * spacing;
            let prov = provenance(100e6, fs, 16.0);
            let sizes = [1usize, 999, 4096, 63, 10_000, 7];
            let mut stream = Vec::new();
            let mut index = 0u64;
            for k in 0..30 {
                let len = sizes[k % sizes.len()];
                stream.push(Chunk::F32(
                    header(index, &prov),
                    synth::tone(index, len, offset, fs, 0.5, 0.1),
                ));
                index += len as u64;
            }
            let mut p = provider_pfb(make, &config)?;
            let blocks = run_pfb(p.as_mut(), &stream);
            let series = channel_series(&blocks);
            ensure!(series.len() > 100, "only {} samples", series.len());
            // Output rate is 2·spacing, so the step is 2π·frac·spacing / (2·spacing).
            let want = std::f64::consts::PI * frac;
            let mut worst = 0.0f64;
            for w in series.windows(2) {
                let step = f64::from((w[1] * w[0].conj()).arg());
                worst = worst.max((step - want).abs());
            }
            ensure!(
                worst <= t.phase_rad,
                "phase step deviates {worst:.2e} rad across {} blocks",
                blocks.len()
            );
            let want_blocks = run_pfb(reference_pfb(&config).as_mut(), &stream);
            compare_blocks(&blocks, &want_blocks)?;
            Ok(format!(
                "{} samples over {} blocks, step error {worst:.1e} rad",
                series.len(),
                blocks.len()
            ))
        }),
        run("pfb_retune_and_reset", || {
            let config = PfbConfig {
                active: Some(vec![3, 16, 30]),
                ..PfbConfig::new(32)
            };
            let a = provenance(100e6, fs, 16.0);
            let b = provenance(100.5e6, fs, 16.0);
            let mut rng = Rng::new(31);
            let mut stream = Vec::new();
            let mut index = 0u64;
            for k in 0..12 {
                let prov = if k < 6 { &a } else { &b };
                let len = 1500 + 37 * k;
                stream.push(Chunk::F32(
                    header(index, prov),
                    synth::complex_noise(&mut rng, len, 1e-2),
                ));
                index += len as u64;
            }
            let mut p = provider_pfb(make, &config)?;
            let got = run_pfb(p.as_mut(), &stream);
            let want = run_pfb(reference_pfb(&config).as_mut(), &stream);
            compare_blocks(&got, &want)?;
            ensure!(
                got.iter()
                    .any(|blk| blk.flags.contains(Discontinuity::RETUNE)),
                "no block carries RETUNE"
            );
            p.reset();
            let again = run_pfb(p.as_mut(), &stream);
            let mut r = reference_pfb(&config);
            run_pfb(r.as_mut(), &stream);
            r.reset();
            let want_again = run_pfb(r.as_mut(), &stream);
            compare_blocks(&again, &want_again).map_err(|e| format!("after reset(): {e}"))?;
            Ok(format!(
                "{} blocks; RETUNE flagged; identical after reset()",
                got.len()
            ))
        }),
        run("pfb_block_chopping_invariance", || {
            let config = PfbConfig {
                raster_offset_hz: 3000.0,
                ..PfbConfig::new(50)
            };
            let one = tone_noise_stream(fs, 50_000, &[50_000], 41);
            let many = tone_noise_stream(fs, 50_000, &[3, 1024, 25, 7777], 41);
            let mut p = provider_pfb(make, &config)?;
            let a = channel_series(&run_pfb(p.as_mut(), &one));
            let mut p = provider_pfb(make, &config)?;
            let b = channel_series(&run_pfb(p.as_mut(), &many));
            ensure!(a.len() == b.len(), "{} vs {} samples", a.len(), b.len());
            let rel = complex_rel(&a, &b);
            ensure!(rel <= t.complex_rel, "chopped vs whole {rel:e}");
            Ok(format!(
                "{} samples, chopped vs whole {rel:.1e} ({})",
                a.len(),
                if bitwise_c(&a, &b) {
                    "bitwise"
                } else {
                    "within tolerance"
                }
            ))
        }),
        run("pfb_determinism", || {
            let config = PfbConfig::new(256);
            let stream = tone_noise_stream(fs, 60_000, &[16_384, 5000], 51);
            let a = channel_series(&run_pfb(provider_pfb(make, &config)?.as_mut(), &stream));
            let b = channel_series(&run_pfb(provider_pfb(make, &config)?.as_mut(), &stream));
            ensure!(bitwise_c(&a, &b), "two runs differ");
            Ok(format!("{} samples bitwise identical", a.len()))
        }),
        run("pfb_invalid_config_refused", || {
            for bad in [PfbConfig::new(49), PfbConfig::new(0)] {
                ensure!(make(&bad).is_err(), "M={} accepted", bad.channels);
            }
            Ok("odd and zero channel counts refused without panicking".into())
        }),
    ];
    Report {
        suite: "pfb",
        provider: provider.to_string(),
        checks,
    }
}
