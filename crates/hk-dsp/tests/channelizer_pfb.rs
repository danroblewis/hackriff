//! C11 polyphase filter bank (T-008): measured prototype response, channel isolation and
//! edge-straddle flatness on synthetic tones, phase/time-map correctness, block boundaries and
//! discontinuities. The narrowband channel streams it produces feed the SIGNAL-062, AWARE-036
//! and SIGNAL-001 chains (see `signal_062_*`, `aware_036_*`, `signal_001_*`).

mod common;

use std::f64::consts::PI;

use common::*;
use hk_core::{Discontinuity, ProvenanceHandle};
use hk_dsp::synth::{self, Rng};
use hk_dsp::{ChannelTime, InputInfo, IqSample, Pfb, PfbBackend, PfbConfig, pfb_prototype};
use num_complex::Complex32;

/// Every channel's samples from one run, plus the first non-empty block's time map.
struct Run {
    channels: Vec<Vec<Complex32>>,
    first: ChannelTime,
    blocks: Vec<(Discontinuity, u64, usize, ChannelTime)>,
}

fn run_pfb<T: IqSample>(
    pfb: &mut Pfb,
    x: &[T],
    start: u64,
    prov: &ProvenanceHandle,
    chunks: &[usize],
) -> Run {
    let m = pfb.config().channels;
    let mut channels = vec![Vec::new(); m];
    let mut first: Option<ChannelTime> = None;
    let mut blocks = Vec::new();
    let mut pos = 0;
    let mut i = 0;
    while pos < x.len() {
        let n = chunks[i % chunks.len()].min(x.len() - pos);
        let flags = if i == 0 {
            Discontinuity::STREAM_START
        } else {
            Discontinuity::NONE
        };
        let h = header(start + pos as u64, prov, flags);
        let out = pfb.process(InputInfo::from(&h), &x[pos..pos + n]);
        let t = out.header.time;
        if out.frames > 0 {
            if let Some(f) = first {
                let done = channels[out.active_channels()[0]].len();
                assert_eq!(
                    t.source_index,
                    f.source_index_of(done),
                    "time map continuity"
                );
                assert_eq!(t.out_index, f.out_index + done as u64);
            } else {
                first = Some(t);
            }
            for &c in out.active_channels() {
                let ch = out.channel(c).unwrap();
                assert_eq!(ch.len(), out.frames);
                channels[c].extend(ch.iter());
            }
        }
        blocks.push((
            out.header.discontinuity,
            out.header.dropped_before,
            out.frames,
            t,
        ));
        pos += n;
        i += 1;
    }
    Run {
        channels,
        first: first.expect("PFB produced output"),
        blocks,
    }
}

fn mean_power(y: &[Complex32]) -> f64 {
    y.iter().map(|s| f64::from(s.norm_sqr())).sum::<f64>() / y.len() as f64
}

/// Circular frequency distance on the `fs` wrap.
fn wrap_dist(a: f64, b: f64, fs: f64) -> f64 {
    let d = (a - b).rem_euclid(fs);
    d.min(fs - d)
}

fn tone(offset_hz: f64, fs: f64, power: f64, phase: f64, len: usize) -> Vec<Complex32> {
    synth::tone(0, len, offset_hz, fs, power, phase)
}

