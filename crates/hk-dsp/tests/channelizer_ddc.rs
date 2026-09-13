//! C11 on-demand DDC (T-008): tone frequency, amplitude and phase against the analytic
//! baseband, stopband and alias rejection, phase continuity across block boundaries, resets on
//! discontinuities, re-planning on a rate change, and the serde data spec. The DDC is the
//! front of the SIGNAL-062 / AWARE-036 / SIGNAL-001 chains (see those tests).

mod common;

use std::f64::consts::PI;

use common::*;
use hk_core::{Discontinuity, ProvenanceHandle};
use hk_dsp::synth::{self, Rng};
use hk_dsp::{ChannelTime, Ddc, DdcSpec, InputInfo, IqSample, ResampleKind};
use num_complex::Complex32;

const FS: f64 = 2e6;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Single,
    Integer,
    Rational,
    Fractional,
}

fn kind_of(ddc: &Ddc) -> Kind {
    match ddc.plan().resample.as_ref().map(|r| r.kind) {
        None => Kind::Single,
        Some(ResampleKind::Integer { .. }) => Kind::Integer,
        Some(ResampleKind::Rational { .. }) => Kind::Rational,
        Some(ResampleKind::Fractional { .. }) => Kind::Fractional,
    }
}

fn summary(ddc: &Ddc) -> String {
    let p = ddc.plan();
    format!(
        "stage 1 /{} ({} taps), stage 2 {:?} ({} taps/phase), {:.1} mults/input sample",
        p.xlate_decimation,
        p.xlate.len(),
        p.resample.as_ref().map(|r| r.kind),
        p.resample.as_ref().map_or(0, |r| r.taps_per_phase),
        p.cost_per_input_sample()
    )
}

/// One spec per plan shape.
fn cases() -> Vec<(DdcSpec, Kind)> {
    vec![
        (
            DdcSpec::new(200e3, 100e3).with_output_rate(1e6),
            Kind::Single,
        ),
        (
            DdcSpec::new(312_345.6, 40e3).with_output_rate(50e3),
            Kind::Integer,
        ),
        (
            DdcSpec::new(-412_345.6, 30e3).with_output_rate(48e3),
            Kind::Rational,
        ),
        (
            DdcSpec::new(555_555.5, 30e3).with_output_rate(47_123.4),
            Kind::Fractional,
        ),
    ]
}

struct Out {
    samples: Vec<Complex32>,
    first: ChannelTime,
}

fn run<T: IqSample>(
    ddc: &mut Ddc,
    x: &[T],
    start: u64,
    prov: &ProvenanceHandle,
    chunks: &[usize],
) -> Out {
    let mut samples = Vec::new();
    let mut first: Option<ChannelTime> = None;
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
        let b = ddc.process(InputInfo::from(&h), &x[pos..pos + n]).unwrap();
        if !b.samples.is_empty() {
            let t = b.header.time;
            match first {
                None => first = Some(t),
                Some(f) => {
                    let want = f.source_index_of(samples.len());
                    assert!(
                        (t.source_index - want).abs() < 1e-6,
                        "{} vs {want}",
                        t.source_index
                    );
                    assert_eq!(t.out_index, f.out_index + samples.len() as u64);
                }
            }
            samples.extend_from_slice(b.samples);
        }
        pos += n;
        i += 1;
    }
    Out {
        samples,
        first: first.expect("DDC produced output"),
    }
}

fn mean_power(y: &[Complex32]) -> f64 {
    y.iter().map(|s| f64::from(s.norm_sqr())).sum::<f64>() / y.len() as f64
}

#[test]
fn ddc_tone_lands_at_baseband_with_amplitude_and_phase() {
    let prov = provenance(433.92e6, FS);
    for (spec, kind) in cases() {
        let mut ddc = Ddc::new(spec.clone(), FS).unwrap();
        assert_eq!(kind_of(&ddc), kind, "{spec:?}: {}", summary(&ddc));
        let delta = 0.37 * spec.bandwidth_hz / 2.0;
        let (a, phi) = (0.5f64, 0.3f64);
        let x = synth::tone(0, 200_000, spec.center_offset_hz + delta, FS, a * a, phi);
        let out = run(&mut ddc, &x, 0, &prov, &[65_536]);
        assert!(out.samples.len() > 1000);
        assert_eq!(
            out.first.source_per_output,
            FS / spec.output_rate_hz.unwrap()
        );
        let mut worst = 0.0f64;
        for (k, s) in out.samples.iter().enumerate() {
            let tau = out.first.source_index_of(k);
            let ph = (phi + 2.0 * PI * delta * tau / FS).rem_euclid(2.0 * PI);
            let want = Complex32::from_polar(a as f32, ph as f32);
            worst = worst.max(f64::from((s - want).norm()) / a);
        }
        let err_db = 20.0 * worst.log10();
        assert!(
            err_db < -45.0,
            "{kind:?}: worst deviation from analytic baseband {err_db:.1} dB ({})",
            summary(&ddc)
        );
        eprintln!("DDC {kind:?}: {}; tone error {err_db:.1} dB", summary(&ddc));
    }
}

