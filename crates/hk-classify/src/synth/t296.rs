//! T-296: where the synthetic `psk-qam` grid's pre-receiver distortion came from.
//!
//! T-291 established that it is not the receiver: an oracle *given* the true symbol period, timing
//! phase and roll-off still saw 12.35 % EVM on the shipped grid, and ablating carrier offset,
//! quantisation and IQ imbalance each moved the residual by under 0.4 N₀. **Both halves of that
//! were artefacts of a saturated statistic.** T-291 measured the distance from the recovered cloud
//! to the *nearest point of a candidate constellation*, which stops growing once the cloud is a
//! ring — a denser constellation always has a nearby point — so it capped near 15 N₀ and every
//! ablation of it read flat.
//!
//! Measured instead against the **transmitted symbols**, nothing saturates, and two generator
//! defects are visible:
//!
//! 1. [`super::recentre_offset_hz`] was the *strongest bin* for every class. On a flat-spectrum
//!    emission that is a coin toss among hundreds of equal bins, so the harness rotated each
//!    snippet by an arbitrary fraction of its own symbol rate: **66.7 % ± 46.8 % EVM**.
//! 2. [`super::shaped_linear`] placed each symbol at the nearest whole sample, which is per-symbol
//!    timing jitter of up to ±0.5 sample: **2.59 % EVM**, and unreachable by any uniform-grid
//!    oracle because the instants are genuinely not uniformly spaced.
//!
//! Two things make the measurement trustworthy:
//!
//! - [`t296_the_replica_is_the_shipped_generator`] asserts the replica with every stage on is
//!   **bit-identical** to `generate(Class::Qpsk, …).symbol_samples`. An ablation of a replica that
//!   has drifted from the thing it models measures nothing.
//! - The EVM is computed against the **known transmitted symbols** with a matched filter evaluated
//!   at arbitrary fractional delay — not against the nearest point of a candidate constellation,
//!   which is the statistic whose saturation hid this for two tasks.

use super::*;

/// [`super::rrc_at`] tabulated on a 1/4096-symbol grid: the pulse is band-limited to about one
/// cycle per symbol, so linear interpolation of this table is exact to ~1e-7 and the oracle search
/// below becomes affordable.
struct Pulse {
    step: f64,
    v: Vec<f64>,
}

impl Pulse {
    fn new(alpha: f64) -> Self {
        let step = 1.0 / 4096.0;
        let m = (2.0 * RRC_SPAN / step).round() as usize + 2;
        let v = (0..m)
            .map(|i| rrc_at(-RRC_SPAN + i as f64 * step, alpha))
            .collect();
        Self { step, v }
    }

    fn at(&self, t: f64) -> f64 {
        if t <= -RRC_SPAN || t >= RRC_SPAN {
            return 0.0;
        }
        let u = (t + RRC_SPAN) / self.step;
        let i = u as usize;
        let f = u - i as f64;
        self.v[i] * (1.0 - f) + self.v[i + 1] * f
    }
}

/// Which recentring rule the build applies.
#[derive(Clone, Copy, PartialEq)]
enum Recentre {
    /// No recentring at all.
    Off,
    /// **The defect**: the strongest PSD bin, as the harness did before T-296.
    Peak,
    /// The shipped rule: the carrier line where there is one, the spectral centroid where there is
    /// not ([`super::recentre_offset_hz`]).
    Shipped,
}

/// Which stages of [`super::generate`] this build applies.
#[derive(Clone, Copy)]
struct Stages {
    /// Place each symbol at the **nearest whole sample**, as `shaped_linear` did before T-296,
    /// rather than at its exact fractional instant.
    old_placement: bool,
    recentre: Recentre,
    /// Take the channel-filter cutoff, the recentring offset and the decimation from the
    /// **gate-SNR reference snippet**, as the generator does after T-564, rather than re-measuring
    /// them on the delivered (rung-SNR) snippet.
    gate_reference_geometry: bool,
    noise: bool,
    lo: bool,
    iq: bool,
    quant: bool,
    chan: bool,
    decim: bool,
}

