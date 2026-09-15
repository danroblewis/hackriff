//! T-076 (SIGNAL-062): Listen on a live-like device plays audio. The FM fixture loops through the
//! mock SDR **paced at real time** (not lossless: the ring overwrites and the chain skips to the
//! live edge, as on a HackRF, DC spike at the tuned centre included), and each request streams for
//! longer than the background re-refinement interval (15 s). For every request the stream must
//! deliver PCM data records within a bounded time, open its squelch, report a sane SNR and keep a
//! refined centre on the station.
//!
//! Requests (blind: placed around the private truth, as a user drags near what they see):
//! - the supervisor's live request shape: a 400 kHz selection (`f_lo`/`f_hi`) on the station;
//! - a tight 150 kHz box around the station;
//! - the station's inventory emitter.
//!
//! `HK_T076_CASE=exact` runs only the literal live request `f_lo=100.6e6, f_hi=101.0e6`
//! (diagnostic; it fails the assertions). This capture is tuned to 100.8 MHz, the same settings as the live server, so that
//! box holds the HackRF DC spike and a weak signal about 2 dB above the noise, not the station,
//! which is at 101.3 MHz (+500 kHz). It stays squelched both at 268fcd4 and at e95ec47.
//!
//! The earlier Listen tests (T-043 acceptance, T-070 refine) ran the device **unpaced**
//! (lossless: the chain's gate cursor holds capture, so no skip, no overwrite) and asserted on
//! the stream header or a single status record, never on data records over a run longer than the
//! live re-refinement interval.

#[path = "acceptance/common.rs"]
mod common;

#[path = "acceptance/blind.rs"]
#[allow(dead_code)]
mod blind;

use std::time::{Duration, Instant};

use blind::{BlindSource, blind_live_paced, private_truth};
use common::*;
use hk_core::Pacing;
use hk_e2e::blind::matching;
use hk_model::InventoryQuery;
use hk_stream::{Declared, OpenRequest, OpenedStream, Record, StreamOpener, StreamReader};
use serde_json::Value;

const FM_FIXTURE: &str = "fm_100p8M_2p4M_l32g30a1_t1p5_5s";
const TAG: &str = "T-076";
/// Streaming time per request: longer than the 15 s live re-refinement interval.
const RUN_S: f64 = 20.0;
/// Longest wait for the first PCM record after the stream opened.
const FIRST_AUDIO_S: f64 = 5.0;
const CENTER_TOL_HZ: f64 = 5_000.0;
const MIN_SNR_DB: f64 = 15.0;

