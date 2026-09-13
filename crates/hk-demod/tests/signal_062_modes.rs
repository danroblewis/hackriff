//! SIGNAL-062 negatives for auto-mode selection: NBFM and AM tones are not WFM (and get their
//! own mode), noise is `unknown`. No manual mode input anywhere.

mod common;

use common::*;
use hk_demod::{AnalogMode, AnalogReceiver, RecordContext, write_session};
use hk_estimate::SnippetRequest;
use hk_model::Repository;
use num_complex::Complex32;

const FS: f64 = 250e3;

fn select(iq: &[Complex32], offset_hz: f64, bandwidth_hz: f64) -> hk_demod::AnalogSession {
    let prov = provenance(100e6, FS);
    let req = SnippetRequest {
        start_index: 0,
        end_index: iq.len() as u64,
        center_offset_hz: offset_hz,
        bandwidth_hz,
    };
    AnalogReceiver::default()
        .run(info(0, &prov), iq, &req)
        .unwrap()
}

#[test]
fn signal_062_nbfm_tone_is_nbfm_not_wfm() {
    let iq = nbfm_tone(FS, 1.0, -30e3, 0.1, 3e3, 1e3);
    let s = select(&iq, -30e3, 16e3);
    eprintln!("[{SIGNAL_062}] NBFM: {:?}", s.mode);
    assert_eq!(s.mode.mode, AnalogMode::Nbfm, "[{SIGNAL_062}] {:?}", s.mode);
    assert!(s.mode.confidence >= 0.5);
    assert!(s.wfm.is_none());
}

#[test]
fn signal_062_am_tone_is_am_not_wfm() {
    let iq = am_tone(FS, 1.0, 20e3, 0.1, 0.5, 1e3);
    let s = select(&iq, 20e3, 10e3);
    eprintln!("[{SIGNAL_062}] AM: {:?}", s.mode);
    assert_eq!(s.mode.mode, AnalogMode::Am, "[{SIGNAL_062}] {:?}", s.mode);
    let env = s.mode.features.envelope_variation.unwrap();
    // 50 % tone AM: κ − 1 = (1 + 3m² + 3m⁴/8)/(1 + m²/2)² − 1 ≈ 0.40.
    assert!((env - 0.40).abs() < 0.05, "envelope variation {env}");
    assert!(s.wfm.is_none());
}

#[test]
fn signal_062_noise_is_unknown() {
    let iq = noise_only(FS, 1.0);
    let s = select(&iq, 50e3, 12.5e3);
    eprintln!("[{SIGNAL_062}] noise: {:?}", s.mode);
    assert_eq!(
        s.mode.mode,
        AnalogMode::Unknown,
        "[{SIGNAL_062}] {:?}",
        s.mode
    );
    assert!(s.mode.reason.is_some());
    let mut repo = Repository::open_in_memory().unwrap();
    let w = write_session(&mut repo, &s, &RecordContext::default()).unwrap();
    assert_eq!(
        repo.demodulation(w.demodulation_id).unwrap().mode,
        "unknown"
    );
    assert!(w.emitter_id.is_none() && w.decode_ids.is_empty());
}
