//! T1 tests for spectral estimation (C07): tones, noise level, Parseval, holds, STFT block
//! handling, discontinuities, spectral kurtosis, persistence.

mod common;

use common::*;
use hk_core::{BlockHeader, Discontinuity};
use hk_dsp::synth::{self, Rng};
use hk_dsp::{
    Hold, HoldKind, InputInfo, Persistence, PersistenceConfig, PowerUnit, SpectrumFrame,
    StftConfig, StftProcessor, WelchConfig, Window, WindowKind, sk, welch,
};
use num_complex::Complex32;

const WINDOWS: [WindowKind; 3] = [
    WindowKind::Hann,
    WindowKind::BlackmanHarris,
    WindowKind::FlatTop,
];

fn cfg(n: usize, overlap: usize, window: WindowKind) -> WelchConfig {
    WelchConfig {
        fft_len: n,
        overlap,
        window,
        holds: true,
        spectral_kurtosis: true,
    }
}

#[test]
fn tone_frequency_and_power_per_window() {
    let fs = 1e6;
    let n = 1024;
    let bw = fs / n as f64;
    let power = 0.25; // −6.02 dBFS
    for window in WINDOWS {
        let metrics = Window::new(window, n).metrics();
        for bins_off in [100.0, 100.5, -237.25, 3.0] {
            let off_hz = bins_off * bw;
            let x = synth::tone(0, n * 8, off_hz, fs, power, 0.3);
            let s = welch(&x, fs, 433.92e6, &cfg(n, n / 2, window)).unwrap();
            let per_bin = s.to_db(&s.psd, PowerUnit::DbfsPerBin);
            let peak = (0..n)
                .max_by(|&a, &b| per_bin[a].total_cmp(&per_bin[b]))
                .unwrap();

            // Frequency: the peak bin is the nearest bin (either neighbour at a half-bin).
            assert!(
                (s.bin_offset_hz(peak) - off_hz).abs() <= bw / 2.0 + 1e-6,
                "{window} {bins_off}: peak at {} Hz",
                s.bin_offset_hz(peak)
            );
            assert!((s.bin_frequency_hz(peak) - 433.92e6 - s.bin_offset_hz(peak)).abs() < 1e-3);

            // Integrated power is exact regardless of offset.
            let lo = peak.saturating_sub(12);
            let hi = (peak + 13).min(n);
            let integ = db(s.integrated_power(lo..hi));
            assert!(
                (integ - db(power)).abs() < 0.05,
                "{window} {bins_off}: integrated {integ} dB"
            );

            // Peak per-RBW level: true power at bin centre, minus scalloping at half-bin.
            let frac = (bins_off - f64::floor(bins_off)).min(f64::ceil(bins_off) - bins_off);
            let peak_db = f64::from(per_bin[peak]);
            if frac == 0.0 {
                assert!((peak_db - db(power)).abs() < 0.02, "{window}: {peak_db}");
            } else if frac == 0.5 {
                let want = db(power) - metrics.scalloping_loss_db;
                assert!(
                    (peak_db - want).abs() < 0.02,
                    "{window}: {peak_db} vs {want}"
                );
            } else {
                assert!(peak_db <= db(power) + 0.02);
                assert!(peak_db >= db(power) - metrics.scalloping_loss_db - 0.02);
            }
        }
    }
}

#[test]
fn noise_psd_matches_input_variance() {
    let fs = 2e6;
    let n = 4096;
    let variance = 0.01; // −20 dBFS
    let mut rng = Rng::new(42);
    let x = synth::complex_noise(&mut rng, 63 * n / 2 + n, variance);
    for window in WINDOWS {
        let s = welch(&x, fs, 0.0, &cfg(n, n / 2, window)).unwrap();
        let per_hz = db(mean(&s.psd));
        let want = db(variance / fs);
        assert!(
            (per_hz - want).abs() < 0.2,
            "{window}: {per_hz} vs {want} dBFS/Hz"
        );
        // Per-RBW noise level is σ²·ENBW/N.
        let lin_bin: Vec<f32> = s
            .to_db(&s.psd, PowerUnit::DbfsPerBin)
            .iter()
            .map(|d| 10f32.powf(d / 10.0))
            .collect();
        let want_bin = db(variance * s.resolution.window_metrics.enbw_bins / n as f64);
        assert!((db(mean(&lin_bin)) - want_bin).abs() < 0.2, "{window}");
        assert!(
            (s.resolution.rbw_hz / s.bin_width_hz() - s.resolution.window_metrics.enbw_bins).abs()
                < 1e-9
        );
    }
}