#[test]
fn pfb_prototype_measured_response_meets_design() {
    for (m, a) in [(16usize, 60.0), (64, 60.0), (64, 80.0), (512, 60.0)] {
        let d = pfb_prototype(m, a).unwrap();
        let delta = 10f64.powf(-a / 20.0);
        let ripple_bound = 20.0 * ((1.0 + delta) / (1.0 - delta)).log10();
        assert!(d.response.stopband_db >= a, "M={m} A={a}: {:?}", d.response);
        assert!(
            d.response.passband_ripple_db <= ripple_bound * 1.5,
            "M={m} A={a}: ripple {} > {ripple_bound}",
            d.response.passband_ripple_db
        );
        let per_channel = d.len() as f64 / m as f64;
        let kaiser = 2.0 * (a - 7.95) / 14.357;
        assert!(
            per_channel < kaiser * 1.1 + 1.0,
            "M={m}: {per_channel} taps/channel"
        );
        eprintln!(
            "PFB prototype M={m} A={a}: {} taps ({per_channel:.2}/channel), beta {:.3}, \
             ripple {:.4} dB p-p, stopband {:.2} dB",
            d.len(),
            d.beta,
            d.response.passband_ripple_db,
            d.response.stopband_db
        );
        if m > 64 {
            continue; // the dense DTFT below is slow in debug builds
        }
        // Independent check: direct DTFT, not the designer's FFT measurement.
        let dc: f64 = d.taps.iter().map(|&t| f64::from(t)).sum();
        let gain = |f: f64| {
            let (mut re, mut im) = (0.0, 0.0);
            for (n, &t) in d.taps.iter().enumerate() {
                let ph = -2.0 * PI * f * n as f64;
                re += f64::from(t) * ph.cos();
                im += f64::from(t) * ph.sin();
            }
            20.0 * ((re * re + im * im).sqrt() / dc).log10()
        };
        let step = 1.0 / (8.0 * d.len() as f64);
        let (mut pmin, mut pmax, mut smax) = (0.0f64, 0.0f64, f64::NEG_INFINITY);
        let mut f = 0.0;
        while f <= 0.5 / m as f64 {
            let g = gain(f);
            pmin = pmin.min(g);
            pmax = pmax.max(g);
            f += step;
        }
        let mut f = 1.0 / m as f64;
        while f <= 0.5 {
            smax = smax.max(gain(f));
            f += step;
        }
        assert!(
            pmax - pmin <= ripple_bound * 1.5,
            "M={m}: DTFT ripple {}",
            pmax - pmin
        );
        assert!(-smax >= a - 0.2, "M={m}: DTFT stopband {}", -smax);
    }
}

#[test]
fn pfb_adjacent_channel_isolation_meets_stopband() {
    let m = 16;
    let fs = 1.6e6;
    let spacing = fs / m as f64;
    let prov = provenance(433e6, fs);
    let mut pfb = Pfb::new(PfbConfig::new(m)).unwrap();
    let len = pfb.taps() + 64 * m / 2;
    let mut worst_adjacent = f64::NEG_INFINITY;
    let mut worst_any = f64::NEG_INFINITY;
    for c in [0usize, 3, 8, 15] {
        for frac in [0.0, 0.3, -0.5] {
            let f = pfb.config().channel_offset_hz(c, fs) + frac * spacing;
            let x = tone(f, fs, 0.25, 0.7, len);
            pfb.reset();
            let run = run_pfb(&mut pfb, &x, 0, &prov, &[len]);
            for j in 0..m {
                let fj = pfb.config().channel_offset_hz(j, fs);
                if wrap_dist(f, fj, fs) < spacing * (1.0 - 1e-9) {
                    continue;
                }
                let rel = 10.0 * (mean_power(&run.channels[j]) / 0.25).log10();
                assert!(
                    rel <= -60.0,
                    "tone in channel {c} (+{frac}Δ) leaks {rel:.2} dB into channel {j}"
                );
                worst_any = worst_any.max(rel);
                let adjacent = (j + 1) % m == c || (c + 1) % m == j;
                if frac == 0.0 && adjacent {
                    worst_adjacent = worst_adjacent.max(rel);
                }
            }
        }
    }
    eprintln!(
        "PFB M=16 isolation: worst adjacent-channel {worst_adjacent:.2} dB, worst stopband \
         (≥ Δ away) {worst_any:.2} dB"
    );
}

