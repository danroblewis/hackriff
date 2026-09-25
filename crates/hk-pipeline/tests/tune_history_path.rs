//! **T-898 through the mock SDR: the radio's own route is drawn where it actually went.**
//!
//! docs/23 §10.6 rule 2 (the user's second map-UI principle): the device's movement through
//! frequency has (time × frequency) coordinates, so it belongs on the map as a traced path — a
//! directions line. This test scripts a run of retunes through the mock device's interface (never
//! by feeding the pipeline a file), then asserts what `GET /api/tune-history` would serve over the
//! recorded tune intervals:
//!
//!  - the route's **vertices land on the recorded retune instants and centres** — every leg
//!    boundary is a boundary the observation log independently records, and the ordered distinct
//!    centres are exactly the centres that were commanded;
//!  - a **dwell is a vertical run**: two vertices at one frequency, start and end;
//!  - the route is **one per front end**, labelled with the device that produced the samples (the
//!    several-SDRs rule), never the config's `device_id`;
//!  - the same route is served **whichever part of it is on screen** (a pan re-shapes nothing), and
//!    a window the radio never visited or crossed draws nothing.
//!
//! The pixel side of "it moves with the tile rows under scroll and zoom" is structural in the
//! client and asserted there (`ui/test/surface-tune.test.ts`): the vertices are placed through the
//! very `toClip` the tiles are placed with, inside the same render frame.

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::*;
use hk_api::ApiState;
use hk_api::tune_history::{TuneHistoryQuery, answer};
use hk_core::{MockEnd, Pacing};
use hk_model::attention::observation::ObservationRecord;
use hk_model::{FreqRange, Region, TimeRange, Timestamp};
use hk_pipeline::{Pipeline, PipelineConfig, TrackInventory, open_mock_replay, replay_plan};
use hk_store::observation::{MAX_RECORD_LIMIT, RecordQuery};

/// A `PipelineConfig::device_id` naming no device that produced anything (T-314/T-378's point):
/// if the route ever labels a path with this, it is naming a radio that recorded nothing.
const CONFIG_NAMES_NO_DEVICE: &str = "config-default:names-no-device";

