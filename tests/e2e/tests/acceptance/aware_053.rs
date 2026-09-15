//! AWARE-053 (T4): known-signal priors separate known from unexpected. The tests are **blind** (T-039):
//! each replay goes through `blind::blind_replay` with the fixture's truth stripped. The test
//! then matches everything the pipeline produced (all detections, all inventory emitters) against
//! the truth only it holds. No test classifies an emitter or looks a frequency up.
//!
//! - **Known allocation.** The FM fixture's strong station is detected, and its emitters rank "FM
//!   broadcast" first in their explanations. The status is `known`, with the 47 CFR 2.106 FM row
//!   as `prior_ref`.
//! - **Off raster.** A synthesised copy (the IQ shifted +150 kHz, onto 101.45 MHz) is still
//!   detected, with FM broadcast in the top-k flagged `off-raster`.
//! - **Shape only (T-054).** The same IQ relabelled onto 162 MHz (maritime VHF) is
//!   `metadata-only`, so no content chain runs and only the track's occupancy (shape) evidence
//!   exists. FM broadcast is ranked as a `shape-only`, `off-allocation` suggestion, and the status
//!   stays `unknown` with no prior_ref: a wide carrier is not "FM broadcast, unexpected here" on
//!   shape alone.
//! - **Off allocation.** The same IQ relabelled 19.2 MHz up (aeronautical VHF comm), with the user
//!   vouching the recording `unrestricted` and adding the airband to the analog chain's range
//!   (plan configuration, not truth), so the analog chain may demodulate it. The WFM
//!   demodulation ranks FM broadcast first, `off-allocation`, with status `unexpected-here` and
//!   prior_ref `aviation-vhf-comm`. Aviation voice is the allocation-only alternative.
//!   *T-047 audit (kept):* the T-054 relaxation is user configuration a real operator could set
//!   (a content class for their own recording, and a 19 MHz band the analog chain may demodulate
//!   in). Neither names the station, its frequency, its mode or its label: the station is still
//!   found by blind detection, mode selection still has to pick WFM, and the truth list is read
//!   only after the run. It stays until an auto analog chain on any confirmed emitter (T-054
//!   follow-up) removes the need for it.

use hk_e2e::TruthItem;
use hk_e2e::blind::matching;
use hk_model::{
    EmitterId, FreqRange, InventoryQuery, KnownStatus, KnownStatusChange, Region, StatusAuthor,
};
use hk_pipeline::family::MIN_CONFIDENCE;
use hk_pipeline::{Explanation, explanations};

use crate::blind::{BlindRun, BlindSource, assert_truth_found, blind_replay, private_truth};
use crate::common::*;
use crate::signal_062::FM_FIXTURE;

const AWARE_053: &str = "AWARE-053";
const FM_ROW: &str = "us-47cfr2106-compact:fm-broadcast";
const AIRBAND_ROW: &str = "us-47cfr2106-compact:aviation-vhf-comm";
/// A produced extent matches the truth when its centre is this close and the extents overlap.
const CENTER_TOL_HZ: f64 = 100e3;

/// The broadcast station in the fixture's private truth.
fn station(fx: &hk_e2e::Fixture) -> TruthItem {
    fx.of_kind("wfm-broadcast")
        .first()
        .map(|t| (*t).clone())
        .unwrap_or_else(|| panic!("[{AWARE_053}] fixture truth has no wfm-broadcast station"))
}

#[derive(Debug)]
struct Seen {
    id: EmitterId,
    status: KnownStatus,
    last: KnownStatusChange,
    explanations: Vec<Explanation>,
}

/// Matched detections and emitters of a blind run against `truth` moved by `shift_hz`.
fn matched(run: &BlindRun, truth: &TruthItem, shift_hz: f64, tag: &str) -> (usize, Vec<Seen>) {
    let repo = repo(&run.dir.0);
    let detections = repo
        .detections_in_region(&Region::new(FreqRange::new(0.0, 7.0e9), ever()))
        .unwrap();
    let dets = matching(
        truth,
        shift_hz,
        &detections,
        |d| (d.f_center_hz, d.obw_hz),
        CENTER_TOL_HZ,
    )
    .len();
    let all = inventory(&repo, InventoryQuery::default());
    let hits = matching(
        truth,
        shift_hz,
        &all,
        |e| (e.emitter.f_center_hz, e.emitter.bandwidth_hz),
        CENTER_TOL_HZ,
    );
    eprintln!(
        "[{AWARE_053}] {tag}: {dets} of {} detections and {} of {} emitters match the private truth",
        detections.len(),
        hits.len(),
        all.len()
    );
    let seen = hits
        .into_iter()
        .map(|e| {
            let x = explanations(&repo, e.emitter.id).unwrap();
            let last = repo
                .known_status_history(e.emitter.id)
                .unwrap()
                .pop()
                .unwrap();
            eprintln!(
                "[{AWARE_053}] {tag}: emitter {:.4} MHz bw {:.0} Hz family {:?} status {:?} ({:?}: {}) top-k {:?}",
                e.emitter.f_center_hz / 1e6,
                e.emitter.bandwidth_hz,
                e.family,
                e.emitter.known_status,
                last.author,
                last.reason,
                x.iter()
                    .map(|x| (x.label.as_str(), (x.score * 100.0).round() / 100.0, &x.flags))
                    .collect::<Vec<_>>()
            );
            Seen {
                id: e.emitter.id,
                status: e.emitter.known_status,
                last,
                explanations: x,
            }
        })
        .collect();
    (dets, seen)
}

