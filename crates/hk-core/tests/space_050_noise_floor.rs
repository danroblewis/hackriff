//! SPACE-050 (natural radio noise-floor survey): the substrate part. A noise-floor recording
//! must replay through the source API and the ring with its statistics and provenance intact,
//! or every downstream noise-floor estimate (C08) is built on sand.

mod common;

use common::*;
use hk_core::{
    Discontinuity, Pacing, ReadOutcome, ReplayOptions, RingConfig, SigmfReplaySource, Source,
    ring_buffer,
};
use hk_model::TimestampMethod;
use hk_model::sigmf::Datatype;
use num_complex::Complex32;

const N: usize = 262_144;
const FS: f64 = 2e6;
/// Per-component standard deviation, ci8 counts.
const SIGMA_COUNTS: f64 = 20.0;

#[test]
fn space_050_noise_floor_fixture_replays_with_known_variance_and_provenance() {
    // Generate: complex Gaussian noise, per-component sigma 20 counts, quantised to ci8.
    let mut rng = Rng::new(0x5ace_0050);
    let mut data = Vec::with_capacity(2 * N);
    let mut clipped = 0usize;
    for _ in 0..N {
        let (i, q) = rng.gaussian_pair();
        for v in [i, q] {
            let x = (v * SIGMA_COUNTS).round();
            if !(-128.0..=127.0).contains(&x) {
                clipped += 1;
            }
            data.push(x.clamp(-128.0, 127.0) as i8 as u8);
        }
    }
    assert_eq!(clipped, 0, "sigma 20 counts should never clip");

    let dir = TempDir::new("space-050");
    let mut meta = meta(Datatype::Ci8, FS);
    meta.global.description = Some("SPACE-050 synthetic noise floor, sigma 20 counts".into());
    let mut prov = provenance("synthetic:space-050", 3.5e6, FS);
    prov.tune.lna_db = 40.0;
    prov.tune.vga_db = 30.0;
    prov.tune.amp_on = true;
    meta.global.provenance = Some(prov.clone());
    meta.captures.push(capture(0, 3.5e6));
    meta.captures[0].datetime = Some("2026-09-13T03:00:00Z".into());
    let path = write_recording(dir.path(), "noise", &meta, &data);

    // Replay through the source API into the ring; read back through an independent reader.
    let mut source = SigmfReplaySource::open(
        &path,
        ReplayOptions {
            block_len: 65_536,
            pacing: Pacing::Unpaced,
        },
    )
    .unwrap();
    let (mut writer, ring) = ring_buffer::<Complex32>(RingConfig {
        sample_capacity: N,
        block_capacity: 16,
    });
    let mut reader = ring.reader();
    let mut block = Vec::with_capacity(65_536);
    let mut out = vec![Complex32::default(); 10_000];
    let (mut sum_sq, mut sum, mut count) = (0.0f64, Complex32::default(), 0usize);
    let mut expect_next = 0u64;
    while let Some(header) = source.read_block(&mut block).unwrap() {
        assert_eq!(
            *header.provenance.get(),
            prov,
            "provenance attached at the source"
        );
        writer.push(&header, &block).unwrap();
        loop {
            match reader.read(&mut out) {
                ReadOutcome::Data(chunk) => {
                    assert_eq!(chunk.first_sample(), expect_next, "no gap");
                    assert_eq!(
                        *chunk.provenance.get(),
                        prov,
                        "provenance intact through the ring"
                    );
                    assert_eq!(
                        chunk.provenance.timestamp_method,
                        TimestampMethod::Synthetic
                    );
                    if chunk.first_sample() == 0 {
                        assert_eq!(chunk.discontinuity, Discontinuity::STREAM_START);
                    }
                    for s in &out[..chunk.len] {
                        sum_sq += f64::from(s.norm_sqr());
                        sum += *s;
                    }
                    count += chunk.len;
                    expect_next = chunk.end_sample();
                }
                ReadOutcome::Empty => break,
                other => panic!("unexpected {other:?}"),
            }
        }
    }
    assert_eq!(count, N);
    assert_eq!(reader.lost_samples() + reader.gap_samples(), 0);

    // Expected E|z|^2 in normalised units: 2 (sigma^2 + 1/12 quantisation) / 128^2.
    let expected = 2.0 * (SIGMA_COUNTS * SIGMA_COUNTS + 1.0 / 12.0) / (128.0 * 128.0);
    let measured = sum_sq / N as f64;
    let rel = (measured - expected).abs() / expected;
    // |z|^2 is exponential: relative standard error 1/sqrt(N) ≈ 0.2%; allow 2%.
    assert!(
        rel < 0.02,
        "variance {measured:.6e} vs expected {expected:.6e} ({rel:.3})"
    );
    let mean = sum / N as f32;
    assert!(mean.norm() < 0.005, "mean {mean} should be ~0");

    // The same statistic straight from the bytes agrees exactly with the replayed samples.
    let direct: f64 = data
        .chunks_exact(2)
        .map(|c| {
            let (i, q) = (f64::from(c[0] as i8) / 128.0, f64::from(c[1] as i8) / 128.0);
            i * i + q * q
        })
        .sum::<f64>()
        / N as f64;
    assert!((direct - measured).abs() / direct < 1e-5);
}
