//! SPACE-050 (natural radio noise-floor survey): a known noise floor injected as 8-bit
//! quantised IQ, replayed through hk-core's SigMF replay source and the `Complex<i8>` ring, is
//! recovered from the Welch PSD in dBFS within ±0.2 dB.

mod common;

use std::io::Cursor;

use common::*;
use hk_core::{
    Pacing, ReadOutcome, ReplayOptions, RingConfig, SigmfReplaySource, Source, ring_buffer,
};
use hk_dsp::synth::{self, Rng};
use hk_dsp::{InputInfo, PowerUnit, SpectrumFrame, StftConfig, StftProcessor, WelchConfig};
use hk_model::sigmf::SigmfMeta;
use num_complex::Complex;

const FS: f64 = 2e6;
const CENTER: f64 = 30e6;
const N: usize = 4096;
const K: usize = 16;

fn replay_frames(bytes: Vec<u8>) -> Vec<SpectrumFrame> {
    let meta = SigmfMeta::from_json_str(&format!(
        r#"{{"global": {{"core:datatype": "ci8", "core:version": "1.2.0", "core:sample_rate": {FS}}},
            "captures": [{{"core:sample_start": 0, "core:frequency": {CENTER}}}],
            "annotations": []}}"#
    ))
    .expect("meta");
    let options = ReplayOptions {
        block_len: 10_000,
        pacing: Pacing::Unpaced,
    };
    let mut source = SigmfReplaySource::from_reader(meta, Cursor::new(bytes), options).unwrap();
    let (mut writer, handle) = ring_buffer::<Complex<i8>>(RingConfig {
        sample_capacity: 1 << 18,
        block_capacity: 256,
    });
    let mut reader = handle.reader();
    let mut stft = StftProcessor::new(StftConfig::new(WelchConfig::new(N), K)).unwrap();

    let mut block = Vec::new();
    let mut out = vec![Complex::<i8>::default(); 8192];
    let mut frames = Vec::new();
    let mut drain = |reader: &mut hk_core::RingReader<Complex<i8>>,
                     stft: &mut StftProcessor,
                     frames: &mut Vec<SpectrumFrame>| {
        loop {
            match reader.read(&mut out) {
                ReadOutcome::Data(chunk) => {
                    stft.push(InputInfo::from(&chunk), &out[..chunk.len], |f| {
                        frames.push(f.clone())
                    });
                }
                ReadOutcome::Overrun { .. } => panic!("reader lapped"),
                ReadOutcome::Empty | ReadOutcome::Closed => break,
            }
        }
    };
    while let Some(h) = source.read_block_ci8(&mut block).unwrap() {
        writer.push(&h, &block).unwrap();
        drain(&mut reader, &mut stft, &mut frames);
    }
    drop(writer);
    drain(&mut reader, &mut stft, &mut frames);
    assert_eq!(stft.stats().samples_dropped, 0);
    frames
}

#[test]
fn space_050_injected_noise_floor_recovered_within_0_2_db() {
    // Per-component sigma in ci8 counts: a comfortable level and a coarse one where the 8-bit
    // quantisation noise (1/12 count² per component) is a visible part of the floor.
    for (sigma_counts, seed) in [(12.0, 0x5ace_0050u64), (3.0, 0x5ace_0051)] {
        let analog_variance = 2.0 * sigma_counts * sigma_counts / (128.0 * 128.0);
        // What an ideal 8-bit ADC delivers: signal plus uniform quantisation noise.
        let injected_variance = 2.0 * (sigma_counts * sigma_counts + 1.0 / 12.0) / (128.0 * 128.0);
        let injected_dbfs = db(injected_variance);
        let injected_dbfs_hz = injected_dbfs - db(FS);

        let len = 4 * K * (N / 2) + N;
        let mut rng = Rng::new(seed);
        let analog = synth::complex_noise(&mut rng, len, analog_variance);
        let (ci8, clipped) = synth::quantize_ci8(&analog);
        assert_eq!(clipped, 0);
        let frames = replay_frames(synth::ci8_bytes(&ci8));
        assert_eq!(frames.len(), 4);

        for (k, f) in frames.iter().enumerate() {
            let s = &f.spectrum;
            assert_eq!(f.t.sample_index, (k * K * N / 2) as u64);
            assert_eq!(s.f_center_hz, CENTER);
            assert_eq!(f.provenance.tune.center_hz, CENTER);
            assert_eq!(s.bins(), N);
            assert_eq!(s.bin_width_hz(), FS / N as f64);
            assert_eq!(s.resolution.n_avg, K as u32);

            let floor_dbfs_hz = db(mean(&s.psd));
            assert!(
                (floor_dbfs_hz - injected_dbfs_hz).abs() < 0.2,
                "sigma {sigma_counts}: recovered {floor_dbfs_hz:.3} vs injected \
                 {injected_dbfs_hz:.3} dBFS/Hz"
            );
            // The same floor integrated over the span is the injected dBFS.
            assert!((db(s.total_power()) - injected_dbfs).abs() < 0.2);
            // And in dBFS per RBW it is σ²·ENBW/N.
            let per_bin = db(mean(&s.psd)) + f64::from(s.db_offset(PowerUnit::DbfsPerBin));
            let want = injected_dbfs + db(s.resolution.window_metrics.enbw_bins / N as f64);
            assert!((per_bin - want).abs() < 0.2);
            // Pure noise: SK ≈ 1.
            assert!((mean(&s.sk) - 1.0).abs() < 0.05);
        }
        // The analog level is within tolerance too at sigma 12 (quantisation adds 0.003 dB).
        if sigma_counts > 10.0 {
            let f = db(mean(&frames[0].spectrum.psd));
            assert!((f - (db(analog_variance) - db(FS))).abs() < 0.2);
        }
    }
}