fn has_fm(s: &Seen) -> bool {
    s.explanations.iter().any(|x| x.service == "fm-broadcast")
}

fn evidence_backed(x: &Explanation) -> bool {
    x.evidence_confidence >= MIN_CONFIDENCE
}

/// Backed by demodulator/decoder/classifier evidence, not shape alone: may set a status.
fn status_backed(x: &Explanation) -> bool {
    x.status_evidence_confidence >= MIN_CONFIDENCE
}

#[test]
fn aware_053_api_serves_the_family_map_author() {
    assert_eq!(
        hk_api::query::EXPLANATIONS_AUTHOR_REF,
        hk_pipeline::family::FAMILY_MAP_VERSION,
        "[{AWARE_053}] /api/inventory must serve the family map's explanations"
    );
}

#[test]
fn aware_053_blind_fm_station_ranks_fm_broadcast_first_and_is_known() {
    let Some((meta, fx)) = private_truth(FM_FIXTURE) else {
        return;
    };
    let truth = station(&fx);
    let run = blind_replay(&meta, "a053k", BlindSource::default());
    let (dets, seen) = matched(&run, &truth, 0.0, "FM station");
    assert_truth_found(AWARE_053, &run.dir.0, &fx, 0.0, true);
    assert!(dets > 0, "[{AWARE_053}] the station was not detected blind");
    assert!(!seen.is_empty(), "[{AWARE_053}] no emitter at the station");
    for s in &seen {
        assert!(
            has_fm(s),
            "[{AWARE_053}] FM broadcast not in the top-k: {s:?}"
        );
    }
    let best = seen
        .iter()
        .find(|s| {
            s.explanations.first().is_some_and(|x| {
                x.service == "fm-broadcast" && status_backed(x) && !x.has_flag("off-raster")
            })
        })
        .unwrap_or_else(|| {
            panic!("[{AWARE_053}] no emitter ranks evidence-backed, on-raster FM broadcast first")
        });
    assert_eq!(best.status, KnownStatus::Known, "[{AWARE_053}] {best:?}");
    assert_eq!(best.last.author, StatusAuthor::Prior);
    assert_eq!(best.last.prior_ref.as_deref(), Some(FM_ROW));
    assert_eq!(best.explanations[0].label, "FM broadcast");
    let row = run
        .api_rows
        .iter()
        .find(|r| r["id"] == best.id.to_string())
        .unwrap_or_else(|| panic!("[{AWARE_053}] emitter missing from /api/inventory"));
    assert_eq!(
        row["explanations"][0]["service"], "fm-broadcast",
        "[{AWARE_053}] /api/inventory serves the ranked explanations"
    );
}

#[test]
fn aware_053_blind_station_shifted_150_khz_keeps_fm_broadcast_flagged_off_raster() {
    const SHIFT_HZ: f64 = 150e3;
    let Some((meta, fx)) = private_truth(FM_FIXTURE) else {
        return;
    };
    let truth = station(&fx);
    let run = blind_replay(
        &meta,
        "a053o",
        BlindSource {
            iq_shift_hz: SHIFT_HZ,
            ..BlindSource::default()
        },
    );
    let (dets, seen) = matched(&run, &truth, SHIFT_HZ, "FM station +150 kHz");
    assert_truth_found(AWARE_053, &run.dir.0, &fx, SHIFT_HZ, true);
    assert!(
        dets > 0,
        "[{AWARE_053}] the shifted station was not detected"
    );
    assert!(
        !seen.is_empty(),
        "[{AWARE_053}] no emitter at the shifted station"
    );
    for s in &seen {
        assert!(
            has_fm(s),
            "[{AWARE_053}] FM broadcast not in the top-k: {s:?}"
        );
    }
    let off: Vec<&Explanation> = seen
        .iter()
        .flat_map(|s| &s.explanations)
        .filter(|x| x.service == "fm-broadcast" && x.has_flag("off-raster"))
        .collect();
    assert!(
        off.iter().any(|x| evidence_backed(x)),
        "[{AWARE_053}] no evidence-backed FM broadcast explanation flagged off-raster: {off:?}"
    );
}