#[test]
fn ddc_rejects_out_of_band_and_alias_tones() {
    let prov = provenance(433.92e6, FS);
    for (spec, kind) in cases() {
        let mut ddc = Ddc::new(spec.clone(), FS).unwrap();
        let (fp, fst, out, fs1) = {
            let p = ddc.plan();
            (
                p.passband_hz,
                p.stopband_hz,
                p.output_rate_hz,
                p.stage1_rate_hz(),
            )
        };
        let dists = [
            fst * 1.01,
            -fst * 1.01,
            1.7 * fst,
            out,
            -out,
            out + 0.3 * fp,
            3.0 * out + 1234.5,
            fs1 + 0.2 * fp,
            -fs1,
            2.0 * fs1 - 0.5 * fp,
        ];
        let mut worst = f64::NEG_INFINITY;
        for d in dists {
            let f = spec.center_offset_hz + d;
            if f.abs() > 0.49 * FS || d.abs() < fst {
                continue;
            }
            ddc.reset();
            let x = synth::tone(0, 120_000, f, FS, 0.25, 0.0);
            let y = run(&mut ddc, &x, 0, &prov, &[50_000]).samples;
            let rel = 10.0 * (mean_power(&y) / 0.25).log10();
            assert!(
                rel <= -59.9,
                "{kind:?}: tone at centre{d:+.0} Hz only {rel:.2} dB down"
            );
            worst = worst.max(rel);
        }
        eprintln!("DDC {kind:?}: worst out-of-band/alias tone {worst:.2} dB");
    }
}

#[test]
fn ddc_output_is_phase_continuous_across_block_boundaries() {
    let prov = provenance(433.92e6, FS);
    let mut rng = Rng::new(0x8008);
    let len = 150_000;
    let mut x = synth::complex_noise(&mut rng, len, 1e-3);
    for (spec, _) in cases() {
        let f = spec.center_offset_hz + 0.2 * spec.bandwidth_hz / 2.0;
        synth::add_into(&mut x, &synth::tone(1000, len, f, FS, 0.01, 0.0));
    }
    let (xi8, _) = synth::quantize_ci8(&x);
    let xc: Vec<Complex32> = xi8.iter().map(|s| s.to_complex32()).collect();
    for (spec, kind) in cases() {
        let mono = run(
            &mut Ddc::new(spec.clone(), FS).unwrap(),
            &x,
            1000,
            &prov,
            &[x.len()],
        );
        let chunked = run(
            &mut Ddc::new(spec.clone(), FS).unwrap(),
            &x,
            1000,
            &prov,
            &[1, 3, 64, 4095, 7, 20_000, 333],
        );
        assert_eq!(
            mono.samples, chunked.samples,
            "{kind:?}: chunking changed output"
        );
        assert_eq!(mono.first, chunked.first);
        let from_i8 = run(
            &mut Ddc::new(spec.clone(), FS).unwrap(),
            &xi8,
            1000,
            &prov,
            &[5000, 17],
        );
        let from_c32 = run(
            &mut Ddc::new(spec.clone(), FS).unwrap(),
            &xc,
            1000,
            &prov,
            &[xc.len()],
        );
        assert_eq!(
            from_i8.samples, from_c32.samples,
            "{kind:?}: ci8 path differs"
        );
    }
}

#[test]
fn ddc_resets_on_gap_and_carries_flags_to_next_output() {
    let prov = provenance(433.92e6, FS);
    let mut rng = Rng::new(3);
    let x = synth::complex_noise(&mut rng, 140_000, 1e-2);
    for (spec, kind) in cases() {
        let mut ddc = Ddc::new(spec.clone(), FS).unwrap();
        let _ = run(&mut ddc, &x[..60_000], 0, &prov, &[60_000]);
        let seg2 = &x[60_000..];
        let gap_start = 70_000u64;
        let mut got = Vec::new();
        let mut first: Option<(Discontinuity, u64, ChannelTime)> = None;
        let mut pos = 0;
        while pos < seg2.len() {
            let n = if pos < 5_000 { 10 } else { 20_000 }.min(seg2.len() - pos);
            let h = header(gap_start + pos as u64, &prov, Discontinuity::NONE);
            let b = ddc
                .process(InputInfo::from(&h), &seg2[pos..pos + n])
                .unwrap();
            if b.samples.is_empty() {
                assert_eq!(b.header.discontinuity, Discontinuity::NONE);
            } else if first.is_none() {
                first = Some((
                    b.header.discontinuity,
                    b.header.dropped_before,
                    b.header.time,
                ));
                got.extend_from_slice(b.samples);
            } else {
                assert!(b.header.discontinuity.is_empty() && b.header.dropped_before == 0);
                got.extend_from_slice(b.samples);
            }
            pos += n;
        }
        let (flags, dropped, t) = first.unwrap();
        assert!(flags.contains(Discontinuity::GAP), "{kind:?}: {flags:?}");
        assert_eq!(dropped, 10_000, "{kind:?}");
        let fresh = run(
            &mut Ddc::new(spec.clone(), FS).unwrap(),
            seg2,
            gap_start,
            &prov,
            &[seg2.len()],
        );
        assert_eq!(got, fresh.samples, "{kind:?}: post-gap output != fresh DDC");
        assert_eq!(t.source_index, fresh.first.source_index);
        assert!(t.source_index > gap_start as f64);
    }
}