fn wait_for(what: &str, limit: Duration, f: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + limit;
    while !f() {
        assert!(std::time::Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn ts_s(t: Timestamp) -> f64 {
    t.as_unix_nanos() as f64 * 1e-9
}

#[test]
fn the_retune_route_is_drawn_where_the_radio_actually_went() {
    const FS: f64 = 1e6;
    // The scripted route: four dwells, three retunes, up and then back down — so the drawn line
    // cannot be confused with a monotone ramp, and a repeated centre is not a repeated leg.
    const CENTERS: [f64; 4] = [433.92e6, 435.42e6, 436.92e6, 435.42e6];
    let dir = TempDir::new("tune-path");
    let rec = tone_recording(&dir.0.join("src"), "tone", FS, 2.0, CENTERS[0], None);
    let replay = open_mock_replay(&rec, Pacing::RealTime { speed: 4.0 }, MockEnd::Loop).unwrap();
    let plan = replay_plan(
        replay.info.center_hz,
        replay.info.sample_rate_hz,
        replay.info.start_time,
    );
    let mut cfg = PipelineConfig::new(dir.0.join("data"), plan).unwrap();
    cfg.source_class = replay.class;
    cfg.live_window_class = true;
    let device = replay.device.device_id.clone();
    assert_ne!(device, CONFIG_NAMES_NO_DEVICE);
    cfg.device_id = CONFIG_NAMES_NO_DEVICE.into();
    let handle = Pipeline::start(
        cfg,
        Box::new(replay.source),
        replay.info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let counters = handle.counters();
    let plane = handle.controller();
    let limit = Duration::from_secs(60);
    // Drive the device through the scripted route, each dwell long enough to be measured.
    for (k, c) in CENTERS.iter().enumerate() {
        if k > 0 {
            plane.retune(*c, FS).expect("retune");
        }
        wait_for("the tune reaches the analysed data", limit, || {
            (f64::from_bits(counters.tune_center_bits.load(Ordering::Relaxed)) - c).abs() < 1.0
        });
        let s0 = counters.stream_time_ns.load(Ordering::Relaxed);
        wait_for("a second of stream time at the tune", limit, || {
            counters.stream_time_ns.load(Ordering::Relaxed) >= s0 + 1_000_000_000
        });
    }
    let store = handle
        .observation_store()
        .expect("the run keeps an observation log");
    handle.stop();
    let (_summary, stopped) = wait_guarded(handle, Duration::from_secs(60));
    assert!(!stopped, "the run stopped when asked");

    // ---- the hidden truth: the tune intervals the log recorded, read independently ----
    let window = TimeRange::new(
        Timestamp::from_unix_nanos(0),
        Timestamp::from_unix_nanos(i64::MAX / 4),
    );
    let page = store.query(&RecordQuery {
        freq: FreqRange::new(0.0, 7e9),
        span: window,
        tier: None,
        cursor: 0,
        limit: MAX_RECORD_LIMIT,
    });
    let mut dwells: Vec<(TimeRange, f64, Option<String>)> = page
        .records
        .iter()
        .filter_map(|r| match r {
            ObservationRecord::Dwell(d) => {
                Some((d.observed, d.window.center_hz, d.device_id.clone()))
            }
            _ => None,
        })
        .filter(|(t, _, _)| t.duration_ns() > 0)
        .collect();
    dwells.sort_by_key(|(t, _, _)| t.start.as_unix_nanos());
    eprintln!(
        "[T-898] {} dwell records: {:?}",
        dwells.len(),
        dwells
            .iter()
            .map(|(t, c, _)| (ts_s(t.start), ts_s(t.end), c / 1e6))
            .collect::<Vec<_>>()
    );
    assert!(
        dwells.len() >= CENTERS.len() - 1,
        "a dwell seals on each retune: {dwells:?}"
    );
    for (_, _, id) in &dwells {
        assert_eq!(
            id.as_deref(),
            Some(device.as_str()),
            "a record names the source's own front end, never the config default"
        );
    }
    // The distinct centres, in the order they were recorded: the scripted route.
    let mut recorded_centres: Vec<f64> = Vec::new();
    for (_, c, _) in &dwells {
        if recorded_centres.last() != Some(c) {
            recorded_centres.push(*c);
        }
    }
    assert_eq!(
        recorded_centres.len(),
        CENTERS.len(),
        "every commanded tune is recorded once, in order: {recorded_centres:?}"
    );
    for (got, want) in recorded_centres.iter().zip(CENTERS) {
        assert!((got - want).abs() < 1.0, "{recorded_centres:?}");
    }

    // ---- what the route serves over exactly those records ----
    let t0 = ts_s(dwells.first().unwrap().0.start) - 1.0;
    let t1 = ts_s(dwells.last().unwrap().0.end) + 1.0;
    let state = ApiState {
        observations: Some(store.clone()),
        ..ApiState::default()
    };
    let query = |f_lo: f64, f_hi: f64, t0: f64, t1: f64| TuneHistoryQuery {
        window: Region::new(
            FreqRange::new(f_lo, f_hi),
            TimeRange::new(
                Timestamp::from_unix_nanos((t0 * 1e9) as i64),
                Timestamp::from_unix_nanos((t1 * 1e9) as i64),
            ),
        ),
        device: None,
        limit: 64,
    };
    let whole = query(430e6, 440e6, t0, t1);
    let v = answer(&state, &whole);
    eprintln!("[T-898] tune-history: {v}");
    assert_eq!(
        v["total"],
        serde_json::json!(1),
        "one front end, one run: {v}"
    );
    let p = &v["paths"][0];
    assert_eq!(
        p["device"].as_str(),
        Some(device.as_str()),
        "the route is labelled by the radio that produced the samples: {p}"
    );
    assert_eq!(p["device_named"], serde_json::json!(true), "{p}");
    assert_eq!(
        p["retunes"].as_u64(),
        Some(CENTERS.len() as u64 - 1),
        "three scripted retunes: {p}"
    );
    assert_eq!(v["devices"], serde_json::json!([device]), "{v}");

    // Every vertex lands on a recorded instant, at a recorded centre, and a dwell is a vertical
    // run: (start, f) then (end, f) at the same frequency.
    let vertices = p["vertices"].as_array().expect("vertices").clone();
    assert_eq!(vertices.len() % 2, 0, "two vertices per leg: {vertices:?}");
    let legs: Vec<(f64, f64, f64)> = vertices
        .chunks(2)
        .map(|w| {
            let (a, b) = (&w[0], &w[1]);
            assert_eq!(a["at"], serde_json::json!("start"), "{a}");
            assert_eq!(b["at"], serde_json::json!("end"), "{b}");
            assert_eq!(a["f_hz"], b["f_hz"], "a dwell is a vertical run: {a} {b}");
            (
                a["t_s"].as_f64().unwrap(),
                b["t_s"].as_f64().unwrap(),
                a["f_hz"].as_f64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        legs.len(),
        CENTERS.len(),
        "one leg per scripted dwell: {legs:?}"
    );
    for (leg, want) in legs.iter().zip(CENTERS) {
        assert!((leg.2 - want).abs() < 1.0, "the scripted centres: {legs:?}");
        assert!(leg.1 > leg.0, "a leg runs forward in time: {legs:?}");
        // The instants are the log's own, not resampled or rounded onto a grid.
        assert!(
            dwells
                .iter()
                .any(|(t, c, _)| (c - leg.2).abs() < 1.0 && (ts_s(t.start) - leg.0).abs() < 1e-6),
            "leg start {:.6} is a recorded instant: {dwells:?}",
            leg.0
        );
        assert!(
            dwells
                .iter()
                .any(|(t, c, _)| (c - leg.2).abs() < 1.0 && (ts_s(t.end) - leg.1).abs() < 1e-6),
            "leg end {:.6} is a recorded instant: {legs:?}",
            leg.1
        );
    }
    // A retune's vertex is at the instant the tune changed: the legs abut.
    for w in legs.windows(2) {
        assert!(
            (w[1].0 - w[0].1).abs() < 0.5,
            "the jump is at the retune instant, not across a gap: {legs:?}"
        );
    }

    // The same route, whichever part of it is on screen: a pan re-shapes nothing.
    let mid = (t0 + t1) / 2.0;
    let panned = answer(&state, &query(430e6, 440e6, mid, t1));
    assert_eq!(panned["paths"][0]["id"], p["id"], "the id survives a pan");
    assert_eq!(
        panned["paths"][0]["vertices"], p["vertices"],
        "the vertices survive a pan: {panned}"
    );
    // A zoom into one dwell's band still carries the whole route (the jumps cross the pane).
    let zoomed = answer(
        &state,
        &query(CENTERS[1] - 100e3, CENTERS[1] + 100e3, t0, t1),
    );
    assert_eq!(
        zoomed["paths"][0]["vertices"], p["vertices"],
        "a zoom re-shapes nothing: {zoomed}"
    );
    // A band the radio neither visited nor crossed draws nothing.
    let elsewhere = answer(&state, &query(2.0e9, 2.1e9, t0, t1));
    assert_eq!(elsewhere["total"], serde_json::json!(0), "{elsewhere}");
    // Nor does a window before the radio was ever recorded.
    let before = answer(&state, &query(430e6, 440e6, t0 - 4000.0, t0 - 3000.0));
    assert_eq!(before["total"], serde_json::json!(0), "{before}");
}