/// T-054: a wideband FM-like carrier at 162 MHz with shape evidence only stays `unknown`; FM
/// broadcast is only a shape-only suggestion.
#[test]
fn aware_053_blind_wide_carrier_at_162_mhz_stays_unknown_with_a_shape_only_suggestion() {
    let Some((meta, fx)) = private_truth(FM_FIXTURE) else {
        return;
    };
    let truth = station(&fx);
    let shift_hz = 162.0e6 - 101.3e6;
    let run = blind_replay(
        &meta,
        "a053s",
        BlindSource {
            relabel_hz: shift_hz,
            ..BlindSource::default()
        },
    );
    assert_eq!(
        run.summary.source_class, "metadata-only",
        "[{AWARE_053}] 162 MHz is no unrestricted band prior"
    );
    let (dets, seen) = matched(&run, &truth, shift_hz, "162 MHz carrier");
    assert_truth_found(AWARE_053, &run.dir.0, &fx, shift_hz, true);
    assert!(dets > 0, "[{AWARE_053}] the carrier was not detected");
    assert!(!seen.is_empty(), "[{AWARE_053}] no emitter at the carrier");
    let suggested: Vec<&Explanation> = seen
        .iter()
        .flat_map(|s| &s.explanations)
        .filter(|x| x.service == "fm-broadcast")
        .collect();
    assert!(
        !suggested.is_empty(),
        "[{AWARE_053}] FM broadcast should still be suggested from the shape"
    );
    for x in &suggested {
        assert!(
            x.has_flag("shape-only") && !status_backed(x),
            "[{AWARE_053}] {x:?}"
        );
    }
    for s in &seen {
        assert_eq!(s.status, KnownStatus::Unknown, "[{AWARE_053}] {s:?}");
        assert_eq!(s.last.prior_ref, None, "[{AWARE_053}] {s:?}");
    }
}

#[test]
fn aware_053_blind_off_allocation_station_is_unexpected_here_with_prior_ref() {
    const SHIFT_HZ: f64 = 19.2e6;
    let Some((meta, fx)) = private_truth(FM_FIXTURE) else {
        return;
    };
    let truth = station(&fx);
    let run = blind_replay(
        &meta,
        "a053u",
        BlindSource {
            relabel_hz: SHIFT_HZ,
            vouched_class: Some("unrestricted"),
            demod_freq_hz: Some([118.0e6, 137.0e6]),
            ..BlindSource::default()
        },
    );
    assert_eq!(
        run.summary.source_class, "unrestricted",
        "[{AWARE_053}] the user vouched the recording"
    );
    let (dets, seen) = matched(&run, &truth, SHIFT_HZ, "aeronautical band");
    assert_truth_found(AWARE_053, &run.dir.0, &fx, SHIFT_HZ, true);
    assert!(
        dets > 0,
        "[{AWARE_053}] the relabelled station was not detected"
    );
    let flagged: Vec<&Seen> = seen
        .iter()
        .filter(|s| s.status == KnownStatus::UnexpectedHere)
        .collect();
    assert!(
        !flagged.is_empty(),
        "[{AWARE_053}] no emitter at the relabelled station reached `unexpected-here`"
    );
    for s in &flagged {
        assert_eq!(s.last.author, StatusAuthor::Prior, "[{AWARE_053}]");
        assert_eq!(
            s.last.prior_ref.as_deref(),
            Some(AIRBAND_ROW),
            "[{AWARE_053}]"
        );
        let top = &s.explanations[0];
        assert_eq!(top.service, "fm-broadcast", "[{AWARE_053}] {s:?}");
        assert!(top.has_flag("off-allocation") && status_backed(top));
        assert!(
            s.explanations.iter().any(|x| x.service == "aviation-voice"),
            "[{AWARE_053}] the aviation allocation is a ranked alternative"
        );
    }
    let listed = inventory(
        &repo(&run.dir.0),
        InventoryQuery {
            status: vec![KnownStatus::UnexpectedHere],
            ..InventoryQuery::default()
        },
    );
    assert!(
        flagged
            .iter()
            .all(|f| listed.iter().any(|e| e.emitter.id == f.id)),
        "[{AWARE_053}] query_inventory status filter"
    );
}
