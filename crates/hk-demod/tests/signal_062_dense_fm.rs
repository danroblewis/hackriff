//! T-099 (SIGNAL-062): mode selection with strong adjacent channels. A dense FM scene: the target
//! station (stereo WFM, RDS PI C0DE) with equal-power neighbours 200 kHz either side (their own
//! PIs), each from the `fm_broadcast_rds` synthesiser, summed and quantised as a HackRF would.
//!
//! The receiver sees only IQ, provenance and a detection-like box around the target; the PIs are
//! the scene's private truth, compared after the run.

mod common;

use common::*;
use hk_demod::{AnalogMode, AnalogReceiver, AnalogSession};
use hk_e2e::SynthRequest;
use hk_estimate::SnippetRequest;
use num_complex::Complex;

const FS: f64 = 1.2e6;
const TARGET_HZ: f64 = 100e3;
const SPACING_HZ: f64 = 200e3;

struct Station {
    offset_hz: f64,
    pi: &'static str,
    seed: u64,
}

fn synth(s: &Station, secs: f64) -> Option<(Vec<Complex<i8>>, f64)> {
    let req = SynthRequest::new("fm_broadcast_rds")
        .seed(s.seed)
        .param("sample_rate", FS)
        .param("offset_hz", s.offset_hz)
        .param("duration_s", secs)
        .param("pi_hex", s.pi)
        .param("noise_dbfs", -60.0);
    let out = match req.generate() {
        Ok(out) => out,
        Err(e) if e.is_unavailable() && !hk_e2e::synth::require_synth() => {
            eprintln!("SKIP: synth unavailable: {e}");
            return None;
        }
        Err(e) => panic!("synth failed: {e}"),
    };
    let fx = out.fixture(0).unwrap();
    Some((read_ci8(&fx.data_path()), fx.center_hz_at(0).unwrap()))
}

/// Target plus neighbours at ±`SPACING_HZ`, summed; with the capture centre and the target's PI.
fn dense_scene(secs: f64) -> Option<(Vec<Complex<i8>>, f64, &'static str)> {
    let stations = [
        Station {
            offset_hz: TARGET_HZ,
            pi: "C0DE",
            seed: 991,
        },
        Station {
            offset_hz: TARGET_HZ - SPACING_HZ,
            pi: "1A2B",
            seed: 992,
        },
        Station {
            offset_hz: TARGET_HZ + SPACING_HZ,
            pi: "3C4D",
            seed: 993,
        },
    ];
    let mut sum: Vec<(i32, i32)> = Vec::new();
    let mut centre = 0.0;
    for s in &stations {
        let (iq, c) = synth(s, secs)?;
        centre = c;
        if sum.is_empty() {
            sum = vec![(0, 0); iq.len()];
        }
        for (a, z) in sum.iter_mut().zip(&iq) {
            a.0 += i32::from(z.re);
            a.1 += i32::from(z.im);
        }
    }
    let q = |v: i32| v.clamp(-128, 127) as i8;
    let iq = sum
        .iter()
        .map(|&(i, q_)| Complex::new(q(i), q(q_)))
        .collect();
    Some((iq, centre, stations[0].pi))
}

fn run(iq: &[Complex<i8>], centre_hz: f64, box_offset_hz: f64, box_bw_hz: f64) -> AnalogSession {
    let prov = provenance(centre_hz, FS);
    let request = SnippetRequest {
        start_index: 0,
        end_index: iq.len() as u64,
        center_offset_hz: box_offset_hz,
        bandwidth_hz: box_bw_hz,
    };
    AnalogReceiver::default()
        .run(info(0, &prov), iq, &request)
        .unwrap()
}

#[test]
fn signal_062_dense_fm_selects_wfm_and_decodes_the_target_pi() {
    let Some((iq, centre, truth_pi)) = dense_scene(1.5) else {
        return;
    };
    let mut failures = Vec::new();
    // Detection-like boxes: centred, a few kHz off, and as wide as a merged row.
    for (off, bw) in [
        (0.0, 200e3),
        (0.0, 272e3),
        (0.0, 150e3),
        (8e3, 200e3),
        (-12e3, 240e3),
    ] {
        let s = run(&iq, centre, TARGET_HZ + off, bw);
        let pi = s.rds().and_then(|r| r.pi.as_ref().map(|p| p.hex()));
        eprintln!(
            "[{SIGNAL_062}] box {off:+.0} Hz / {bw:.0} Hz: mode {:?} ({:.2}) OBW99 {:?} pilot {:?} \
             PI {pi:?} reason {:?}",
            s.mode.mode,
            s.mode.confidence,
            s.mode.features.obw99_hz,
            s.mode.features.pilot.map(|p| p.found),
            s.mode.reason
        );
        // A box reaching the neighbours makes C13 abstain; the adjacent-channel measure decides.
        if bw >= 200e3 && s.mode.features.adjacent.is_none() {
            failures.push(format!(
                "box {off:+.0}/{bw:.0}: no adjacent-channel measure"
            ));
        }
        if s.mode.mode != AnalogMode::Wfm || pi.as_deref() != Some(truth_pi) {
            failures.push(format!(
                "box {off:+.0}/{bw:.0}: {:?}, PI {pi:?}",
                s.mode.mode
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "[{SIGNAL_062}] dense FM: {}",
        failures.join("; ")
    );
}