impl Stages {
    /// The generator as it ships **after** this commit.
    const SHIPPED: Stages = Stages {
        old_placement: false,
        recentre: Recentre::Shipped,
        gate_reference_geometry: true,
        noise: true,
        lo: true,
        iq: true,
        quant: true,
        chan: true,
        decim: true,
    };
    /// The generator as it shipped **before** this commit: the grid every density was fitted on.
    const AS_WAS: Stages = Stages {
        old_placement: true,
        recentre: Recentre::Peak,
        gate_reference_geometry: false,
        ..Stages::SHIPPED
    };
    const NONE: Stages = Stages {
        old_placement: false,
        recentre: Recentre::Off,
        gate_reference_geometry: true,
        noise: false,
        lo: false,
        iq: false,
        quant: false,
        chan: false,
        decim: false,
    };
}

/// One built waveform plus everything needed to find its symbol instants again.
struct Built {
    x: Vec<Complex32>,
    /// Samples per symbol at the **generation** rate.
    sps_gen: f64,
    alpha: f64,
    rate: f64,
    /// The transmitted symbols: the reference the EVM is measured against.
    syms: Vec<Complex64>,
    /// Generation-rate index of `x[0]` (the channel filter trims its own transient).
    origin: f64,
    /// Generation-rate samples per element of `x` (decimation).
    stride: f64,
    /// What the **old** argmax rule reported at the recentring step.
    peak_hz: f64,
    /// What the **shipped** centroid rule reports at the same step.
    centroid_hz: f64,
}

/// `shaped_linear` exactly as it was before T-296: symbols written into the nearest whole sample of
/// an upsampled buffer, then filtered with a fixed unit-energy tap set.
fn old_rounded_shaped(n: usize, sps: f64, alpha: f64, syms: &[Complex64]) -> Vec<Complex64> {
    let nt = (2.0 * RRC_SPAN * sps) as usize | 1;
    let mid = (nt / 2) as f64;
    let mut taps: Vec<f64> = (0..nt)
        .map(|i| rrc_at((i as f64 - mid) / sps, alpha))
        .collect();
    let e: f64 = taps.iter().map(|v| v * v).sum::<f64>().sqrt();
    taps.iter_mut().for_each(|v| *v /= e);
    let mut up = vec![Complex64::new(0.0, 0.0); n + taps.len()];
    for (k, s) in syms.iter().enumerate() {
        let i = (k as f64 * sps).round() as usize;
        if i < up.len() {
            up[i] = *s;
        }
    }
    let d = taps.len() / 2;
    (0..n)
        .map(|i| {
            taps.iter()
                .enumerate()
                .map(|(m, w)| {
                    let j = i + d;
                    if j >= m && j - m < up.len() {
                        up[j - m] * *w
                    } else {
                        Complex64::new(0.0, 0.0)
                    }
                })
                .sum::<Complex64>()
        })
        .collect()
}

/// `peak_offset_hz` exactly as it was before T-296: the offset of the strongest PSD bin. Kept here,
/// test-only, to quantify what removing it bought.
fn old_peak_offset_hz(samples: &[Complex32], fs: f64) -> f64 {
    let fft_len = (samples.len() / 8).next_power_of_two().clamp(64, 2048);
    let cfg = hk_dsp::WelchConfig {
        fft_len,
        overlap: fft_len / 2,
        window: hk_dsp::WindowKind::Hann,
        holds: false,
        spectral_kurtosis: false,
    };
    let Ok(s) = hk_dsp::welch(samples, fs, 0.0, &cfg) else {
        return 0.0;
    };
    let peak = s
        .psd
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.total_cmp(b))
        .map_or(s.psd.len() / 2, |(i, _)| i);
    s.bin_offset_hz(peak)
}

