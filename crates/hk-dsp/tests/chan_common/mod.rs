//! Helpers for the T-008 channelizer/DDC tests (used alongside `common`).
#![allow(dead_code)]

use hk_core::{Discontinuity, ProvenanceHandle};
use hk_dsp::fft::{CpuFft, FftBackend};
use hk_dsp::{ChannelTime, Ddc, InputInfo, IqSample};
use hk_e2e::Fixture;
use hk_model::sigmf::Datatype;
use num_complex::{Complex, Complex32};

use crate::common::header;

/// The fixture's samples as native `Complex<i8>` (hk-core convention: full scale 128).
pub fn read_ci8(fx: &Fixture) -> Vec<Complex<i8>> {
    assert_eq!(
        fx.meta.global.datatype,
        Datatype::Ci8,
        "fixture must be ci8"
    );
    let bytes = std::fs::read(fx.data_path()).expect("read .sigmf-data");
    bytes
        .chunks_exact(2)
        .map(|c| Complex::new(c[0] as i8, c[1] as i8))
        .collect()
}

/// Runs `x` (stream indices from 0) through `ddc` in `block`-sample chunks; returns every output
/// sample and the time map of the first non-empty block. Asserts header continuity.
pub fn run_ddc<T: IqSample>(
    ddc: &mut Ddc,
    x: &[T],
    prov: &ProvenanceHandle,
    block: usize,
) -> (Vec<Complex32>, ChannelTime) {
    let mut out = Vec::new();
    let mut first: Option<ChannelTime> = None;
    let mut index = 0u64;
    for (i, chunk) in x.chunks(block).enumerate() {
        let flags = if i == 0 {
            Discontinuity::STREAM_START
        } else {
            Discontinuity::NONE
        };
        let h = header(index, prov, flags);
        let b = ddc.process(InputInfo::from(&h), chunk).unwrap();
        if !b.samples.is_empty() {
            let t = b.header.time;
            match first {
                None => first = Some(t),
                Some(f) => {
                    let want = f.source_index_of(out.len());
                    assert!(
                        (t.source_index - want).abs() < 1e-6,
                        "time map discontinuous: {} vs {want}",
                        t.source_index
                    );
                    assert_eq!(t.out_index, f.out_index + out.len() as u64);
                }
            }
            out.extend_from_slice(b.samples);
        }
        index += chunk.len() as u64;
    }
    (out, first.expect("DDC produced output"))
}

/// Averaged Hann periodogram, DC-centred, power per Hz (white noise of variance σ² reads σ²/fs).
pub fn psd(y: &[Complex32], n: usize, fs: f64) -> Vec<f64> {
    let w: Vec<f64> = (0..n)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos())
        .collect();
    let s2: f64 = w.iter().map(|v| v * v).sum();
    let mut fft = CpuFft::new(n);
    let mut buf = vec![Complex32::default(); n];
    let mut acc = vec![0.0f64; n];
    let mut segs = 0;
    let mut start = 0;
    while start + n <= y.len() {
        for (b, (s, wi)) in buf.iter_mut().zip(y[start..start + n].iter().zip(&w)) {
            *b = s * *wi as f32;
        }
        fft.forward(&mut buf);
        for (k, v) in buf.iter().enumerate() {
            acc[(k + n / 2) % n] += f64::from(v.norm_sqr()) / (fs * s2);
        }
        segs += 1;
        start += n / 2;
    }
    assert!(segs > 0, "not enough samples for a {n}-point PSD");
    acc.iter_mut().for_each(|v| *v /= segs as f64);
    acc
}

/// Offset of DC-centred PSD bin `k`, Hz.
pub fn bin_hz(k: usize, n: usize, fs: f64) -> f64 {
    (k as f64 - (n / 2) as f64) * fs / n as f64
}

/// Quadrature discriminator: instantaneous frequency in Hz (length `y.len() − 1`).
pub fn discriminator(y: &[Complex32], fs: f64) -> Vec<f64> {
    y.windows(2)
        .map(|w| f64::from((w[1] * w[0].conj()).arg()) * fs / (2.0 * std::f64::consts::PI))
        .collect()
}

/// Amplitude of the real sinusoid at `freq_hz` in `x` (coherent projection, `2·|mean|`).
pub fn sine_amplitude(x: &[f64], freq_hz: f64, fs: f64) -> f64 {
    let mean = x.iter().sum::<f64>() / x.len() as f64;
    let (mut re, mut im) = (0.0, 0.0);
    for (n, v) in x.iter().enumerate() {
        let ph = 2.0 * std::f64::consts::PI * freq_hz * n as f64 / fs;
        re += (v - mean) * ph.cos();
        im += (v - mean) * ph.sin();
    }
    2.0 * (re * re + im * im).sqrt() / x.len() as f64
}

/// Mean `|x|²`.
pub fn power<T: IqSample>(x: &[T]) -> f64 {
    x.iter()
        .map(|&s| f64::from(s.to_complex32().norm_sqr()))
        .sum::<f64>()
        / x.len() as f64
}

pub fn db10(x: f64) -> f64 {
    10.0 * x.log10()
}