fn open(listen: &dyn StreamOpener, params: &[(&str, String)]) -> OpenedStream {
    let query: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect();
    let req = OpenRequest::from_query(&query, "t076-test");
    let deadline = Instant::now() + Duration::from_secs(60);
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

/// Streams `params` for [`RUN_S`] and asserts audio, squelch, SNR and the refined centre.
fn listen_plays(listen: &dyn StreamOpener, what: &str, params: &[(&str, String)], centre: f64) {
    let stream = open(listen, params);
    let h = stream.header.clone();
    let audio = h.audio.clone().expect("audio profile");
    eprintln!(
        "[{TAG}] {what}: mode {} centre {:?} bw {:?} snr {:?} squelch noise {:?}",
        audio.mode, h.center_hz, h.bandwidth_hz, audio.snr_db, audio.squelch.noise_dbfs,
    );
    let buf = Buf::default();
    stream
        .handle
        .subscribe("t076", Declared::local(buf.clone()), Box::new(|_| {}))
        .unwrap();
    let opened = Instant::now();
    std::thread::sleep(Duration::from_secs_f64(RUN_S));
    drop(stream);
    let bytes = buf.0.lock().unwrap().clone();
    let mut reader = StreamReader::new(&bytes[..]);
    reader.read_header().expect("stream header");
    let (mut data, mut statuses) = (0usize, Vec::<Value>::new());
    while let Ok(Some(r)) = reader.next_record() {
        match r {
            Record::Binary(b) if b.header.record_type == 1 => data += 1,
            // Status records (contract 1.1) come back as unknown frames.
            Record::Unknown(frame) => {
                if let Some((_, v)) = hk_stream::record::parse_status_record(&frame) {
                    statuses.push(v);
                }
            }
            _ => {}
        }
    }
    let every = (statuses.len() / 12).max(1);
    for s in statuses.iter().step_by(every) {
        eprintln!("[{TAG}] {what}: status {s}");
    }
    eprintln!(
        "[{TAG}] {what}: {data} data records, {} status records in {:.1} s",
        statuses.len(),
        opened.elapsed().as_secs_f64()
    );
    // 20 ms records: at least half the run's audio (skips on a loaded test host allowed).
    let want = (0.5 * (RUN_S - FIRST_AUDIO_S) / 0.02) as usize;
    assert!(
        data >= want,
        "[{TAG}] {what}: {data} PCM records in {RUN_S} s (want >= {want})"
    );
    let last = statuses.last().expect("status records");
    assert_eq!(
        last["squelch_open"],
        Value::Bool(true),
        "[{TAG}] {what}: squelch {last}"
    );
    let snr = last["snr_db"].as_f64();
    assert!(
        snr.is_none_or(|s| s >= MIN_SNR_DB),
        "[{TAG}] {what}: snr {last}"
    );
    for s in &statuses {
        if let Some(c) = s["refined_center_hz"].as_f64() {
            assert!(
                (c - centre).abs() <= CENTER_TOL_HZ,
                "[{TAG}] {what}: refined centre {c} off the station {centre}: {s}"
            );
        }
    }
}

fn station_centre(fx: &hk_e2e::Fixture) -> (hk_e2e::TruthItem, f64) {
    let s = fx.of_kind("wfm-broadcast")[0].clone();
    let c = s.expect_f64("/center_hz");
    (s, c)
}

#[test]
fn listen_on_a_live_paced_device_streams_audio_through_re_refinement() {
    let Some((meta, fx)) = private_truth(FM_FIXTURE) else {
        return;
    };
    if hardware_skip(TAG) {
        return;
    }
    let (truth, centre) = station_centre(&fx);
    let live = blind_live_paced(
        &meta,
        "t076",
        BlindSource::default(),
        Pacing::RealTime { speed: 1.0 },
    );
    let listen = live.handle.listen_service();
    let only = std::env::var("HK_T076_CASE").unwrap_or_default();
    let run = |name: &str| only.is_empty() || only == name;

    if only == "exact" {
        // Diagnostic: the supervisor's literal request (on this capture's tuned centre).
        listen_plays(
            listen.as_ref(),
            "exact 100.6-101.0 MHz",
            &[("f_lo", "100600000".into()), ("f_hi", "101000000".into())],
            centre,
        );
    }
    if run("selection") {
        listen_plays(
            listen.as_ref(),
            "400 kHz selection",
            &[
                ("f_lo", format!("{}", centre - 200e3)),
                ("f_hi", format!("{}", centre + 200e3)),
            ],
            centre,
        );
    }
    if run("tight") {
        listen_plays(
            listen.as_ref(),
            "150 kHz box",
            &[
                ("f_lo", format!("{}", centre - 75e3)),
                ("f_hi", format!("{}", centre + 75e3)),
            ],
            centre,
        );
    }
    if run("emitter") {
        let emitter = {
            let deadline = Instant::now() + Duration::from_secs(120);
            loop {
                let rows = inventory(&repo(&live.dir.0), InventoryQuery::default());
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
        listen_plays(
            listen.as_ref(),
            "inventory emitter",
            &[("emitter", emitter.to_string())],
            centre,
        );
    }
    live.handle.stop();
    finish(live.handle);
}