/// A faithful replica of `generate(Class::Qpsk, …)`, stage by stage.
fn build(seed: u64, snr_db: f64, st: Stages) -> Built {
    let cfg = SynthConfig::new(snr_db, seed);
    let mut rng = Rng::new(cfg.seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ Class::Qpsk as u64);
    let n = cfg.samples;
    let fs = cfg.sample_rate_hz;
    let rate = 25e3 + 75e3 * rng.unit();
    // `linear()`: sps, roll-off, then the symbol draw, in that order.
    let sps = (fs / rate).max(2.0);
    let alpha = 0.25 + 0.2 * rng.unit();
    let taps_len = (2.0 * RRC_SPAN * sps) as usize | 1;
    let count = (n as f64 / sps).ceil() as usize + taps_len;
    let syms: Vec<Complex64> = (0..count)
        .map(|_| constellation(4, (rng.next_u64() as usize) % 4))
        .collect();

    let mut x = if st.old_placement {
        old_rounded_shaped(n, sps, alpha, &syms)
    } else {
        shaped_linear(n, sps, alpha, |k| syms[k])
    };
    normalise(&mut x);
    // T-564: the gate-SNR reference snippet the shipped generator settles its geometry from.
    let emission = x.clone();

    let bw = (1.35 * rate).clamp(0.001 * fs, 0.9 * fs);
    let sigma_at = |snr_db: f64| (fs / (bw * 10f64.powf(snr_db / 10.0)) / 2.0).sqrt();
    let sigma = sigma_at(cfg.snr_db);
    let mut reference = {
        let mut y = emission;
        let mut rng = Rng::new(cfg.seed ^ (Class::Qpsk as u64) ^ GEOMETRY_REFERENCE_STREAM);
        impair(
            &mut y,
            sigma_at(geometry_reference_snr_db(Class::Qpsk)),
            &cfg,
            fs,
            &mut rng,
        )
    };
    for s in x.iter_mut() {
        let (a, b) = rng.gaussian_pair();
        if st.noise {
            *s += Complex64::new(a * sigma, b * sigma);
        }
    }
    if st.lo && cfg.lo_offset_hz != 0.0 {
        for (i, s) in x.iter_mut().enumerate() {
            let ph = TAU * cfg.lo_offset_hz * i as f64 / fs;
            *s *= Complex64::new(ph.cos(), ph.sin());
        }
    }
    if st.iq && cfg.iq_imbalance != 0.0 {
        for s in x.iter_mut() {
            *s = Complex64::new(s.re * (1.0 + cfg.iq_imbalance), s.im);
        }
    }
    let mut samples: Vec<Complex32> = x
        .iter()
        .map(|s| Complex32::new(s.re as f32, s.im as f32))
        .collect();
    if st.quant && cfg.quantise_8bit {
        let peak = samples
            .iter()
            .map(|s| s.re.abs().max(s.im.abs()))
            .fold(0.0_f32, f32::max)
            .max(1e-9);
        let g = 63.0 / peak;
        for s in samples.iter_mut() {
            *s = Complex32::new(
                ((s.re * g).round().clamp(-127.0, 127.0)) / 127.0,
                ((s.im * g).round().clamp(-127.0, 127.0)) / 127.0,
            );
        }
    }
    let mut origin = 0.0;
    let mut stride = 1.0;
    let peak_hz = old_peak_offset_hz(&samples, fs);
    // The geometry source: the gate-SNR reference snippet after T-564, the delivered one before.
    fn geometry<'a>(
        st: &Stages,
        reference: &'a [Complex32],
        samples: &'a [Complex32],
    ) -> &'a [Complex32] {
        if st.gate_reference_geometry {
            reference
        } else {
            samples
        }
    }
    let centroid_hz = recentre_offset_hz(geometry(&st, &reference, &samples), fs);
    let applied = match st.recentre {
        Recentre::Off => 0.0,
        Recentre::Peak => peak_hz,
        Recentre::Shipped => centroid_hz,
    };
    derotate(&mut samples, applied, fs);
    derotate(&mut reference, centroid_hz, fs);
    let obw_hz = measured_obw(geometry(&st, &reference, &samples), fs).min(bw);
    let cutoff = (0.75 * obw_hz / fs).clamp(0.005, 0.49);
    if cutoff < 0.45 {
        if st.chan {
            let trimmed = samples.len() > 4 * 95;
            samples = channel_filter(&samples, cutoff);
            if trimmed {
                origin += 47.0;
            }
        }
        reference = channel_filter(&reference, cutoff);
    }
    let obw_hz = measured_obw(geometry(&st, &reference, &samples), fs);
    let max_decim = (samples.len() / 2048).max(1);
    let symbol_decim = ((fs / (crate::symbols::SYMBOL_SAMPLES_PER_OBW * obw_hz)).floor() as usize)
        .clamp(1, max_decim);
    if st.decim && symbol_decim > 1 {
        samples = samples.iter().step_by(symbol_decim).copied().collect();
        stride = symbol_decim as f64;
    }
    Built {
        x: samples,
        sps_gen: sps,
        alpha,
        rate,
        syms,
        origin,
        stride,
        peak_hz,
        centroid_hz,
    }
}

