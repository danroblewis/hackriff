//! T-948: **the admission rule at the inventory seam** — what
//! [`hk_detect::track::inventory::track_sighting`] refuses, and what it must not.
//!
//! Two lines in one known-floor scene, both narrow, both on for the whole scene, one at the tuned
//! centre and one 200 bins above it. The detector's rule 2 calls the first a DC spike
//! (`SpurReason::Dc`) and says nothing about the second. The rule:
//!
//! | track | admitted? | why |
//! |---|---|---|
//! | every member attributed to the receiver, CW-narrow | **no** | the receiver made it; the notch it sits in is *excluded from analysis* (T-595) |
//! | the same track after a clean twin refuted its DC flag (T-174) | **yes** | the flag was a hypothesis about one tuning and it lost |
//! | a line nothing attributed to the receiver | **yes** | narrow and always-on is what a CW beacon looks like too |
//! | a centre no emitter can have (−0.598 MHz, the explorer's) | **no** | below DC is the baseband's image of itself, not a transmitter |
//!
//! The DC row still exists as detections with their reason — refusing an inventory *entry* is not
//! dropping the measurement. The whole-pipeline half of T-948 is
//! `hk-pipeline/tests/dc_point_never_a_candidate.rs`.

mod common;

use common::*;
use hk_detect::track::inventory::{ARTIFACT_MAX_BINS, receiver_artifact, track_sighting};
use hk_detect::{DetectorConfig, TrackEvent, TrackSummary, Tracker, TrackerConfig};
use hk_model::detection::SpurReason;
use hk_model::{SurveyId, TrackId};

const FS: f64 = 2e6;
const CENTER: f64 = 162.2e6;
const BINS: usize = 1024;
/// Bins above the centre of the honest line: well outside the DC tolerance (15 kHz = 7.7 bins).
const LINE_BINS: usize = 200;
const FRAMES: usize = 40;
const SNR_DB: f64 = 25.0;

/// Runs the scene and returns the closed summaries. With `refute`, every DC-flagged member is
/// refuted as the tracker takes it, exactly as the reader does when a clean twin from another
/// tuning arrives (T-174, `hk_pipeline::detect`).
fn closed(refute: bool) -> Vec<TrackSummary> {
    let prov = provenance(CENTER, FS, 24.0);
    let mut scene = Scene::new(
        DetectorConfig::new(SurveyId::new()),
        GammaFrames::new(BINS, 10, prov, 0x51_9e),
    );
    let mut tr = Tracker::new(TrackerConfig::default());
    let mut events: Vec<TrackEvent> = Vec::new();
    let mut profile = flat(BINS);
    add_line(&mut profile, BINS / 2, 1, SNR_DB);
    add_line(&mut profile, BINS / 2 + LINE_BINS, 1, SNR_DB);
    for _ in 0..FRAMES {
        scene.step(&profile);
        for r in scene.out.detections.drain(..) {
            tr.push_detection(&r, &mut |e| events.push(e));
            if refute && r.detection.flags.spur_reason == Some(SpurReason::Dc) {
                tr.refute_dc(r.detection.id);
            }
        }
        scene.out.confirmations.clear();
        scene.out.evaluations.clear();
        tr.observe_frame(&scene.det, &scene.frame, &mut |e| events.push(e));
    }
    scene.finish();
    for r in scene.out.detections.drain(..) {
        tr.push_detection(&r, &mut |e| events.push(e));
        if refute && r.detection.flags.spur_reason == Some(SpurReason::Dc) {
            tr.refute_dc(r.detection.id);
        }
    }
    tr.finish(&mut |e| events.push(e));
    events
        .into_iter()
        .filter_map(|e| match e {
            TrackEvent::Closed(s) => Some(s),
            _ => None,
        })
        .collect()
}

/// The closed summary nearest `f_hz`.
fn near(summaries: &[TrackSummary], f_hz: f64) -> TrackSummary {
    let s = summaries
        .iter()
        .min_by(|a, b| {
            let d = |s: &TrackSummary| (s.track.f_center_hz - f_hz).abs();
            d(a).total_cmp(&d(b))
        })
        .unwrap_or_else(|| panic!("no closed track at all (wanted one near {f_hz:.0} Hz)"));
    assert!(
        (s.track.f_center_hz - f_hz).abs() < 30e3,
        "nearest track to {:.4} MHz is {:.4} MHz",
        f_hz / 1e6,
        s.track.f_center_hz / 1e6
    );
    s.clone()
}

#[test]
fn t948_a_track_that_is_only_the_receivers_dc_spike_gets_no_inventory_entry() {
    let summaries = closed(false);
    let dc = near(&summaries, CENTER);
    assert_eq!(
        dc.artifact_detections, dc.track.detection_count,
        "every member of the DC track is the receiver's own: {dc:?}"
    );
    assert_eq!(dc.artifact_reason, Some(SpurReason::Dc));
    assert_eq!(receiver_artifact(&dc), Some(SpurReason::Dc));
    assert!(
        track_sighting(&dc).is_none(),
        "the DC spike must not become an inventory entry: {dc:?}"
    );
}

#[test]
fn t948_a_steady_line_beside_the_notch_is_still_admitted() {
    let summaries = closed(false);
    let line = near(&summaries, CENTER + LINE_BINS as f64 * FS / BINS as f64);
    assert_eq!(
        line.artifact_detections, 0,
        "nothing attributes the honest line to the receiver: {line:?}"
    );
    assert_eq!(receiver_artifact(&line), None);
    assert!(
        track_sighting(&line).is_some(),
        "a narrow, always-on emission is exactly what a CW beacon looks like: {line:?}"
    );
}

#[test]
fn t948_a_refuted_dc_flag_admits_the_track_again() {
    let summaries = closed(true);
    let dc = near(&summaries, CENTER);
    assert!(
        dc.track.detection_count > 0,
        "the scene produced members at the centre: {dc:?}"
    );
    assert_eq!(
        dc.artifact_detections, 0,
        "T-174: a refuted DC flag stops counting as the receiver's own line: {dc:?}"
    );
    assert_eq!(receiver_artifact(&dc), None);
    assert!(
        track_sighting(&dc).is_some(),
        "an emission the receiver was merely tuned on top of is admitted: {dc:?}"
    );
}

#[test]
fn t948_a_modulated_emission_is_admitted_however_its_members_were_flagged() {
    let summaries = closed(false);
    let mut wide = near(&summaries, CENTER);
    // The same all-artifact evidence, at a width no windowed tone can reach: the rule stops.
    wide.track.bandwidth_hz = (ARTIFACT_MAX_BINS + 1.0) * wide.bin_hz;
    assert_eq!(receiver_artifact(&wide), None);
    assert!(
        track_sighting(&wide).is_some(),
        "wider than a window's own main lobe is a modulated emission, not a receiver line"
    );
}

#[test]
fn t948_an_impossible_centre_is_refused_at_admission() {
    let summaries = closed(false);
    let line = near(&summaries, CENTER + LINE_BINS as f64 * FS / BINS as f64);
    for f in [-0.598e6, 0.0, f64::NAN] {
        let mut bad = line.clone();
        bad.track.id = TrackId::new();
        bad.track.f_center_hz = f;
        assert!(
            track_sighting(&bad).is_none(),
            "no emitter transmits at {f} Hz, so nothing claiming to is one"
        );
    }
}