#[test]
fn pfb_multitone_channel_powers_match_tones() {
    let m = 64;
    let fs = 6.4e6;
    let spacing = fs / m as f64;
    let prov = provenance(100e6, fs);
    let mut pfb = Pfb::new(PfbConfig::new(m)).unwrap();
    let cfg = pfb.config().clone();
    let len = pfb.taps() + 80 * m / 2;
    // Four channel-centre tones and one on the 40|41 boundary.
    let centre = [(5usize, 0.1), (20, 0.03), (33, 0.2), (50, 0.01)];
    let boundary = (cfg.channel_offset_hz(40, fs) + spacing / 2.0, 0.05);
    let mut x = vec![Complex32::default(); len];
    let mut all = Vec::new();
    for (i, &(c, p)) in centre.iter().enumerate() {
        let f = cfg.channel_offset_hz(c, fs);
        synth::add_into(&mut x, &tone(f, fs, p, 0.4 * i as f64, len));
        all.push((f, p));
    }
    synth::add_into(&mut x, &tone(boundary.0, fs, boundary.1, 1.1, len));
    all.push(boundary);
    let total: f64 = all.iter().map(|t| t.1).sum();

    let run = run_pfb(&mut pfb, &x, 0, &prov, &[4096]);
    let mut worst_empty = f64::NEG_INFINITY;
    for (i, &(c, p)) in centre.iter().enumerate() {
        let y = &run.channels[c];
        let err_db = 10.0 * (mean_power(y) / p).log10();
        assert!(
            err_db.abs() < 0.005,
            "channel {c}: {err_db:.4} dB off its tone"
        );
        // A channel-centre tone comes out at DC with its phase (phase referenced to index 0).
        let want = Complex32::from_polar(p.sqrt() as f32, 0.4 * i as f32);
        let worst = y.iter().map(|s| (s - want).norm()).fold(0.0f32, f32::max);
        assert!(
            f64::from(worst) / p.sqrt() < 0.02,
            "channel {c}: sample deviates {worst} from {want}"
        );
    }
    for c in [40usize, 41] {
        let err_db = 10.0 * (mean_power(&run.channels[c]) / boundary.1).log10();
        assert!(
            err_db.abs() < 0.02,
            "boundary tone in channel {c}: {err_db:.4} dB (should be flat)"
        );
    }
    for j in 0..m {
        if centre.iter().any(|t| t.0 == j) || j == 40 || j == 41 {
            continue;
        }
        let fj = cfg.channel_offset_hz(j, fs);
        assert!(
            all.iter()
                .all(|t| wrap_dist(t.0, fj, fs) >= spacing * 0.999)
        );
        let rel = 10.0 * (mean_power(&run.channels[j]) / total).log10();
        assert!(
            rel <= -60.0,
            "empty channel {j} holds {rel:.2} dB of total tone power"
        );
        worst_empty = worst_empty.max(rel);
    }
    eprintln!("PFB M=64 multitone: worst empty channel {worst_empty:.2} dB re total tone power");
}

#[test]
fn pfb_edge_response_is_flat_and_phase_follows_time_map() {
    let m = 16;
    let fs = 1.6e6;
    let spacing = fs / m as f64;
    let prov = provenance(433e6, fs);
    let mut pfb = Pfb::new(PfbConfig::new(m)).unwrap();
    let len = pfb.taps() + 40 * m / 2;
    let c = 6;
    let fc = pfb.config().channel_offset_hz(c, fs);
    for step in -10..=10 {
        let delta = step as f64 / 20.0 * spacing; // ±Δ/2
        let x = tone(fc + delta, fs, 0.25, -0.3, len);
        pfb.reset();
        let run = run_pfb(&mut pfb, &x, 0, &prov, &[997, 3000]);
        let p = 10.0 * (mean_power(&run.channels[c]) / 0.25).log10();
        assert!(p.abs() < 0.02, "Δ·{:.2}: {p:.4} dB", step as f64 / 20.0);
        if step == 10 {
            let q = 10.0 * (mean_power(&run.channels[c + 1]) / 0.25).log10();
            assert!(
                q.abs() < 0.02,
                "boundary tone in upper neighbour: {q:.4} dB"
            );
        }
        // Analytic output: A·e^{j(φ0 + 2π·δ·τ/fs)} with τ the time map's source index.
        let worst = run.channels[c]
            .iter()
            .enumerate()
            .map(|(k, s)| {
                let tau = run.first.source_index_of(k);
                let want = Complex32::from_polar(0.5, (-0.3 + 2.0 * PI * delta * tau / fs) as f32);
                f64::from((s - want).norm()) / 0.5
            })
            .fold(0.0, f64::max);
        assert!(
            worst < 0.01,
            "Δ·{}: phase/time-map error {worst}",
            step as f64 / 20.0
        );
    }
}

