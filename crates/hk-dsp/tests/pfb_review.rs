//! T-008 review fixes for the C11 PFB: any rate change resets filter state whatever the reset
//! flags; even (non-power-of-two) channel counts for real rasters (25 kHz at 20 Msps is
//! M = 800); and a raster centre offset. Channel streams feed SIGNAL-062 / AWARE-036 /
//! SIGNAL-001 chains.

mod common;

use std::f64::consts::PI;

use common::*;
use hk_core::{Discontinuity, ProvenanceHandle};
use hk_dsp::synth::{self, Rng};
use hk_dsp::{ChannelTime, InputInfo, Pfb, PfbBackend, PfbConfig};
use num_complex::Complex32;

/// Runs `x` from stream index `start` in `chunk`-sample blocks; returns each active channel's
/// samples (indexed by channel) and the first non-empty block's time map.
fn run(
    pfb: &mut Pfb,
    x: &[Complex32],
    start: u64,
    prov: &ProvenanceHandle,
    chunk: usize,
) -> (Vec<Vec<Complex32>>, ChannelTime) {
    let mut channels = vec![Vec::new(); pfb.config().channels];
    let mut first = None;
    for (i, c) in x.chunks(chunk).enumerate() {
        let flags = if i == 0 {
            Discontinuity::STREAM_START
        } else {
            Discontinuity::NONE
        };
        let h = header(start + (i * chunk) as u64, prov, flags);
        let out = pfb.process(InputInfo::from(&h), c);
        if out.frames > 0 && first.is_none() {
            first = Some(out.header.time);
        }
        for &ch in out.active_channels() {
            channels[ch].extend(out.channel(ch).unwrap().iter());
        }
    }
    (channels, first.expect("output"))
}

fn rel_db(y: &[Complex32], power: f64) -> f64 {
    let p = y.iter().map(|s| f64::from(s.norm_sqr())).sum::<f64>() / y.len() as f64;
    10.0 * (p / power).log10()
}

#[test]
fn pfb_rate_change_resets_even_without_the_flag() {
    let m = 16;
    let prov1 = provenance(433e6, 1.6e6);
    let prov2 = provenance(433e6, 3.2e6);
    let mut rng = Rng::new(21);
    let x = synth::complex_noise(&mut rng, 20_000, 1e-2);
    let mut pfb = Pfb::new(PfbConfig::new(m)).unwrap();
    pfb.set_reset_on(Discontinuity::STREAM_START | Discontinuity::RETUNE | Discontinuity::GAP);
    let l = pfb.taps();
    let _ = run(&mut pfb, &x[..10_000], 0, &prov1, 10_000);
    let h = header(10_000, &prov2, Discontinuity::NONE);
    let out = pfb.process(InputInfo::from(&h), &x[10_000..]);
    assert!(
        out.header
            .discontinuity
            .contains(Discontinuity::RATE_CHANGE)
    );
    assert_eq!(
        out.frames,
        1 + (10_000 - l) / (m / 2),
        "window restarted at the rate change"
    );
    assert_eq!(out.header.sample_rate_hz, 400e3);
    assert_eq!(
        out.header.time.source_index,
        10_000.0 + (l - 1) as f64 / 2.0
    );
    let got = out.channel(5).unwrap().to_vec();
    let mut fresh = Pfb::new(PfbConfig::new(m)).unwrap();
    let (want, _) = run(&mut fresh, &x[10_000..], 10_000, &prov2, 10_000);
    assert_eq!(got, want[5]);
}

