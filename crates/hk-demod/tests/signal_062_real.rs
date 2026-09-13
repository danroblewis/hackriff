//! SIGNAL-062 (RDS/RBDS), recorded: `fm_100p8M_2p4M_l32g30a1_t1p5_5s` (HackRF One, 2.4 Msps,
//! centre 100.8 MHz, station 101.3 MHz at +500 kHz, 5 s).
//!
//! The receiver is given only the channel box (a detection: centre and 200 kHz). It must
//! auto-select WFM, lock the pilot within 1 Hz of 18 999.87 Hz, decode PI 1694 and at least one
//! PS frame of the scrolling set, report block/group error rates consistent with the reference
//! decoder's 6.7 % in this window, and write the Emitter (identity rds-pi 1694 plus a label).

mod common;

use common::*;
use hk_demod::{AnalogMode, AnalogReceiver, RecordContext, write_session};
use hk_e2e::Fixture;
use hk_estimate::SnippetRequest;
use hk_model::{AnnotationTarget, DecodedIdentity, IdentityScheme, Repository};

const NAME: &str = "fm_100p8M_2p4M_l32g30a1_t1p5_5s";

#[test]
fn signal_062_real_fm_auto_wfm_pilot_pi_ps_and_emitter() {
    let (meta, data) = fixture_or_skip!(NAME);
    let fx = Fixture::load(&meta).unwrap();
    let prov = meta_provenance(&meta);
    let station = fx.of_kind("wfm-broadcast")[0];
    let pilot_truth = station.expect_f64("/pilot/frequency_hz");
    let pi_truth = station.str("/rds/pi_hex").unwrap().to_owned();
    let bler_truth = station.expect_f64("/rds/decode_window/block_error_rate");
    let known_ps: Vec<String> = station
        .get("/rds/ps_frames")
        .and_then(|v| v.as_object())
        .unwrap()
        .keys()
        .cloned()
        .collect();
    let iq = read_ci8(&data);
    let request = SnippetRequest {
        start_index: 0,
        end_index: iq.len() as u64,
        center_offset_hz: station.center_hz() - prov.tune.center_hz,
        bandwidth_hz: 200e3,
    };

    let t = std::time::Instant::now();
    let mut rx = AnalogReceiver::default();
    let session = rx.run(info(0, &prov), &iq, &request).unwrap();
    let elapsed = t.elapsed().as_secs_f64();
    let wfm = session.wfm.as_ref().expect("WFM chain ran");
    let rds = session.rds().expect("RDS enabled");
    eprintln!(
        "[{SIGNAL_062}] mode {:?} conf {:.2} features {:?} ({elapsed:.1} s)",
        session.mode.mode, session.mode.confidence, session.mode.features
    );
    eprintln!("[{SIGNAL_062}] pilot {:?}", wfm.pilot);
    eprintln!(
        "[{SIGNAL_062}] RDS PI {:?} votes {:?} PS frames {:?} PTY {:?} TP {:?} groups {:?} \
         blocks {}/{} BLER {:?} (reference {bler_truth:.3}) group ER {:?} sync {} slips {} \
         timing contrast {:?}, deviation {:?}, carrier offset {:.0} Hz",
        rds.pi,
        rds.pi_votes,
        rds.ps_frames,
        rds.pty,
        rds.tp,
        rds.group_types,
        rds.blocks_ok,
        rds.blocks_total,
        rds.block_error_rate,
        rds.group_error_rate,
        rds.sync_acquisitions,
        rds.bit_slips,
        wfm.rds_timing_contrast,
        wfm.peak_deviation_hz,
        wfm.carrier_offset_hz
    );

    assert_eq!(
        session.mode.mode,
        AnalogMode::Wfm,
        "[{SIGNAL_062}] {:?}",
        session.mode
    );
    assert!(session.mode.features.pilot.is_some_and(|p| p.found));
    assert!(wfm.pilot.present && wfm.stereo);
    let pilot = wfm.pilot.frequency_hz.unwrap();
    assert!(
        (pilot - pilot_truth).abs() < 1.0,
        "[{SIGNAL_062}] pilot {pilot:.3} Hz vs {pilot_truth:.3}"
    );
    let pi = rds.pi.expect("PI decoded");
    assert_eq!(pi.hex(), pi_truth, "[{SIGNAL_062}] PI");
    assert_eq!(
        rds.pi_votes.len(),
        1,
        "[{SIGNAL_062}] no other PI: {:?}",
        rds.pi_votes
    );
    assert!(
        rds.ps_frames.iter().any(|(t, _)| known_ps.contains(t)),
        "[{SIGNAL_062}] PS frames {:?} not in {known_ps:?}",
        rds.ps_frames
    );
    assert!(
        rds.frame_log.iter().all(|f| known_ps.contains(&f.text)),
        "[{SIGNAL_062}] unexpected PS frame {:?}",
        rds.frame_log
    );
    let bler = rds.block_error_rate.expect("block error rate");
    assert!(
        bler <= 0.15 && rds.blocks_total >= 150,
        "[{SIGNAL_062}] BLER {bler:.3} over {} blocks (reference {bler_truth:.3})",
        rds.blocks_total
    );
    assert!(rds.group_error_rate.is_some());

    let mut repo = Repository::open_in_memory().unwrap();
    let written = write_session(&mut repo, &session, &RecordContext::default()).unwrap();
    let identity = DecodedIdentity {
        scheme: IdentityScheme::RdsPi,
        value: "1694".into(),
    };
    let emitter = repo
        .emitter_by_identity(&identity)
        .unwrap()
        .expect("emitter with rds-pi 1694");
    assert_eq!(Some(emitter.id), written.emitter_id);
    let label = written.label.clone().expect("label");
    assert!(
        known_ps.iter().any(|p| p.trim() == label),
        "label {label:?}"
    );
    let labels = repo
        .annotations_for(&AnnotationTarget::Emitter(emitter.id))
        .unwrap();
    assert_eq!(labels.len(), 1);
    assert_eq!(labels[0].value, label);
    let decodes = repo.decodes_for_identity(&identity).unwrap();
    assert_eq!(decodes.len(), written.decode_ids.len());
    assert!(
        decodes
            .iter()
            .all(|d| d.content_class == hk_model::ContentClass::Unrestricted)
    );
    let demod = repo.demodulation(written.demodulation_id).unwrap();
    assert_eq!(demod.mode, "wfm");
    assert_eq!(demod.emitter_ref, Some(emitter.id));
    eprintln!(
        "[{SIGNAL_062}] emitter {} label {label:?}, {} decodes",
        emitter.id,
        decodes.len()
    );
}