/// Matched-filter output at an arbitrary fractional time `t`, by direct correlation with the
/// continuous RRC — no interpolation of the signal, so the receiver adds no distortion of its own.
fn sample_at(x: &[Complex64], t: f64, sps: f64, p: &Pulse) -> Complex64 {
    let lo_f = (t - RRC_SPAN * sps).ceil().max(0.0);
    let hi_f = (t + RRC_SPAN * sps).floor().min((x.len() - 1) as f64);
    if hi_f < lo_f {
        return Complex64::new(0.0, 0.0);
    }
    let (lo, hi) = (lo_f as usize, hi_f as usize);
    let mut acc = Complex64::new(0.0, 0.0);
    for (i, v) in x.iter().enumerate().take(hi + 1).skip(lo) {
        acc += *v * p.at((t - i as f64) / sps);
    }
    acc
}

/// What the oracle recovered.
struct Evm {
    /// EVM with a constant complex gain only — the reading T-291's oracle was limited to, which
    /// **any** residual carrier offset drives straight to 100 % over a long window.
    raw: f64,
    /// EVM after also removing a residual carrier offset, data-aided from the known symbols. This
    /// is the constellation's own quality, with the frequency error taken out.
    derotated: f64,
    /// The residual carrier offset the generator left, Hz.
    cfo_hz: f64,
}

/// EVM of `ys` against the transmitted symbols under the best constant complex gain.
fn ls_evm(ys: &[Complex64], ks: &[usize], syms: &[Complex64]) -> f64 {
    let mut num = Complex64::new(0.0, 0.0);
    let mut den = 0.0;
    for (i, k) in ks.iter().enumerate() {
        num += syms[*k] * ys[i].conj();
        den += ys[i].norm_sqr();
    }
    if den <= 0.0 {
        return f64::NAN;
    }
    let g = num / den;
    let mut err = 0.0;
    let mut pw = 0.0;
    for (i, k) in ks.iter().enumerate() {
        err += (syms[*k] - g * ys[i]).norm_sqr();
        pw += syms[*k].norm_sqr();
    }
    (err / pw).sqrt()
}

