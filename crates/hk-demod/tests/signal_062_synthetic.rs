//! SIGNAL-062 (RDS/RBDS), synthetic: the T-023 `fm_broadcast_rds` scenario (stereo WFM, 1 kHz /
//! 400 Hz tones, 19 kHz pilot, RDS 0A with PI C0DE, PS HACKRIFF), offset inside a 1.2 Msps window.
//!
//! - WFM auto-selected, pilot detected, PI C0DE and PS HACKRIFF exact, audio tones recovered,
//!   Emitter written with the label.
//! - The same station with no RDS (RDS deviation 0): WFM, no PI, no decodes, no emitter.
//! - A 60 ms burst of garbage mid-stream: no wrong PI or PS; the syndrome check rejects blocks.

mod common;

use common::*;
use hk_demod::{AnalogMode, AnalogReceiver, AnalogSession, RecordContext, write_session};
use hk_dsp::synth::Rng;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_estimate::SnippetRequest;
use hk_estimate::clock::tone_frequency;
use hk_model::{AnnotationTarget, DecodedIdentity, IdentityScheme, Repository};
use num_complex::Complex;

const FS: f64 = 1.2e6;
const OFFSET: f64 = 200e3;

fn run(iq: &[Complex<i8>], center_hz: f64, bandwidth_hz: f64) -> AnalogSession {
    let prov = provenance(center_hz, FS);
    let request = SnippetRequest {
        start_index: 0,
        end_index: iq.len() as u64,
        center_offset_hz: OFFSET,
        bandwidth_hz,
    };
    AnalogReceiver::default()
        .run(info(0, &prov), iq, &request)
        .unwrap()
}

fn synth(rds_deviation_hz: f64) -> Option<(Vec<Complex<i8>>, f64, f64)> {
    let req = SynthRequest::new("fm_broadcast_rds")
        .seed(62)
        .param("sample_rate", FS)
        .param("offset_hz", OFFSET)
        .param("duration_s", 2.0)
        .param("rds_deviation_hz", rds_deviation_hz);
    let out = match req.generate() {
        Ok(out) => out,
        Err(e) if e.is_unavailable() && !hk_e2e::synth::require_synth() => {
            eprintln!("SKIP: synth unavailable: {e}");
            return None;
        }
        Err(e) => panic!("synth failed: {e}"),
    };
    let fx = out.fixture(0).unwrap();
    let wfm = fx.of_kind("wfm-broadcast")[0];
    Some((
        read_ci8(&fx.data_path()),
        fx.center_hz_at(0).unwrap(),
        wfm.bandwidth_hz(),
    ))
}