#[test]
fn parseval_holds() {
    let fs = 1e6;
    let n = 2048;
    let mut rng = Rng::new(7);
    let mut x = synth::complex_noise(&mut rng, n, 0.02);
    synth::add_into(&mut x, &synth::tone(0, n, 123_456.0, fs, 0.1, 0.0));
    for window in WINDOWS {
        // One segment: Σ S·Δf = Σ|w·x|² / Σw² exactly.
        let s = welch(&x, fs, 0.0, &cfg(n, 0, window)).unwrap();
        let w = Window::new(window, n);
        let weighted: f64 = x
            .iter()
            .zip(w.coefficients())
            .map(|(v, &c)| f64::from(v.norm_sqr()) * f64::from(c) * f64::from(c))
            .sum::<f64>()
            / w.sum_sq();
        let rel = (s.total_power() - weighted).abs() / weighted;
        assert!(rel < 1e-4, "{window}: rel err {rel}");
    }
    // Averaged: total power ≈ mean sample power.
    let y = synth::complex_noise(&mut rng, 200 * n, 0.05);
    let s = welch(&y, fs, 0.0, &WelchConfig::new(n)).unwrap();
    let mean_power: f64 = y.iter().map(|v| f64::from(v.norm_sqr())).sum::<f64>() / y.len() as f64;
    assert!((db(s.total_power()) - db(mean_power)).abs() < 0.05);
}

#[test]
fn max_and_min_hold() {
    let fs = 1e6;
    let n = 256;
    let bw = fs / n as f64;
    let mut rng = Rng::new(9);
    let mut x = synth::complex_noise(&mut rng, 8 * n, 1e-6);
    // A tone in segment 3 only (overlap 0).
    let t = synth::tone(3 * n as u64, n, 40.0 * bw, fs, 0.01, 0.0);
    synth::add_into(&mut x[3 * n..4 * n], &t);
    let s = welch(&x, fs, 0.0, &cfg(n, 0, WindowKind::Hann)).unwrap();
    let b = s.bin_for_offset_hz(40.0 * bw).unwrap();
    let max_db = s.to_db(&s.max_hold, PowerUnit::DbfsPerBin);
    let avg_db = s.to_db(&s.psd, PowerUnit::DbfsPerBin);
    let min_db = s.to_db(&s.min_hold, PowerUnit::DbfsPerBin);
    assert!(
        (f64::from(max_db[b]) - db(0.01)).abs() < 0.05,
        "max-hold {}",
        max_db[b]
    );
    // One segment in 8: the average is ~9 dB below the max.
    assert!((f64::from(max_db[b] - avg_db[b]) - db(8.0)).abs() < 0.1);
    assert!(min_db[b] < avg_db[b] - 30.0);
    for i in 0..n {
        assert!(s.max_hold[i] >= s.psd[i] && s.psd[i] >= s.min_hold[i]);
    }

    let mut hmax = Hold::new(HoldKind::Max, 3);
    let mut hmin = Hold::new(HoldKind::Min, 3);
    for tr in [[1.0, 5.0, 3.0], [4.0, 2.0, 6.0]] {
        hmax.update(&tr);
        hmin.update(&tr);
    }
    assert_eq!(hmax.values(), &[4.0, 5.0, 6.0]);
    assert_eq!(hmin.values(), &[1.0, 2.0, 3.0]);
    assert_eq!(hmax.updates(), 2);
    hmax.reset();
    assert_eq!(hmax.updates(), 0);
    assert!(hmax.values().iter().all(|v| *v == f32::NEG_INFINITY));
}

