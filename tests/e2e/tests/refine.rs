//! T-070 (SIGNAL-062): output-driven demodulator refinement, blind, through the mock SDR (the
//! T-047 harness: truth sealed, the device serves a stripped copy).
//!
//! - **Selections refine to the carrier.** On `fm_100p8M_2p4M_l32g30a1_t1p5_5s` looping through the
//!   mock device, Listen selections 60 kHz and 400 kHz wide, offset −100, −50, +50 and +100 kHz from
//!   the station, each refine to within 2 kHz of the private truth centre (the fixture truth's
//!   receiver-frame `center_hz`, pilot-derived: the 101.3 MHz channel under the capture's
//!   −6.77 ppm clock), with a 150–220 kHz bandwidth and RDS PI decoded in the refinement's
//!   validation. The stream header and status records report the refined tuning; a Listen on the
//!   station's inventory emitter stores it with provenance `refined by output analysis`, served by
//!   `/api/inventory`.
//! - **Off raster, not snapped.** The same recording IQ-shifted +150 kHz (a device retune cannot
//!   move a station: the mock keeps absolute frequencies) puts the station 50 kHz below the
//!   101.5 MHz raster channel. The analog chain attached to that raster channel refines to the
//!   station's true centre, stores it on the emitter, and the explanations flag FM broadcast
//!   `off-raster` from the refined centre.
//!
//! The selections are placed around the private truth, as a user drags near what they see; the
//! system under test never sees the truth.

#[path = "acceptance/common.rs"]
mod common;

// The harness serves the whole acceptance suite; this binary uses part of it.
#[path = "acceptance/blind.rs"]
#[allow(dead_code)]
mod blind;

use std::time::{Duration, Instant};

use blind::{BlindSource, blind_config, blind_live, private_truth, start};
use common::*;
use hk_e2e::blind::matching;
use hk_model::InventoryQuery;
use hk_pipeline::explanations;
use hk_pipeline::family::{CENTER_REFINED, ExplanationEvidence};
use hk_stream::{Declared, OpenRequest, OpenedStream, StreamOpener};
use serde_json::json;

const FM_FIXTURE: &str = "fm_100p8M_2p4M_l32g30a1_t1p5_5s";
const TAG: &str = "T-070";
const CENTER_TOL_HZ: f64 = 2_000.0;
const BANDWIDTH_HZ: (f64, f64) = (150e3, 220e3);

/// The private truth: receiver-frame centre and RDS PI of the station.
fn station(fx: &hk_e2e::Fixture) -> (hk_e2e::TruthItem, f64, String) {
    let s = fx.of_kind("wfm-broadcast")[0].clone();
    let centre = s.expect_f64("/center_hz");
    let pi = s.str("/rds/pi_hex").expect("truth PI").to_owned();
    (s, centre, pi)
}

/// Opens a listen stream, retrying while the previous one's slot is still being released (503) or
/// the run has not published its first tuning yet (409 right after start).
fn open(listen: &dyn StreamOpener, params: &[(&str, String)]) -> OpenedStream {
    let query: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect();
    let req = OpenRequest::from_query(&query, "t070-test");
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match listen.open(&req) {
            Ok(s) => return s,
            Err(e) if matches!(e.status, 409 | 503) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("[{TAG}] listen {params:?} refused: {e}"),
        }
    }
}

