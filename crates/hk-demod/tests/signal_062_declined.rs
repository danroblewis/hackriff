//! T-416, SIGNAL-062: a mono broadcast station is recognised without a pilot, and a probe that
//! declines still records what it measured.
//!
//! Two halves of one defect, found on a real 101.3 MHz station whose probe read
//! `mode unknown (0.75), pilot Some(false) -> stop` and wrote nothing at all.
//!
//! - **The pilot was evidence in name and a requirement in effect.** The WFM candidate was raised
//!   on width alone, but without a pilot the only confidence that cleared the decide floor needed
//!   OBW99 ≥ 150 kHz — so every mono station between the 120 kHz candidate edge and 150 kHz was
//!   undecidable however constant its envelope, and the real one missed by 110 Hz. A mono
//!   broadcaster has no pilot by definition, so that is every mono broadcaster in the world at the
//!   quiet end of its programme.
//! - **The refusal left no record.** A chain that declines used to return having written nothing,
//!   which makes "we measured this window and it is not ours" indistinguishable from "nothing ever
//!   looked here" — the distinction `Coverage::Unobserved` draws against observed-and-quiet, and
//!   `BiasTee::Unknown` against off.
//!
//! The blind confusion matrix in `signal_062_mode_sweep.rs` carries the control that this did not
//! become "anything wide and constant-envelope is FM": a 4-CPFSK data link of the same width and
//! the same constant envelope must still not be called WFM, at any SNR.

mod common;

use common::*;
use hk_demod::{AnalogMode, AnalogReceiver, AnalogSession, RecordContext, write_declined};
use hk_estimate::SnippetRequest;
use hk_model::{Fingerprint, LinkTarget, Repository, Sighting, TimeRange, Timestamp};
use num_complex::Complex32;

const FS: f64 = 500e3;