/// **Oracle EVM against the transmitted symbols**: the true roll-off, the true symbol period, a
/// uniform symbol grid, a complex gain, and — unlike T-291's oracle — a **residual carrier
/// offset**, estimated data-aided from the known symbols.
///
/// Adding the frequency dimension is what makes the measurement readable. Without it a few hundred
/// hertz of residual CFO rotates the constellation through several cycles across the window and
/// the reading pins at 100 % no matter what the constellation itself looks like, so "the grid got
/// worse" and "the grid got a smaller frequency error" are indistinguishable. The only thing this
/// oracle still cannot do is follow symbol instants that are not uniformly spaced.
fn oracle_evm(b: &Built) -> (Evm, usize) {
    let p = Pulse::new(b.alpha);
    let sps_l = b.sps_gen / b.stride;
    let x: Vec<Complex64> =
        b.x.iter()
            .map(|s| Complex64::new(f64::from(s.re), f64::from(s.im)))
            .collect();
    let at = |k: usize| (k as f64 * b.sps_gen - b.origin) / b.stride;
    let guard = (RRC_SPAN + 1.0) * sps_l;
    let ks: Vec<usize> = (0..b.syms.len())
        .filter(|k| at(*k) - guard >= 0.0 && at(*k) + guard <= (x.len() - 1) as f64)
        .take(300)
        .collect();
    if ks.len() < 32 {
        return (
            Evm {
                raw: f64::NAN,
                derotated: f64::NAN,
                cfo_hz: f64::NAN,
            },
            ks.len(),
        );
    }
    let mut best = Evm {
        raw: f64::NAN,
        derotated: f64::INFINITY,
        cfo_hz: f64::NAN,
    };
    for j in 0..=40 {
        let tau = (-0.5 + j as f64 / 40.0) * sps_l;
        let ys: Vec<Complex64> = ks
            .iter()
            .map(|k| sample_at(&x, at(*k) + tau, sps_l, &p))
            .collect();
        let raw = ls_evm(&ys, &ks, &b.syms);
        // Data-aided CFO: strip the modulation with the known symbols, then take the mean phase
        // step between consecutive symbols.
        let mut acc = Complex64::new(0.0, 0.0);
        for w in 0..ks.len() - 1 {
            let z0 = ys[w] * b.syms[ks[w]].conj();
            let z1 = ys[w + 1] * b.syms[ks[w + 1]].conj();
            acc += z1 * z0.conj();
        }
        let dphi = acc.arg();
        let de: Vec<Complex64> = ys
            .iter()
            .enumerate()
            .map(|(w, y)| {
                let a = -dphi * w as f64;
                *y * Complex64::new(a.cos(), a.sin())
            })
            .collect();
        let derotated = ls_evm(&de, &ks, &b.syms);
        // Phase step per symbol -> Hz at the generation rate.
        let cfo_hz = dphi * SynthConfig::new(30.0, 0).sample_rate_hz / (TAU * b.sps_gen);
        if derotated.is_finite() && derotated < best.derotated {
            best = Evm {
                raw,
                derotated,
                cfo_hz,
            };
        }
    }
    (best, ks.len())
}

/// **The check that makes the ablation mean anything**: with every stage on, the replica is the
/// shipped generator, sample for sample.
#[test]
fn t296_the_replica_is_the_shipped_generator() {
    for seed in [1u64, 2, 3, 7].map(|s| ACCEPTANCE_SEED_BASE + 960_000 + s) {
        let b = build(seed, 30.0, Stages::SHIPPED);
        let s = generate(Class::Qpsk, &SynthConfig::new(30.0, seed));
        assert_eq!(
            b.x.len(),
            s.symbol_samples.len(),
            "seed {seed}: replica length"
        );
        assert_eq!(
            b.x, s.symbol_samples,
            "seed {seed}: replica is not the shipped generator"
        );
    }
}