#[test]
fn listen_selections_around_the_station_refine_to_its_carrier() {
    let Some((meta, fx)) = private_truth(FM_FIXTURE) else {
        return;
    };
    if hardware_skip(TAG) {
        return;
    }
    let (truth, centre, pi) = station(&fx);
    let live = blind_live(&meta, "t070sel", BlindSource::default());
    let listen = live.handle.listen_service();

    let mut table = Vec::new();
    for width in [60e3, 400e3] {
        for offset in [-100e3, -50e3, 50e3, 100e3] {
            let fc = centre + offset;
            let t = Instant::now();
            let stream = open(
                listen.as_ref(),
                &[
                    ("f_lo", format!("{}", fc - 0.5 * width)),
                    ("f_hi", format!("{}", fc + 0.5 * width)),
                ],
            );
            let open_s = t.elapsed().as_secs_f64();
            let h = &stream.header;
            let audio = h.audio.as_ref().expect("audio profile");
            let r = audio
                .refinement
                .clone()
                .unwrap_or_else(|| panic!("[{TAG}] {offset:+} Hz / {width} Hz not refined: {h:?}"));
            let err = r.center_hz - centre;
            eprintln!(
                "[{TAG}] start {:+.0} kHz / {:.0} kHz -> centre error {err:+.0} Hz, bandwidth \
                 {:.1} kHz, PI {:?}, {} iterations, {} evaluations, refine {:.2} s (open {open_s:.2} \
                 s), mode {}, quality {:.1} dB-Hz",
                offset / 1e3,
                width / 1e3,
                r.bandwidth_hz / 1e3,
                r.labels.get("rds_pi"),
                r.iterations,
                r.evaluations,
                r.elapsed_s,
                audio.mode,
                r.quality
            );
            assert_eq!(r.provenance, hk_model::REFINED_BY_OUTPUT_ANALYSIS);
            assert_eq!(audio.mode, "wfm");
            assert!(
                err.abs() <= CENTER_TOL_HZ,
                "[{TAG}] {offset:+} Hz / {width} Hz: centre error {err} Hz"
            );
            assert!(
                (BANDWIDTH_HZ.0..=BANDWIDTH_HZ.1).contains(&r.bandwidth_hz),
                "[{TAG}] bandwidth {}",
                r.bandwidth_hz
            );
            assert_eq!(r.labels.get("rds_pi"), Some(&pi), "[{TAG}] RDS PI");
            assert_eq!(
                h.center_hz,
                Some(r.center_hz),
                "header reports the refined centre"
            );
            assert_eq!(h.bandwidth_hz, Some(r.bandwidth_hz));
            assert_eq!(audio.params.bandwidth_hz, Some(r.bandwidth_hz));
            table.push((
                offset,
                width,
                err,
                r.bandwidth_hz,
                r.iterations,
                r.elapsed_s,
            ));
            drop(stream);
        }
    }
    eprintln!("[{TAG}] start offset / width -> centre error, bandwidth, iterations, time");
    for (o, w, e, b, i, s) in &table {
        eprintln!(
            "[{TAG}]   {:+5.0} kHz / {:3.0} kHz -> {e:+6.0} Hz, {:5.1} kHz, {i}, {s:.2} s",
            o / 1e3,
            w / 1e3,
            b / 1e3
        );
    }

    // Listen on the station's inventory emitter (found blind through the private truth): the
    // refined tuning is stored on it and served.
    let emitter = {
        let deadline = Instant::now() + Duration::from_secs(240);
        loop {
            let repo = repo(&live.dir.0);
            let rows = inventory(&repo, InventoryQuery::default());
            let hits = matching(
                &truth,
                0.0,
                &rows,
                |e| (e.emitter.f_center_hz, e.emitter.bandwidth_hz),
                100e3,
            );
            if let Some(e) = hits.first() {
                break e.emitter.id;
            }
            assert!(
                Instant::now() < deadline,
                "[{TAG}] station never in the inventory"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    };
    let stream = open(listen.as_ref(), &[("emitter", emitter.to_string())]);
    let r = stream
        .header
        .audio
        .as_ref()
        .and_then(|a| a.refinement.clone())
        .expect("refined");
    assert!((r.center_hz - centre).abs() <= CENTER_TOL_HZ);
    // Status records (flat JSON) carry the refined tuning in force.
    let buf = Buf::default();
    stream
        .handle
        .subscribe(
            "t070-status",
            Declared::local(buf.clone()),
            Box::new(|_| {}),
        )
        .unwrap();
    let needle = format!("\"refined_center_hz\":{}", r.center_hz);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let bytes = buf.0.lock().unwrap().clone();
        if bytes.windows(needle.len()).any(|w| w == needle.as_bytes()) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "[{TAG}] no status record with {needle} ({} bytes read)",
            bytes.len()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(stream);

    let stored = repo(&live.dir.0)
        .refined_tuning_history(emitter)
        .unwrap()
        .into_iter()
        .find(|t| t.source == hk_pipeline::refine::SOURCE_LISTEN)
        .expect("listen stored a refined tuning on the emitter");
    assert_eq!(stored.provenance, hk_model::REFINED_BY_OUTPUT_ANALYSIS);
    assert!((stored.center_hz - centre).abs() <= CENTER_TOL_HZ);
    assert!(stored.detected_center_hz > 0.0, "detected values kept");

    let counters = live.handle.counters();
    live.handle.stop();
    finish(live.handle);
    let server = serve_api(&live.dir.0, counters);
    let (_, rows) = api_inventory(server.local_addr());
    let row = rows
        .iter()
        .find(|r| r["id"] == json!(emitter.to_string()))
        .expect("emitter served");
    eprintln!("[{TAG}] /api/inventory refined: {}", row["refined"]);
    assert_eq!(
        row["refined"]["provenance"],
        json!(hk_model::REFINED_BY_OUTPUT_ANALYSIS)
    );
    assert!((row["refined"]["center_hz"].as_f64().unwrap() - centre).abs() <= CENTER_TOL_HZ);
    assert!(
        row["f_center_hz"].as_f64().is_some(),
        "detected centre still served"
    );
}

#[test]
fn a_station_150_khz_off_raster_refines_to_its_centre_and_is_flagged_not_snapped() {
    const SHIFT_HZ: f64 = 150e3;
    const RASTER_CHANNEL_HZ: f64 = 101.5e6;
    let Some((meta, fx)) = private_truth(FM_FIXTURE) else {
        return;
    };
    if hardware_skip(TAG) {
        return;
    }
    let (truth, centre, _) = station(&fx);
    let shifted = centre + SHIFT_HZ;
    let run = blind_config(
        &meta,
        "t070off",
        BlindSource {
            iq_shift_hz: SHIFT_HZ,
            ..BlindSource::default()
        },
        json!({}),
    );
    let dir = run.dir;
    let handle = start(run.cfg, run.replay);
    let counters = handle.counters();
    finish(handle);

    let repo = repo(&dir.0);
    let rows = inventory(&repo, InventoryQuery::default());
    let hits = matching(
        &truth,
        SHIFT_HZ,
        &rows,
        |e| (e.emitter.f_center_hz, e.emitter.bandwidth_hz),
        100e3,
    );
    let refined: Vec<_> = hits
        .iter()
        .filter_map(|e| {
            repo.refined_tuning(e.emitter.id)
                .unwrap()
                .map(|r| (e.emitter.id, r))
        })
        .collect();
    assert!(
        !refined.is_empty(),
        "[{TAG}] no emitter at the shifted station carries a refined tuning ({} matched)",
        hits.len()
    );
    for (id, r) in &refined {
        let err = r.center_hz - shifted;
        eprintln!(
            "[{TAG}] off-raster: start {:.4} MHz -> refined {:.4} MHz (error {err:+.0} Hz), \
             bandwidth {:.1} kHz, detected {:.4} MHz, {} iterations, {} evaluations, {:.2} s, \
             source {}",
            r.start_center_hz / 1e6,
            r.center_hz / 1e6,
            r.bandwidth_hz / 1e3,
            r.detected_center_hz / 1e6,
            r.iterations,
            r.evaluations,
            r.elapsed_s,
            r.source
        );
        assert_eq!(r.provenance, hk_model::REFINED_BY_OUTPUT_ANALYSIS);
        assert!(
            err.abs() <= CENTER_TOL_HZ,
            "[{TAG}] refined centre error {err} Hz"
        );
        assert!(
            (r.center_hz - RASTER_CHANNEL_HZ).abs() > 40e3,
            "[{TAG}] snapped to the raster: {}",
            r.center_hz
        );
        let x = explanations(&repo, *id).unwrap();
        let fm = x
            .iter()
            .find(|e| e.service == "fm-broadcast")
            .unwrap_or_else(|| panic!("[{TAG}] FM broadcast not explained: {x:?}"));
        assert!(fm.has_flag("off-raster"), "[{TAG}] {fm:?}");
        let raster = fm
            .evidence
            .iter()
            .find_map(|e| match e {
                ExplanationEvidence::Raster {
                    offset_hz,
                    center_source,
                    nearest_channel_hz,
                    ..
                } => Some((*offset_hz, center_source.clone(), *nearest_channel_hz)),
                _ => None,
            })
            .expect("raster evidence");
        eprintln!("[{TAG}] raster evidence (offset, centre source, channel): {raster:?}");
        assert_eq!(raster.1, CENTER_REFINED);
        assert!((raster.2 - RASTER_CHANNEL_HZ).abs() < 1.0);
        assert!((raster.0 - (r.center_hz - RASTER_CHANNEL_HZ)).abs() < 1.0);
    }
    let server = serve_api(&dir.0, counters);
    let (_, api) = api_inventory(server.local_addr());
    assert!(
        api.iter()
            .any(|row| row["refined"]["provenance"] == json!(hk_model::REFINED_BY_OUTPUT_ANALYSIS)),
        "[{TAG}] /api/inventory serves the refined tuning"
    );
}