#[test]
fn pfb_large_bank_and_active_subset() {
    let m = 512;
    let fs = 20e6;
    let prov = provenance(900e6, fs);
    let cfg = PfbConfig {
        active: Some(vec![300, 299, 301, 100]),
        ..PfbConfig::new(m)
    };
    let mut pfb = Pfb::new(cfg.clone()).unwrap();
    let len = pfb.taps() + 24 * m / 2;
    let x = tone(cfg.channel_offset_hz(300, fs), fs, 0.5, 0.0, len);
    let run = run_pfb(&mut pfb, &x, 0, &prov, &[65_536]);
    assert!(
        run.channels[5].is_empty(),
        "inactive channels are not materialised"
    );
    let p = 10.0 * (mean_power(&run.channels[300]) / 0.5).log10();
    assert!(p.abs() < 0.005, "{p}");
    for j in [299, 301, 100] {
        let rel = 10.0 * (mean_power(&run.channels[j]) / 0.5).log10();
        assert!(rel <= -60.0, "channel {j}: {rel:.2} dB");
    }
    assert_eq!(run.first.source_per_output, (m / 2) as f64);
    assert_eq!(
        run.first.source_index,
        (pfb.taps() - 1) as f64 / 2.0,
        "first output represents the centre of the first full window"
    );

    // The subset equals the same channels of a full bank.
    let mut full = Pfb::new(PfbConfig::new(m)).unwrap();
    let all = run_pfb(&mut full, &x, 0, &prov, &[65_536]);
    for j in [300, 299, 301, 100] {
        assert_eq!(run.channels[j], all.channels[j], "channel {j}");
    }
    // Frame-major access agrees with per-channel access.
    let h = header(0, &prov, Discontinuity::STREAM_START);
    let mut again = Pfb::new(cfg).unwrap();
    let out = again.process(InputInfo::from(&h), &x);
    assert!(out.frames > 2);
    assert_eq!(out.frame(2)[0], out.channel(300).unwrap().get(2).unwrap());
    assert_eq!(out.frame(2)[3], out.slot(3).get(2).unwrap());
    assert_eq!(out.samples().len(), out.frames * 4);
}

#[test]
fn pfb_chunking_and_sample_type_do_not_change_output() {
    let m = 32;
    let fs = 3.2e6;
    let prov = provenance(144e6, fs);
    let mut rng = Rng::new(0x7008);
    let mut x = synth::complex_noise(&mut rng, 40_000, 2e-3);
    synth::add_into(&mut x, &tone(123_456.0, fs, 0.05, 0.0, 40_000));
    let (xi8, _) = synth::quantize_ci8(&x);
    let xc: Vec<Complex32> = xi8.iter().map(|s| s.to_complex32()).collect();

    let mut a = Pfb::new(PfbConfig::new(m)).unwrap();
    let mono = run_pfb(&mut a, &xc, 500, &prov, &[xc.len()]);
    let mut b = Pfb::new(PfbConfig::new(m)).unwrap();
    let chunked = run_pfb(
        &mut b,
        &xi8,
        500,
        &prov,
        &[1, 7, 100, 3, 2048, 511, 16, 5000],
    );
    let mut c = Pfb::new(PfbConfig::new(m)).unwrap();
    let chunked_c32 = run_pfb(&mut c, &xc, 500, &prov, &[333, 4096, 2]);
    assert!(!mono.channels[0].is_empty());
    for j in 0..m {
        assert_eq!(
            mono.channels[j], chunked.channels[j],
            "channel {j}: ci8 chunked"
        );
        assert_eq!(
            mono.channels[j], chunked_c32.channels[j],
            "channel {j}: c32 chunked"
        );
    }
    assert_eq!(mono.first, chunked.first);
}