/// **The identifying measurement.** A root-raised-cosine-shaped random data stream has a spectrum
/// that is *flat* across its passband, so "the strongest bin" is a coin toss among hundreds of
/// statistically identical ones and the old recentring shifted the emission by an arbitrary
/// fraction of its own symbol rate. The centroid, measured on the same PSD, is the quantity that
/// was meant and it is stable.
#[test]
fn t296_the_old_recentring_picked_a_random_bin_of_a_flat_spectrum() {
    eprintln!("\n[T-296] what the recentring step measures, qpsk at 30 dB");
    let mut st = Stages::SHIPPED;
    st.recentre = Recentre::Off;
    let (mut worst_peak, mut worst_centroid) = (0.0_f64, 0.0_f64);
    for seed in (1..=8u64).map(|s| ACCEPTANCE_SEED_BASE + 960_000 + s) {
        let b = build(seed, 30.0, st);
        worst_peak = worst_peak.max((b.peak_hz / b.rate).abs());
        worst_centroid = worst_centroid.max((b.centroid_hz / b.rate).abs());
        eprintln!(
            "[T-296]   seed {seed}: rate {:>6.0} Hz  OLD strongest bin {:>+8.0} Hz ({:+.3} symbol \
             rates)  NEW centroid {:>+7.0} Hz ({:+.3})",
            b.rate,
            b.peak_hz,
            b.peak_hz / b.rate,
            b.centroid_hz,
            b.centroid_hz / b.rate
        );
    }
    eprintln!(
        "[T-296] worst spurious offset: strongest bin {worst_peak:.3} symbol rates, centroid \
         {worst_centroid:.3}"
    );
    // The centroid is the emission's own centre by construction, so it must be a small fraction of
    // the symbol rate on every seed. This is the property the argmax did not have.
    assert!(
        worst_centroid < 0.05,
        "the centroid moved the emission by {worst_centroid:.3} symbol rates"
    );
}

/// **Does the centroid rule behave for every class, or only the flat-spectrum ones it was chosen
/// for?** The argmax was catastrophic on a flat spectrum, but on a *line* emission — a keyed
/// carrier, AM, a 3-level ASK with its DC term — the strongest bin is the carrier and the argmax is
/// exactly right, while a centroid is dragged by whatever the sidebands do. Switching the rule for
/// every class at once could therefore have made those worse, which is where a new wrong-family
/// answer would come from.
///
/// Noiseless, so what is printed is the rule's own behaviour and not a noise draw. `cw` and `ssb`
/// legitimately sit off centre (a keyed tone, a single sideband) — for them a large offset is the
/// correct answer, which is why the harness recentres at all.
#[test]
fn t296_the_centroid_rule_across_every_generated_class() {
    eprintln!("\n[T-296] recentring offset by class, as a fraction of the emission's own OBW99");
    eprintln!("[T-296] (noiseless; cw and ssb are legitimately off-centre by construction)");
    for class in Class::TAXONOMY.iter().chain(Class::HELD_OUT) {
        let (mut peaks, mut cents) = (Vec::new(), Vec::new());
        for seed in 0..6u64 {
            let c = SynthConfig::new(25.0, ACCEPTANCE_SEED_BASE + 950_000 + seed);
            let mut rng = Rng::new(c.seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ *class as u64);
            let n = c.samples;
            let fs = analysis_rate(*class, c.sample_rate_hz);
            let rate = 25e3 + 75e3 * rng.unit();
            let (mut x, _bw) = waveform(*class, &mut rng, n, fs, rate, true);
            normalise(&mut x);
            let samples: Vec<Complex32> = x
                .iter()
                .map(|s| Complex32::new(s.re as f32, s.im as f32))
                .collect();
            let obw = measured_obw(&samples, fs);
            if !(obw.is_finite() && obw > 0.0) {
                continue;
            }
            peaks.push(old_peak_offset_hz(&samples, fs) / obw);
            cents.push(recentre_offset_hz(&samples, fs) / obw);
        }
        let stat = |v: &[f64]| {
            let m = v.iter().sum::<f64>() / v.len().max(1) as f64;
            let sd =
                (v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / v.len().max(1) as f64).sqrt();
            format!("{m:+6.3}+-{sd:5.3}")
        };
        eprintln!(
            "[T-296]   {:<22} OLD argmax {}   NEW centroid {}",
            class.label(),
            stat(&peaks),
            stat(&cents)
        );
    }
}