fn run_blocks(
    proc: &mut StftProcessor,
    prov: &hk_core::ProvenanceHandle,
    x: &[Complex32],
    sizes: &mut dyn Iterator<Item = usize>,
) -> Vec<SpectrumFrame> {
    let mut frames = Vec::new();
    let mut pos = 0;
    while pos < x.len() {
        let len = sizes.next().unwrap().min(x.len() - pos);
        let h = header(pos as u64, prov, Discontinuity::NONE);
        proc.push(InputInfo::from(&h), &x[pos..pos + len], |f| {
            frames.push(f.clone())
        });
        pos += len;
    }
    frames
}

#[test]
fn stft_across_block_boundaries_matches_monolithic() {
    let fs = 1e6;
    let prov = provenance(100e6, fs);
    let mut rng = Rng::new(11);
    let mut x = synth::complex_noise(&mut rng, 60_000, 1e-3);
    synth::add_into(&mut x, &synth::tone(0, 60_000, 77_000.0, fs, 0.05, 1.0));
    let mut config = StftConfig::new(
        WelchConfig {
            fft_len: 512,
            overlap: 199,
            window: WindowKind::BlackmanHarris,
            holds: true,
            spectral_kurtosis: true,
        },
        5,
    );
    config.persistence = Some(PersistenceConfig::default());

    let mut mono = StftProcessor::new(config).unwrap();
    let a = run_blocks(&mut mono, &prov, &x, &mut std::iter::repeat(x.len()));

    let mut chopped = StftProcessor::new(config).unwrap();
    let mut size_rng = Rng::new(5);
    let mut sizes = std::iter::from_fn(move || Some(1 + (size_rng.next_u64() % 1500) as usize));
    let b = run_blocks(&mut chopped, &prov, &x, &mut sizes);

    let expect = ((x.len() - 512) / 313 + 1) / 5;
    assert_eq!(a.len(), expect);
    assert_eq!(a, b, "chopped stream must be bit-identical");
    let img = |p: &StftProcessor| {
        let pers = p.persistence().unwrap();
        let mut v = vec![0.0; pers.bins() * pers.levels()];
        pers.write_image(&mut v);
        v
    };
    assert_eq!(img(&mono), img(&chopped));

    // Frames are back to back, timestamped at their first sample, and equal the one-shot Welch.
    for (k, f) in a.iter().enumerate() {
        assert_eq!(f.seq, k as u64);
        assert_eq!(f.t.sample_index, (k * 5 * 313) as u64);
        assert_eq!(f.t.host_time.as_unix_nanos(), (k * 5 * 313) as i64 * 1000);
        assert_eq!(f.sample_count, 4 * 313 + 512);
        assert_eq!(f.spectrum.resolution.n_avg, 5);
    }
    let first = welch(&x[..4 * 313 + 512], fs, 100e6, &config.welch).unwrap();
    assert_eq!(first, a[0].spectrum);
}