#[test]
fn signal_062_synthetic_wfm_pilot_pi_ps_audio_and_emitter() {
    let _ = synth_or_skip!(
        SynthRequest::new("fm_broadcast_rds")
            .seed(62)
            .param("duration_s", 0.1)
    );
    let Some((iq, center, bw)) = synth(2000.0) else {
        return;
    };
    let s = run(&iq, center, bw);
    eprintln!("[{SIGNAL_062}] mode {:?}", s.mode);
    assert_eq!(s.mode.mode, AnalogMode::Wfm, "[{SIGNAL_062}] {:?}", s.mode);
    let wfm = s.wfm.as_ref().expect("WFM chain");
    let rds = s.rds().unwrap();
    eprintln!("[{SIGNAL_062}] pilot {:?}", wfm.pilot);
    eprintln!("[{SIGNAL_062}] RDS {rds:?}");
    assert_eq!(s.mode.mode, AnalogMode::Wfm, "[{SIGNAL_062}] {:?}", s.mode);
    assert!(wfm.pilot.present);
    let pilot = wfm.pilot.frequency_hz.unwrap();
    assert!(
        (pilot - 19_000.0).abs() < 0.5,
        "[{SIGNAL_062}] pilot {pilot}"
    );
    assert_eq!(rds.pi.expect("PI").hex(), "C0DE", "[{SIGNAL_062}]");
    assert_eq!(rds.pi_votes.len(), 1, "{:?}", rds.pi_votes);
    assert_eq!(
        rds.ps(),
        Some("HACKRIFF"),
        "[{SIGNAL_062}] {:?}",
        rds.ps_frames
    );
    assert_eq!(rds.ps_frames.len(), 1);
    assert_eq!(rds.pty, Some(10));
    assert_eq!(rds.tp, Some(true));
    assert!(
        rds.block_error_rate.unwrap() < 0.01,
        "{:?}",
        rds.block_error_rate
    );

    // Audio: mono = (L + R)/2 carries both tones.
    let audio = s.audio.as_ref().unwrap();
    assert_eq!(audio.sample_rate_hz, 48_000.0);
    let tail = &audio.samples[audio.samples.len() / 4..];
    for tone in [1000.0, 400.0] {
        let f = tone_frequency(tail, 48_000.0, tone - 50.0, tone + 50.0);
        let v = f
            .value()
            .unwrap_or_else(|| panic!("[{SIGNAL_062}] tone {tone}: {f:?}"));
        assert!((v - tone).abs() < 1.0, "[{SIGNAL_062}] tone {v} vs {tone}");
    }
    assert!(audio.to_wav_bytes().len() == 44 + 2 * audio.samples.len());

    let mut repo = Repository::open_in_memory().unwrap();
    let w = write_session(&mut repo, &s, &RecordContext::default()).unwrap();
    let id = DecodedIdentity {
        scheme: IdentityScheme::RdsPi,
        value: "C0DE".into(),
    };
    let e = repo.emitter_by_identity(&id).unwrap().expect("emitter");
    assert_eq!(Some(e.id), w.emitter_id);
    assert!(w.emitter_created);
    let labels = repo
        .annotations_for(&AnnotationTarget::Emitter(e.id))
        .unwrap();
    assert_eq!(labels.len(), 1);
    assert_eq!(labels[0].value, "HACKRIFF");
    assert_eq!(e.current_classification().unwrap().family, "wfm");
    let decodes = repo.decodes_for_identity(&id).unwrap();
    assert!(decodes.len() >= 2, "PI row plus PS frames");
    assert!(decodes.iter().all(|d| d.metadata["pi"] == "C0DE"));
    // Replaying the same station merges into the same emitter.
    let w2 = write_session(&mut repo, &s, &RecordContext::default()).unwrap();
    assert_eq!(w2.emitter_id, w.emitter_id);
    assert!(!w2.emitter_created);
}

#[test]
fn signal_062_synthetic_fm_without_rds_gives_no_pi() {
    let _ = synth_or_skip!(
        SynthRequest::new("fm_broadcast_rds")
            .seed(62)
            .param("duration_s", 0.1)
    );
    let Some((iq, center, bw)) = synth(0.0) else {
        return;
    };
    let s = run(&iq, center, bw);
    let rds = s.rds().unwrap();
    eprintln!("[{SIGNAL_062}] no-RDS: mode {:?} RDS {rds:?}", s.mode.mode);
    assert_eq!(s.mode.mode, AnalogMode::Wfm);
    assert!(s.wfm.as_ref().unwrap().pilot.present);
    assert!(rds.pi.is_none(), "[{SIGNAL_062}] false PI {:?}", rds.pi);
    assert!(rds.frame_log.is_empty());
    let mut repo = Repository::open_in_memory().unwrap();
    let w = write_session(&mut repo, &s, &RecordContext::default()).unwrap();
    assert!(w.decode_ids.is_empty() && w.emitter_id.is_none() && w.label.is_none());
}

#[test]
fn signal_062_synthetic_corrupt_burst_gives_no_wrong_pi() {
    let _ = synth_or_skip!(
        SynthRequest::new("fm_broadcast_rds")
            .seed(62)
            .param("duration_s", 0.1)
    );
    let Some((mut iq, center, bw)) = synth(2000.0) else {
        return;
    };
    let mut rng = Rng::new(99);
    let a = (1.0 * FS) as usize;
    for s in &mut iq[a..a + (0.06 * FS) as usize] {
        let v = rng.next_u64();
        *s = Complex::new(v as i8, (v >> 8) as i8);
    }
    let s = run(&iq, center, bw);
    let rds = s.rds().unwrap();
    eprintln!("[{SIGNAL_062}] burst: RDS {rds:?}");
    assert_eq!(s.mode.mode, AnalogMode::Wfm);
    assert_eq!(rds.pi.expect("PI").hex(), "C0DE");
    assert_eq!(
        rds.pi_votes.len(),
        1,
        "[{SIGNAL_062}] wrong PI votes {:?}",
        rds.pi_votes
    );
    assert!(
        rds.frame_log
            .iter()
            .all(|f| f.text == "HACKRIFF" && f.pi == 0xC0DE)
    );
    assert!(
        rds.blocks_ok < rds.blocks_total,
        "[{SIGNAL_062}] the burst must cost blocks"
    );
}