#[test]
fn ddc_retune_resets_and_gain_change_does_not() {
    let prov = provenance(433.92e6, FS);
    let retuned = provenance(434.5e6, FS);
    let louder = provenance_with(433.92e6, FS, 32.0);
    let spec = cases()[1].0.clone();
    let mut rng = Rng::new(11);
    let x = synth::complex_noise(&mut rng, 120_000, 1e-2);

    let mut ddc = Ddc::new(spec.clone(), FS).unwrap();
    let _ = run(&mut ddc, &x[..60_000], 0, &prov, &[60_000]);
    let h = header(60_000, &retuned, Discontinuity::NONE);
    let b = ddc.process(InputInfo::from(&h), &x[60_000..]).unwrap();
    assert!(b.header.discontinuity.contains(Discontinuity::RETUNE));
    assert_eq!(b.center_hz(), 434.5e6 + spec.center_offset_hz);
    let got = b.samples.to_vec();
    let fresh = run(
        &mut Ddc::new(spec.clone(), FS).unwrap(),
        &x[60_000..],
        60_000,
        &retuned,
        &[60_000],
    );
    assert_eq!(got, fresh.samples);

    let mono = run(
        &mut Ddc::new(spec.clone(), FS).unwrap(),
        &x,
        0,
        &prov,
        &[x.len()],
    );
    let mut ddc = Ddc::new(spec.clone(), FS).unwrap();
    let mut joined = run(&mut ddc, &x[..60_000], 0, &prov, &[60_000]).samples;
    let h = header(60_000, &louder, Discontinuity::GAIN_CHANGE);
    let b = ddc.process(InputInfo::from(&h), &x[60_000..]).unwrap();
    assert!(b.header.discontinuity.contains(Discontinuity::GAIN_CHANGE));
    assert!(!b.header.discontinuity.contains(Discontinuity::RETUNE));
    joined.extend_from_slice(b.samples);
    assert_eq!(
        joined, mono.samples,
        "gain change must not reset filter state"
    );
}

#[test]
fn ddc_replans_on_rate_change() {
    let spec = DdcSpec::new(312_345.6, 40e3).with_output_rate(50e3);
    let prov2 = provenance(433.92e6, 2e6);
    let prov4 = provenance(433.92e6, 4e6);
    let delta = 5_000.0;
    let f = spec.center_offset_hz + delta;
    let mut ddc = Ddc::new(spec.clone(), 2e6).unwrap();
    let _ = run(
        &mut ddc,
        &synth::tone(0, 60_000, f, 2e6, 0.25, 0.0),
        0,
        &prov2,
        &[60_000],
    );
    let x4 = synth::tone(60_000, 120_000, f, 4e6, 0.25, 0.0);
    let h = header(60_000, &prov4, Discontinuity::NONE);
    let b = ddc.process(InputInfo::from(&h), &x4).unwrap();
    assert!(b.header.discontinuity.contains(Discontinuity::RATE_CHANGE));
    assert_eq!(b.header.sample_rate_hz, 50e3);
    let t = b.header.time;
    assert_eq!(t.source_per_output, 80.0);
    let samples = b.samples.to_vec();
    assert_eq!(ddc.plan().input_rate_hz, 4e6);
    assert!(samples.len() > 1000);
    for (k, s) in samples.iter().enumerate() {
        let tau = t.source_index_of(k);
        let ph = (2.0 * PI * delta * tau / 4e6).rem_euclid(2.0 * PI);
        let want = Complex32::from_polar(0.5, ph as f32);
        assert!((s - want).norm() < 0.005, "sample {k}: {s} vs {want}");
    }
}

#[test]
fn ddc_spec_is_a_serde_data_spec() {
    let spec: DdcSpec = serde_json::from_str(
        r#"{"center_offset_hz": -250000, "bandwidth_hz": 12500, "transition": 2500}"#,
    )
    .unwrap();
    assert_eq!(spec.transition_hz, Some(2500.0));
    assert_eq!(spec.output_rate_hz, None);
    let ddc = Ddc::new(spec.clone(), FS).unwrap();
    assert_eq!(
        ddc.output_rate_hz(),
        25_000.0,
        "default: integer decimation, >= 2x bandwidth"
    );
    assert_eq!(ddc.plan().stopband_hz, 6_250.0 + 2_500.0);
    let json = serde_json::to_value(&spec).unwrap();
    assert!(json.get("output_rate_hz").is_none());
    assert_eq!(serde_json::from_value::<DdcSpec>(json).unwrap(), spec);
    assert!(
        serde_json::from_str::<DdcSpec>(r#"{"center_offset_hz": 0, "bandwidth_hz": 1, "gain": 2}"#)
            .is_err()
    );
    assert!(serde_json::from_str::<DdcSpec>(r#"{"bandwidth_hz": 1}"#).is_err());
    assert!(Ddc::new(DdcSpec::new(990e3, 50e3), FS).is_err());
}