#[test]
fn pfb_even_non_power_of_two_channel_count() {
    let m = 12;
    let fs = 1.2e6;
    let spacing = fs / m as f64;
    let prov = provenance(433e6, fs);
    let cfg = PfbConfig::new(m);
    let mut pfb = Pfb::new(cfg.clone()).unwrap();
    let len = pfb.taps() + 48 * m / 2;
    for c in [0usize, 5, 6, 11] {
        for frac in [0.0, 0.5] {
            let delta = frac * spacing;
            let x = synth::tone(0, len, cfg.channel_offset_hz(c, fs) + delta, fs, 0.25, 0.3);
            pfb.reset();
            let (y, _) = run(&mut pfb, &x, 0, &prov, 1_000);
            let own = rel_db(&y[c], 0.25);
            assert!(own.abs() < 0.02, "M=12 channel {c} +{frac}Δ: {own:.4} dB");
            if frac == 0.5 {
                let up = rel_db(&y[(c + 1) % m], 0.25);
                assert!(
                    up.abs() < 0.02,
                    "boundary tone in channel {}: {up:.4} dB",
                    (c + 1) % m
                );
            } else {
                for j in (0..m).filter(|&j| j != c) {
                    let leak = rel_db(&y[j], 0.25);
                    assert!(
                        leak <= -60.0,
                        "M=12 tone in {c} leaks {leak:.2} dB into {j}"
                    );
                }
                let want = Complex32::from_polar(0.5, 0.3);
                let worst = y[c]
                    .iter()
                    .map(|s| f64::from((s - want).norm()) / 0.5)
                    .fold(0.0, f64::max);
                assert!(worst < 0.01, "M=12 channel {c}: phase error {worst}");
            }
        }
    }
    // Chunking invariance.
    let mut rng = Rng::new(12);
    let x = synth::complex_noise(&mut rng, 9_000, 1e-2);
    let (a, _) = run(&mut Pfb::new(cfg.clone()).unwrap(), &x, 77, &prov, 9_000);
    let (b, _) = run(&mut Pfb::new(cfg).unwrap(), &x, 77, &prov, 131);
    assert_eq!(a, b);
}

#[test]
fn pfb_25_khz_raster_at_20_msps() {
    let fs = 20e6;
    let cfg = PfbConfig {
        active: Some(vec![412, 413, 414, 300]),
        ..PfbConfig::new(800)
    };
    assert_eq!(cfg.channel_spacing_hz(fs), 25e3);
    let prov = provenance(460e6, fs);
    let mut pfb = Pfb::new(cfg.clone()).unwrap();
    let len = pfb.taps() + 20 * 400;
    let f = cfg.channel_offset_hz(413, fs);
    assert_eq!(f, 325e3);
    let x = synth::tone(0, len, f, fs, 0.25, 0.0);
    let (y, _) = run(&mut pfb, &x, 0, &prov, 65_536);
    assert!(rel_db(&y[413], 0.25).abs() < 0.005);
    for j in [412, 414, 300] {
        let leak = rel_db(&y[j], 0.25);
        assert!(leak <= -60.0, "M=800 channel {j}: {leak:.2} dB");
    }
}

#[test]
fn pfb_raster_offset_moves_channel_centres() {
    let m = 16;
    let fs = 1.6e6;
    let raster = 30e3;
    let cfg = PfbConfig {
        raster_offset_hz: raster,
        ..PfbConfig::new(m)
    };
    let prov = provenance(433e6, fs);
    let start = 1u64 << 40;
    let c = 5;
    let f = cfg.channel_offset_hz(c, fs);
    assert_eq!(f, raster - 3.0 * 100e3);
    assert_eq!(cfg.channel_for_offset_hz(f + 20e3, fs), c);
    let mut pfb = Pfb::new(cfg.clone()).unwrap();
    let len = pfb.taps() + 40 * m / 2;
    // A tone on the shifted channel centre comes out at DC with its absolute-index phase.
    let x = synth::tone(start, len, f, fs, 0.25, 0.4);
    let (y, t) = run(&mut pfb, &x, start, &prov, 700);
    assert!(rel_db(&y[c], 0.25).abs() < 0.005);
    let worst = y[c]
        .iter()
        .map(|s| f64::from((s - Complex32::from_polar(0.5, 0.4)).norm()) / 0.5)
        .fold(0.0, f64::max);
    assert!(worst < 0.01, "raster channel phase error {worst}");
    for j in [c - 1, c + 1, 12] {
        let leak = rel_db(&y[j], 0.25);
        assert!(leak <= -60.0, "raster offset: channel {j} {leak:.2} dB");
    }
    // An offset tone follows the time map: A·e^{j(φ + 2π·δ·τ/fs)}.
    let delta = 17e3;
    let x = synth::tone(start, len, f + delta, fs, 0.25, 0.0);
    pfb.reset();
    let (y, t2) = run(&mut pfb, &x, start, &prov, 1_024);
    assert_eq!(t.source_index, t2.source_index);
    // Reference phase from the generator's own formula (w·index) at the mapped source index.
    let w = 2.0 * PI * delta / fs;
    let worst = y[c]
        .iter()
        .enumerate()
        .map(|(k, s)| {
            let phase = (w * t2.source_index_of(k)).rem_euclid(2.0 * PI);
            f64::from((s - Complex32::from_polar(0.5, phase as f32)).norm()) / 0.5
        })
        .fold(0.0, f64::max);
    assert!(worst < 0.01, "raster offset tone vs time map: {worst}");
}
