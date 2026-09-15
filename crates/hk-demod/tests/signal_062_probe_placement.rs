//! T-073 / SIGNAL-062 regression: the Listen probe's mode decision must not depend on where the
//! probe lands in the stream.
//!
//! The signal is the hk-pipeline scripted radio's tone (`tests/support/radio.rs`: a clean i8
//! carrier, amplitude 40 in ±4 noise, 150 kHz off a 1 MS/s window) probed the way Listen probes
//! it (0.5 s, 20 kHz box). At some placements 8-bit quantisation left an in-phase residue of
//! ≈ 0.2 % depth whose t-statistic (3.1–3.7) cleared the unmodulated-carrier limit (3) but not
//! the AM rule (depth ≥ 5 %), so rules 0.2.0 abstained and Listen refused the carrier
//! (`no-analog-mode`, `listen_retune` flaking ~1 in 3). The first four placements below failed
//! before the fix; the others passed and pin the unchanged path.

mod common;

use common::*;
use hk_demod::AnalogMode;
use hk_demod::audio::probe;
use hk_estimate::SnippetRequest;
use num_complex::Complex;

const FS: f64 = 1e6;
const OFFSET_HZ: f64 = 150e3;
const PROBE: usize = 500_000;

fn step(state: &mut u64) {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
}

/// The scripted radio's tone from stream index `start` (same generator, advanced to `start`).
fn tone_ci8(start: u64, n: usize) -> Vec<Complex<i8>> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    for _ in 0..start {
        step(&mut state);
    }
    (0..n as u64)
        .map(|i| {
            step(&mut state);
            let noise = ((state >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 8.0;
            let ph = std::f64::consts::TAU * OFFSET_HZ * (start + i) as f64 / FS;
            Complex::new(
                (40.0 * ph.cos() + noise).round().clamp(-128.0, 127.0) as i8,
                (40.0 * ph.sin() - noise).round().clamp(-128.0, 127.0) as i8,
            )
        })
        .collect()
}

#[test]
fn signal_062_listen_probe_reads_a_clean_carrier_at_every_placement() {
    let prov = provenance(100.8e6, FS);
    let mut failures = Vec::new();
    // Block-aligned (16 384-sample) placements.
    for start in [
        1_016_384u64,
        1_049_152,
        1_081_920,
        1_098_304,
        1_000_000,
        1_032_768,
    ] {
        let iq = tone_ci8(start, PROBE);
        let request = SnippetRequest {
            start_index: start,
            end_index: start + PROBE as u64,
            center_offset_hz: OFFSET_HZ,
            bandwidth_hz: 20e3,
        };
        let d = probe(info(start, &prov), &iq, &request)
            .expect("probe runs")
            .mode;
        eprintln!(
            "[{SIGNAL_062}] start {start}: {} {:.2} (in-phase t {:?}, depth {:?})",
            d.mode.as_str(),
            d.confidence,
            d.features.inphase_sideband_t,
            d.features.am_depth
        );
        if d.mode != AnalogMode::Cw || d.confidence < 0.5 {
            failures.push(format!(
                "start {start}: {:?} {:.2} {:?} {:?}",
                d.mode, d.confidence, d.reason, d.features
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "[{SIGNAL_062}] clean carrier not read as CW:\n  {}",
        failures.join("\n  ")
    );
}