#[test]
fn pfb_resets_on_gap_and_retune_and_propagates_flags() {
    let m = 16;
    let fs = 1.6e6;
    let prov = provenance(433e6, fs);
    let mut rng = Rng::new(42);
    let x = synth::complex_noise(&mut rng, 30_000, 1e-2);
    let mut pfb = Pfb::new(PfbConfig::new(m)).unwrap();
    let l = pfb.taps();
    let d = m / 2;

    // Segment 1 at index 0, then a 5000-sample gap, fed in chunks smaller than the window.
    let seg1 = &x[..10_000];
    let seg2 = &x[10_000..];
    let r1 = run_pfb(&mut pfb, seg1, 0, &prov, &[10_000]);
    assert!(r1.blocks[0].0.contains(Discontinuity::STREAM_START));
    let gap_start = 15_000u64;
    let mut blocks = Vec::new();
    let mut after = vec![Vec::new(); m];
    for (i, chunk) in seg2.chunks(10).enumerate() {
        let h = header(gap_start + (i * 10) as u64, &prov, Discontinuity::NONE);
        let out = pfb.process(InputInfo::from(&h), chunk);
        blocks.push((
            out.header.discontinuity,
            out.header.dropped_before,
            out.frames,
        ));
        for (c, dst) in after.iter_mut().enumerate() {
            dst.extend(out.channel(c).unwrap().iter());
        }
        if i == 0 {
            assert_eq!(
                out.frames, 0,
                "history was cleared: no output until a full window"
            );
        }
    }
    let first_nonempty = blocks.iter().position(|b| b.2 > 0).unwrap();
    assert_eq!(first_nonempty, l.div_ceil(10) - 1);
    for b in &blocks[..first_nonempty] {
        assert_eq!(b.0, Discontinuity::NONE, "flags wait for a non-empty block");
    }
    assert!(blocks[first_nonempty].0.contains(Discontinuity::GAP));
    assert_eq!(blocks[first_nonempty].1, 5_000);
    assert!(
        blocks[first_nonempty + 1..]
            .iter()
            .all(|b| b.0.is_empty() && b.1 == 0)
    );

    // Post-gap output equals a fresh bank started at the gap.
    let mut fresh = Pfb::new(PfbConfig::new(m)).unwrap();
    let rf = run_pfb(&mut fresh, seg2, gap_start, &prov, &[seg2.len()]);
    assert_eq!(
        rf.first.source_index,
        gap_start as f64 + (l - 1) as f64 / 2.0
    );
    for (c, (got, want)) in after.iter().zip(&rf.channels).enumerate() {
        assert_eq!(got, want, "channel {c}");
    }

    // A retune without a gap also resets: output restarts a full window after the retune and
    // equals a fresh bank on the retuned stream.
    let retuned = provenance(434e6, fs);
    let mut p = Pfb::new(PfbConfig::new(m)).unwrap();
    let _ = run_pfb(&mut p, &x[..10_000], 0, &prov, &[10_000]);
    let h = header(10_000, &retuned, Discontinuity::NONE);
    let out = p.process(InputInfo::from(&h), &x[10_000..20_000]);
    assert!(out.header.discontinuity.contains(Discontinuity::RETUNE));
    assert_eq!(out.frames, 1 + (10_000 - l) / d);
    assert_eq!(
        out.header.time.source_index,
        10_000.0 + (l - 1) as f64 / 2.0
    );
    let got = out.channel(3).unwrap().to_vec();
    let mut fresh = Pfb::new(PfbConfig::new(m)).unwrap();
    let rf = run_pfb(&mut fresh, &x[10_000..20_000], 10_000, &retuned, &[10_000]);
    assert_eq!(got, rf.channels[3]);
}

#[test]
fn pfb_gain_change_keeps_filter_state() {
    let m = 16;
    let fs = 1.6e6;
    let prov = provenance(433e6, fs);
    let louder = provenance_with(433e6, fs, 24.0);
    let mut rng = Rng::new(9);
    let x = synth::complex_noise(&mut rng, 20_000, 1e-2);
    let mut mono = Pfb::new(PfbConfig::new(m)).unwrap();
    let whole = run_pfb(&mut mono, &x, 0, &prov, &[20_000]);
    let mut split = Pfb::new(PfbConfig::new(m)).unwrap();
    let first = run_pfb(&mut split, &x[..9_000], 0, &prov, &[9_000]);
    let h = header(9_000, &louder, Discontinuity::GAIN_CHANGE);
    let out = split.process(InputInfo::from(&h), &x[9_000..]);
    assert!(
        out.header
            .discontinuity
            .contains(Discontinuity::GAIN_CHANGE)
    );
    let mut joined = first.channels[7].clone();
    joined.extend(out.channel(7).unwrap().iter());
    assert_eq!(joined, whole.channels[7]);
}