/// The attribution: EVM at each stage, and with each stage of the shipped pipeline removed.
#[test]
fn t296_which_generator_stage_costs_the_evm() {
    let seeds: Vec<u64> = (1..=6u64)
        .map(|s| ACCEPTANCE_SEED_BASE + 960_000 + s)
        .collect();
    let run = |name: &str, st: Stages| {
        let (mut der, mut raw, mut cfo) = (Vec::new(), Vec::new(), Vec::new());
        for seed in &seeds {
            let b = build(*seed, 30.0, st);
            let (e, _) = oracle_evm(&b);
            if e.derotated.is_finite() {
                der.push(e.derotated);
                raw.push(e.raw);
                cfo.push(e.cfo_hz.abs() / b.rate);
            }
        }
        let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
        let m = mean(&der);
        let sd =
            (der.iter().map(|v| (v - m).powi(2)).sum::<f64>() / der.len().max(1) as f64).sqrt();
        eprintln!(
            "[T-296] {name:<40} EVM {:6.2} +- {:5.2} % ({:7.2} N0)  | no-CFO-search {:6.2} %  \
             residual CFO {:.4} symbol rates",
            100.0 * m,
            100.0 * sd,
            m * m / 1e-3,
            100.0 * mean(&raw),
            mean(&cfo)
        );
        m
    };

    eprintln!("\n[T-296] oracle EVM vs the TRANSMITTED symbols, qpsk, 30 dB, 6 seeds");
    eprintln!("[T-296] -- one stage at a time, from an otherwise ideal waveform --");
    let one = |name: &str, f: fn(&mut Stages)| {
        let mut st = Stages::NONE;
        f(&mut st);
        run(name, st)
    };
    one("ideal (nothing)", |_| {});
    one("OLD rounded symbol placement", |s| s.old_placement = true);
    one("OLD peak recentring", |s| s.recentre = Recentre::Peak);
    one("NEW carrier-aware recentring", |s| {
        s.recentre = Recentre::Shipped
    });
    one("noise only (30 dB)", |s| s.noise = true);
    one("lo offset only", |s| s.lo = true);
    one("iq imbalance only", |s| s.iq = true);
    one("8-bit quantisation only", |s| s.quant = true);
    one("channel filter only", |s| s.chan = true);
    one("decimation only", |s| s.decim = true);

    eprintln!("[T-296] -- the grid as a whole, before and after this commit --");
    let as_was = run("AS WAS (pre-T-296, densities fitted here)", Stages::AS_WAS);
    let shipped = run("SHIPPED (post-T-296)", Stages::SHIPPED);
    eprintln!(
        "[T-296] the two generator fixes are worth {:+.2} pp of EVM",
        100.0 * (shipped - as_was)
    );

    eprintln!("[T-296] -- the shipped pipeline with one stage removed --");
    let less = |name: &str, f: fn(&mut Stages)| {
        let mut st = Stages::SHIPPED;
        f(&mut st);
        let m = run(name, st);
        eprintln!(
            "[T-296]   -> removing it changes EVM by {:+.2} pp",
            100.0 * (m - shipped)
        );
    };
    less("shipped - noise", |s| s.noise = false);
    less("shipped - lo offset", |s| s.lo = false);
    less("shipped - iq imbalance", |s| s.iq = false);
    less("shipped - quantisation", |s| s.quant = false);
    less("shipped - recentring", |s| s.recentre = Recentre::Off);
    less("shipped - channel filter", |s| s.chan = false);
    less("shipped - decimation", |s| s.decim = false);

    eprintln!("[T-296] -- noiseless, so the distortion is not hidden under 3.16 % of AWGN --");
    let mut nl = Stages::SHIPPED;
    nl.noise = false;
    run("shipped, noiseless", nl);
    let mut nlw = Stages::AS_WAS;
    nlw.noise = false;
    run("as-was, noiseless", nlw);
}