#[test]
fn discontinuities_reset_averaging() {
    let fs = 1e6;
    let n = 256;
    let k = 8;
    let config = StftConfig::new(cfg(n, 0, WindowKind::Hann), k);
    let mut p = StftProcessor::new(config).unwrap();
    let mut rng = Rng::new(3);
    let mut frames: Vec<SpectrumFrame> = Vec::new();
    let mut push = |p: &mut StftProcessor, h: &BlockHeader, x: &[Complex32]| {
        p.push(InputInfo::from(h), x, |f| frames.push(f.clone()));
    };

    // A: loud, 2 frames + 3 segments + 100 samples.
    let prov_a = provenance(100e6, fs);
    let len_a = 2 * k * n + 3 * n + 100;
    let a = synth::complex_noise(&mut rng, len_a, 0.1);
    push(&mut p, &header(0, &prov_a, Discontinuity::STREAM_START), &a);

    // B: retuned, quiet, one frame.
    let prov_b = provenance(101e6, fs);
    let b = synth::complex_noise(&mut rng, k * n, 1e-4);
    let start_b = len_a as u64;
    push(&mut p, &header(start_b, &prov_b, Discontinuity::RETUNE), &b);

    // C: a 1000-sample gap with no flag.
    let start_c = start_b + (k * n) as u64 + 1000;
    let c = synth::complex_noise(&mut rng, k * n, 1e-4);
    push(&mut p, &header(start_c, &prov_b, Discontinuity::NONE), &c);

    // D: flagged gap with dropped_before, after a partial segment.
    let start_d = start_c + (k * n) as u64 + 50;
    let mut hd = header(start_d, &prov_b, Discontinuity::GAP);
    hd.dropped_before = 50;
    let d = synth::complex_noise(&mut rng, k * n, 1e-4);
    push(&mut p, &hd, &d);

    // E: a gain change with no flag (e.g. a ring chunk that starts mid-block).
    let prov_e = provenance_with(101e6, fs, 32.0);
    let start_e = start_d + (k * n) as u64;
    let e = synth::complex_noise(&mut rng, k * n, 4e-4);
    push(&mut p, &header(start_e, &prov_e, Discontinuity::NONE), &e);

    assert_eq!(frames.len(), 6);
    assert!(
        frames[0]
            .discontinuity
            .contains(Discontinuity::STREAM_START)
    );
    assert!(frames[1].discontinuity.is_empty());

    let fb = &frames[2];
    assert!(fb.discontinuity.contains(Discontinuity::RETUNE));
    assert_eq!(fb.t.sample_index, start_b);
    assert_eq!(fb.spectrum.f_center_hz, 101e6);
    assert_eq!(fb.provenance, prov_b);
    let quiet = db(1e-4 / fs);
    assert!(
        (db(mean(&fb.spectrum.psd)) - quiet).abs() < 0.5,
        "loud data leaked into the frame"
    );

    let fc = &frames[3];
    assert!(fc.discontinuity.contains(Discontinuity::GAP));
    assert_eq!(fc.dropped_samples, 1000);
    assert_eq!(fc.t.sample_index, start_c);

    let fd = &frames[4];
    assert!(fd.discontinuity.contains(Discontinuity::GAP));
    assert_eq!(fd.dropped_samples, 50);
    assert_eq!(fd.t.sample_index, start_d);

    let fe = &frames[5];
    assert!(fe.discontinuity.contains(Discontinuity::GAIN_CHANGE));
    assert_eq!(fe.t.sample_index, start_e);
    assert!((db(mean(&fe.spectrum.psd)) - db(4e-4 / fs)).abs() < 0.5);

    let st = p.stats();
    assert_eq!(st.frames, 6);
    assert_eq!(st.resets, 5);
    assert_eq!(st.segments_discarded, 3);
    assert_eq!(st.samples_dropped, 1050);
}

fn run_inputs(
    config: StftConfig,
    inputs: &[(BlockHeader, Vec<Complex32>)],
) -> (Vec<SpectrumFrame>, hk_dsp::StftStats) {
    let mut p = StftProcessor::new(config).unwrap();
    let mut frames = Vec::new();
    for (h, x) in inputs {
        p.push(InputInfo::from(h), x, |f| frames.push(f.clone()));
    }
    p.flush(|f| frames.push(f.clone()));
    (frames, p.stats())
}

fn history_like(n: usize, k: usize) -> StftConfig {
    let mut w = cfg(n, 0, WindowKind::Hann);
    w.holds = false;
    w.spectral_kurtosis = false;
    StftConfig::new(w, k)
}