fn select(iq: &[Complex32], offset_hz: f64, bandwidth_hz: f64) -> AnalogSession {
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

/// A mono broadcast station: 75 kHz peak deviation, programme inside the 15 kHz main channel, and
/// **no 19 kHz pilot and no subcarrier of any kind** — there is nothing here but audio.
///
/// The 4 kHz programme tone is chosen so that none of the harmonics a limiter leaves in a
/// discriminator's output (16 kHz, 20 kHz) lands in the pilot search window, so the pilot test is
/// answering the question asked of it rather than an artefact of the scene.
fn mono_station() -> Vec<Complex32> {
    nbfm_tone(FS, 1.0, 40e3, 0.1, 75e3, 4e3)
}

#[test]
fn t416_a_mono_broadcast_station_is_wfm_although_it_has_no_pilot() {
    let s = select(&mono_station(), 40e3, 200e3);
    eprintln!(
        "[{SIGNAL_062}] mono WFM: {:?} ({:.2}) obw {:?} pilot {:?} baseband {:?}",
        s.mode.mode,
        s.mode.confidence,
        s.mode.features.obw99_hz,
        s.mode.features.pilot.map(|p| p.found),
        s.mode.features.baseband_fraction,
    );
    assert_eq!(
        s.mode.mode,
        AnalogMode::Wfm,
        "[{SIGNAL_062}] a mono broadcaster has no pilot; needing one makes every mono station in \
         the world unrecognisable. Decision: {:?}",
        s.mode
    );
    assert!(
        !s.mode.features.pilot.is_some_and(|p| p.found),
        "[{SIGNAL_062}] this scene carries no 19 kHz pilot, so finding one would mean the pilot \
         test is answering something other than the question"
    );
    // The positive evidence that stood in for the pilot: the modulating signal is a programme
    // baseband, not a symbol stream.
    let fraction = s
        .mode
        .features
        .baseband_fraction
        .expect("[SIGNAL-062] the baseband fit is measured whenever the pilot check runs");
    assert!(
        fraction >= 0.95,
        "[{SIGNAL_062}] a mono station's whole multiplex is its programme channel; got {fraction}"
    );
    // And it was recognised on that, not on width: this station is narrower than the old width bar
    // (150 kHz) that made the rule need a pilot in practice.
    let obw = s.mode.features.obw99_hz.expect("OBW99 measured");
    assert!(
        obw >= 120e3,
        "[{SIGNAL_062}] wide enough to be a WFM candidate at all: {obw} Hz"
    );
}

/// The emitter a refusal is filed against. Made by something else — a refusal never conjures one.
fn existing_emitter(
    repo: &mut Repository,
    center_hz: f64,
    bandwidth_hz: f64,
) -> hk_model::EmitterId {
    let t = Timestamp::from_unix_nanos(1_700_000_000_000_000_000);
    let sighting = Sighting {
        source: LinkTarget::Track(hk_model::TrackId::new()),
        seen: TimeRange::new(t, t),
        count: 1,
        f_center_hz: center_hz,
        bandwidth_hz,
        fingerprint: Some(Fingerprint::new(center_hz, bandwidth_hz)),
        identity: None,
        context: None,
        classification: None,
        tags: Vec::new(),
    };
    repo.record_sighting(&sighting, None).unwrap().emitter_id
}

#[test]
fn t416_a_probe_that_declines_still_records_what_it_measured() {
    // A window the chain would decline: noise, where the selector reaches no mode at all.
    let s = select(&noise_only(FS, 1.0), 50e3, 12.5e3);
    assert_eq!(s.mode.mode, AnalogMode::Unknown, "{:?}", s.mode);

    let mut repo = Repository::open_in_memory().unwrap();
    let emitter = existing_emitter(&mut repo, 100.05e6, 12.5e3);
    let before = repo.classification_history(emitter).unwrap().len();

    let ctx = RecordContext {
        emitter_hint: Some(emitter),
        ..RecordContext::default()
    };
    let id = write_declined(&mut repo, &s, &ctx).unwrap();

    // **The measurement is there**, and it is findable from the thing it is about — a row nothing
    // links to is a row nobody reads.
    let found = repo
        .latest_linked_demodulation_for_emitter(emitter)
        .unwrap()
        .expect("[T-416] a refusal that leaves no record is indistinguishable from never looking");
    assert_eq!(found.id, id);
    assert_eq!(found.mode, "unknown");
    assert_eq!(found.time, s.time_range(), "the window it measured over");
    assert_eq!(found.params, s.estimated_params(), "what it measured");
    assert!(
        found.lock_quality.is_none() && found.params.pilot_hz.is_none(),
        "[T-416] the absent lock is part of the record, not an omission from it: {found:?}"
    );

    // **And it is not a promotion.** Nothing here may make, name, or advance an inventory entry:
    // the whole point of the row is that the system declined to say what this is.
    assert_eq!(
        repo.classification_history(emitter).unwrap().len(),
        before,
        "[T-416] a declined measurement appends no classification"
    );
    assert!(
        repo.decode_evidence_for_emitter(emitter)
            .unwrap()
            .is_empty(),
        "[T-416] a declined measurement writes no decodes"
    );
    assert_eq!(
        repo.emitter(emitter).unwrap().identity,
        hk_model::Identity::Unknown,
        "[T-416] and nothing identity-bearing"
    );
}

#[test]
fn t416_a_declined_measurement_with_no_entry_to_file_it_against_is_still_written() {
    // The common case in a live run: a chain attaches on the *tracker's* confirmation and probes
    // about a second into an emission, which is routinely before the inventory has offered that
    // track a row at all. Writing the row unattached and letting the inventory adopt it is the
    // point; dropping it because there is nowhere to file it yet is the defect.
    let s = select(&noise_only(FS, 1.0), 50e3, 12.5e3);
    let mut repo = Repository::open_in_memory().unwrap();
    let id = write_declined(&mut repo, &s, &RecordContext::default()).unwrap();
    let row = repo.demodulation(id).unwrap();
    assert_eq!(row.mode, "unknown");
    assert!(row.emitter_ref.is_none());
}