/// T-139: once a retune arms the stream, each reset emits the averaging in progress (at least
/// `min_segments`) with its true `n_avg`, span, time, tuning and flags; shorter pieces are still
/// discarded.
#[test]
fn partial_frames_emit_short_steps_once_armed() {
    let (fs, n, k) = (1e6, 256, 8);
    let mut config = history_like(n, k);
    config.partial = Some(hk_dsp::PartialFrames {
        min_segments: 2,
        arm_on: Discontinuity::RETUNE | Discontinuity::RATE_CHANGE,
    });
    let mut rng = Rng::new(9);
    let (prov_a, prov_b, prov_c) = (
        provenance(100e6, fs),
        provenance(101e6, fs),
        provenance(102e6, fs),
    );
    let mut inputs = Vec::new();
    // A: one full frame + 3 segments, loud.
    let len_a = k * n + 3 * n;
    inputs.push((
        header(0, &prov_a, Discontinuity::STREAM_START),
        synth::complex_noise(&mut rng, len_a, 0.1),
    ));
    // B: retuned, 5 segments + 10 samples, quiet.
    let start_b = len_a as u64;
    inputs.push((
        header(start_b, &prov_b, Discontinuity::RETUNE),
        synth::complex_noise(&mut rng, 5 * n + 10, 1e-4),
    ));
    // C: after a 100-sample gap, 2 segments.
    let start_c = start_b + (5 * n + 10) as u64 + 100;
    let mut hc = header(start_c, &prov_b, Discontinuity::GAP);
    hc.dropped_before = 100;
    inputs.push((hc, synth::complex_noise(&mut rng, 2 * n, 1e-4)));
    // D: after another gap, 1 segment (below the minimum).
    let start_d = start_c + (2 * n) as u64 + 100;
    let mut hd = header(start_d, &prov_b, Discontinuity::GAP);
    hd.dropped_before = 100;
    inputs.push((hd, synth::complex_noise(&mut rng, n, 1e-4)));
    // E: retuned, one full frame.
    let start_e = start_d + n as u64;
    inputs.push((
        header(start_e, &prov_c, Discontinuity::RETUNE),
        synth::complex_noise(&mut rng, k * n, 1e-4),
    ));

    let (frames, st) = run_inputs(config, &inputs);
    let summary: Vec<(u64, u32, u64)> = frames
        .iter()
        .map(|f| {
            (
                f.t.sample_index,
                f.spectrum.resolution.n_avg,
                f.sample_count,
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            (0, 8, (k * n) as u64),
            ((k * n) as u64, 3, (3 * n) as u64),
            (start_b, 5, (5 * n) as u64),
            (start_c, 2, (2 * n) as u64),
            (start_e, 8, (k * n) as u64),
        ]
    );
    assert_eq!((st.frames, st.partial_frames), (5, 3));
    assert_eq!(
        st.segments_discarded, 1,
        "D's lone segment is under the minimum"
    );
    let pa = &frames[1];
    assert_eq!(
        pa.provenance, prov_a,
        "the partial keeps the tuning it measured"
    );
    assert_eq!(pa.spectrum.f_center_hz, 100e6);
    assert!(pa.discontinuity.is_empty());
    assert!((db(mean(&pa.spectrum.psd)) - db(0.1 / fs)).abs() < 0.5);
    assert_eq!(pa.t.host_time.as_unix_nanos(), (k * n) as i64 * 1000);
    let pb = &frames[2];
    assert!(pb.discontinuity.contains(Discontinuity::RETUNE));
    assert!(!pb.discontinuity.contains(Discontinuity::GAP));
    assert!((db(mean(&pb.spectrum.psd)) - db(1e-4 / fs)).abs() < 0.5);
    assert!(frames[3].discontinuity.contains(Discontinuity::GAP));
    assert_eq!(frames[3].dropped_samples, 100);
    let seqs: Vec<u64> = frames.iter().map(|f| f.seq).collect();
    assert_eq!(seqs, vec![0, 1, 2, 3, 4]);
}

/// T-139 no-regression: a stream that never retunes (stream start, gaps, a gain step) emits
/// bit-identical frames with partial frames enabled, so fixed-tune history tiles are unchanged.
#[test]
fn partial_frames_never_armed_are_bit_identical() {
    let (fs, n, k) = (1e6, 256, 8);
    let mut rng = Rng::new(21);
    let prov = provenance(100e6, fs);
    let prov_gain = provenance_with(100e6, fs, 32.0);
    let mut inputs = Vec::new();
    let len_a = 2 * k * n + 3 * n + 100;
    inputs.push((
        header(0, &prov, Discontinuity::STREAM_START),
        synth::complex_noise(&mut rng, len_a, 0.1),
    ));
    let start_b = len_a as u64 + 1000;
    inputs.push((
        header(start_b, &prov, Discontinuity::NONE),
        synth::complex_noise(&mut rng, k * n + 5 * n, 1e-3),
    ));
    let start_c = start_b + (k * n + 5 * n) as u64 + 50;
    let mut hc = header(start_c, &prov, Discontinuity::GAP);
    hc.dropped_before = 50;
    inputs.push((hc, synth::complex_noise(&mut rng, k * n + 4 * n, 1e-3)));
    let start_d = start_c + (k * n + 4 * n) as u64;
    inputs.push((
        header(start_d, &prov_gain, Discontinuity::NONE),
        synth::complex_noise(&mut rng, 2 * k * n, 4e-3),
    ));

    let mut on = history_like(n, k);
    on.partial = Some(hk_dsp::PartialFrames {
        min_segments: 1,
        arm_on: Discontinuity::RETUNE | Discontinuity::RATE_CHANGE,
    });
    let (a, sa) = run_inputs(history_like(n, k), &inputs);
    let (b, sb) = run_inputs(on, &inputs);
    assert!(sa.resets >= 3, "the stream resets: {sa:?}");
    assert_eq!(a, b, "never-armed frames must be bit-identical");
    assert_eq!(sb.partial_frames, 0);
    assert_eq!(sa, sb);
}

#[test]
fn spectral_kurtosis_noise_bursts_cw() {
    let fs = 1e6;
    let n = 256;
    let m = 200;
    let bw = fs / n as f64;
    let mut rng = Rng::new(21);
    let sigma = sk::std_dev(m as u32);

    // Gaussian noise: SK ≈ 1, spread ≈ the predicted σ. Overlap 0 and 50% Hann.
    for overlap in [0, n / 2] {
        let len = (m - 1) * (n - overlap) + n;
        let x = synth::complex_noise(&mut rng, len, 1.0);
        let s = welch(&x, fs, 0.0, &cfg(n, overlap, WindowKind::Hann)).unwrap();
        assert_eq!(s.resolution.n_avg, m as u32);
        let mu = mean(&s.sk);
        let var =
            s.sk.iter()
                .map(|&v| (f64::from(v) - mu).powi(2))
                .sum::<f64>()
                / n as f64;
        assert!((mu - 1.0).abs() < 0.03, "overlap {overlap}: mean SK {mu}");
        let ratio = var / sk::variance(m as u32);
        assert!(
            (0.7..1.4).contains(&ratio),
            "overlap {overlap}: var ratio {ratio}"
        );
        assert!((s.sk_std_dev() - sigma).abs() < 1e-12);
    }

    // On/off bursts (1 segment in 5) and CW, both well above the noise.
    let len = m * n;
    let mut x = synth::complex_noise(&mut rng, len, 1e-3);
    let burst_bin = 30.0;
    let cw_bin = -50.0;
    let tone_b = synth::tone(0, len, burst_bin * bw, fs, 0.01, 0.0);
    for seg in (0..m).step_by(5) {
        synth::add_into(
            &mut x[seg * n..(seg + 1) * n],
            &tone_b[seg * n..(seg + 1) * n],
        );
    }
    synth::add_into(&mut x, &synth::tone(0, len, cw_bin * bw, fs, 0.01, 0.0));
    let s = welch(&x, fs, 0.0, &cfg(n, 0, WindowKind::Hann)).unwrap();
    let sk_burst = f64::from(s.sk[s.bin_for_offset_hz(burst_bin * bw).unwrap()]);
    let sk_cw = f64::from(s.sk[s.bin_for_offset_hz(cw_bin * bw).unwrap()]);
    assert!(sk_burst > 1.0 + 10.0 * sigma, "burst SK {sk_burst}");
    assert!(sk_cw < 1.0 - 5.0 * sigma, "CW SK {sk_cw}");
    // Constant-envelope bursts: SK = (M+1)/(M-1)·(1/δ − 1) with δ = 0.2.
    let want = (m as f64 + 1.0) / (m as f64 - 1.0) * 4.0;
    assert!(
        (sk_burst - want).abs() < 0.05 * want,
        "{sk_burst} vs {want}"
    );
}

#[test]
fn persistence_decays_exponentially() {
    let cfg = PersistenceConfig {
        levels: 10,
        min_db: -100.0,
        max_db: 0.0,
        tau_s: 1.0,
    };
    let dt = 0.01;
    let mut p = Persistence::new(2, cfg, dt);
    let beta = (-dt / cfg.tau_s).exp();
    assert!((f64::from(p.beta()) - beta).abs() < 1e-7);

    // Hit: bin 0 at −45 dB (level 5 spans −50..−40); bin 1 at +10 dB clamps to level 9.
    p.update(&[10f32.powf(-4.5), 10.0], 0.0);
    assert_eq!(p.value(1, 9), 1.0);
    // 1000 more updates with bin 0 at −85 dB (level 1); crosses a renormalisation.
    let frames = 1000;
    for _ in 0..frames {
        p.update(&[10f32.powf(-8.5), 0.0], 0.0);
    }
    let decayed = f64::from(p.value(0, 5));
    let want = beta.powi(frames);
    assert!((decayed - want).abs() / want < 2e-3, "{decayed} vs {want}");
    let steady = f64::from(p.value(0, 1));
    let want_steady = (1.0 - beta.powi(frames)) / (1.0 - beta);
    assert!(
        (steady - want_steady).abs() / want_steady < 2e-3,
        "{steady} vs {want_steady}"
    );
    // Zero power clamps to level 0.
    assert!(p.value(1, 0) > 90.0);
    assert!((p.level_db(5) + 50.0).abs() < 1e-4);
    p.clear();
    assert_eq!(p.value(0, 1), 0.0);
}

#[test]
fn config_validation() {
    assert!(
        StftConfig::new(WelchConfig::new(256), 1)
            .validate()
            .is_err()
    );
    let mut c = StftConfig::new(WelchConfig::new(256), 1);
    c.welch.spectral_kurtosis = false;
    assert!(c.validate().is_ok());
    c.welch.overlap = 256;
    assert!(c.validate().is_err());
    let d = StftConfig::for_bin_width(20e6, 1000.0, 4);
    assert_eq!(d.welch.fft_len, 32768);
}

/// T-139 review: a full frame on unchanged tuning disarms partial frames, so a tune the scheduler
/// left and then held emits no partial row at a later gap (e.g. a USB overrun); the next retune
/// arms them again.
#[test]
fn partial_frames_disarm_after_a_full_frame_and_rearm_on_retune() {
    let (fs, n, k) = (1e6, 256, 8);
    let mut config = history_like(n, k);
    config.partial = Some(hk_dsp::PartialFrames {
        min_segments: 2,
        arm_on: Discontinuity::RETUNE | Discontinuity::RATE_CHANGE,
    });
    let mut rng = Rng::new(33);
    let (prov_a, prov_b) = (provenance(100e6, fs), provenance(101e6, fs));
    let mut inputs = Vec::new();
    // A: 3 segments at the stream start.
    inputs.push((
        header(0, &prov_a, Discontinuity::STREAM_START),
        synth::complex_noise(&mut rng, 3 * n, 1e-3),
    ));
    // B: retuned (armed, A's 3 segments emitted), then held for three full frames + 4 segments.
    let start_b = (3 * n) as u64;
    let len_b = 3 * k * n + 4 * n;
    inputs.push((
        header(start_b, &prov_b, Discontinuity::RETUNE),
        synth::complex_noise(&mut rng, len_b, 1e-3),
    ));
    // C: an overrun gap on the held tune: B's 4 segments are discarded, not emitted.
    let start_c = start_b + len_b as u64 + 500;
    let mut hc = header(start_c, &prov_b, Discontinuity::GAP);
    hc.dropped_before = 500;
    inputs.push((hc, synth::complex_noise(&mut rng, 5 * n, 1e-3)));
    // D: retuned back: armed again, C's 5 segments emitted.
    let start_d = start_c + (5 * n) as u64;
    inputs.push((
        header(start_d, &prov_a, Discontinuity::RETUNE),
        synth::complex_noise(&mut rng, k * n, 1e-3),
    ));

    let (frames, st) = run_inputs(config, &inputs);
    let summary: Vec<(u64, u32)> = frames
        .iter()
        .map(|f| (f.t.sample_index, f.spectrum.resolution.n_avg))
        .collect();
    let kn = (k * n) as u64;
    assert_eq!(
        summary,
        vec![
            (0, 3),
            (start_b, 8),
            (start_b + kn, 8),
            (start_b + 2 * kn, 8),
            (start_c, 5),
            (start_d, 8),
        ],
        "no partial row at the held tune's gap"
    );
    assert_eq!(st.partial_frames, 2);
    assert_eq!(st.segments_discarded, 4, "B's tail at the gap");
}
