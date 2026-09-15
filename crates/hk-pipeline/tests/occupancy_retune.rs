//! T-118 review through the mock SDR: a live run retuned between two bands inside one 15-min
//! interval persists occupancy rows for **both** bands (every band the observation log saw in the
//! interval is evaluated, not only the one tuned at close), with the additive floor fields set.
//!
//! Also the real FM fixture (a dense band): a print of local floors, occupied runs vs. gaps and
//! the suspect flag, then (T-129) asserts the learned channels against the fixture's station truth
//! (one channel per station covering its occupied band, occupied) and that the band row has `fco`.

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::*;
use hk_context::occupancy::engine::{EngineConfig, LevelSource};
use hk_context::occupancy::threshold::eighty_percent_floor_db;
use hk_core::{MockEnd, Pacing};
use hk_model::attention::occupancy::OccupancySubject;
use hk_model::{FreqRange, TimeRange};
use hk_pipeline::{Pipeline, PipelineConfig, TrackInventory, open_mock_replay, replay_plan};
use hk_store::occupancy::{OccupancyQuery, SeriesInterval};
use serde_json::json;

fn wait_for(what: &str, limit: Duration, f: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + limit;
    while !f() {
        assert!(std::time::Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn occupancy_rows_cover_every_band_retuned_within_one_interval() {
    const FS: f64 = 1e6;
    const CENTERS: [f64; 2] = [433.92e6, 436.42e6];
    let dir = TempDir::new("occ-retune");
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
    cfg.device_id = replay.device.device_id.clone();
    let t0 = replay.info.start_time;
    let handle = Pipeline::start(
        cfg,
        Box::new(replay.source),
        replay.info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let occ = handle.occupancy();
    let counters = handle.counters();
    let plane = handle.controller();
    let limit = Duration::from_secs(60);
    for (k, c) in CENTERS.iter().enumerate() {
        if k > 0 {
            plane.retune(*c, FS).expect("retune");
        }
        let tuned =
            || (f64::from_bits(counters.tune_center_bits.load(Ordering::Relaxed)) - c).abs() < 1.0;
        wait_for("the tune reaches the analysed data", limit, tuned);
        let s0 = counters.stream_time_ns.load(Ordering::Relaxed);
        wait_for("two seconds of stream time at the tune", limit, || {
            counters.stream_time_ns.load(Ordering::Relaxed) >= s0 + 2_000_000_000
        });
    }
    handle.stop();
    let (_summary, stopped) = wait_guarded(handle, Duration::from_secs(60));
    assert!(!stopped, "the run stopped when asked");

    let (_, f_cell) = occ.plan_info();
    let rows = occ
        .query(&OccupancyQuery {
            freq: FreqRange::new(0.0, 7e9),
            span: TimeRange::new(
                t0.saturating_add_nanos(-3_600_000_000_000),
                t0.saturating_add_nanos(3_600_000_000_000),
            ),
            interval: SeriesInterval::Min15,
            subject: None,
            f_cell_hz: f_cell,
            limit: 10_000,
        })
        .unwrap()
        .rows;
    let bands: Vec<(FreqRange, TimeRange)> = rows
        .iter()
        .filter_map(|r| match r.subject {
            OccupancySubject::Band { freq } => Some((freq, r.interval)),
            OccupancySubject::Channel { .. } => None,
        })
        .collect();
    eprintln!(
        "[T-118] {} rows; bands {:?}; service {:?}",
        rows.len(),
        bands
            .iter()
            .map(|(f, _)| (f.lo_hz / 1e6, f.hi_hz / 1e6))
            .collect::<Vec<_>>(),
        occ.stats()
    );
    for c in CENTERS {
        assert!(
            bands.iter().any(|(f, _)| f.lo_hz < c && f.hi_hz > c),
            "no band row covers {:.2} MHz: {bands:?}",
            c / 1e6
        );
    }
    assert!(
        bands.iter().all(|(_, iv)| *iv == bands[0].1),
        "both bands in one interval: {bands:?}"
    );
    for r in &rows {
        r.validate().unwrap();
        assert!(r.floor_db.is_some() && r.floor_source.is_some(), "{r:?}");
    }
}

/// T-118 sanity print over the real FM fixture (local floors, occupied runs); T-129 asserts the
/// learned channels against the fixture's station truth and the band row's `fco`.
#[test]
fn occupancy_fm_fixture_local_floor_sanity() {
    let Some(meta) = real_fixture("fm_100p8M_2p4M_l32g30a1_t1p5_5s") else {
        return;
    };
    let dir = TempDir::new("occ-fm");
    let (cfg, replay) = replay_config(&dir.0, &meta, json!({}), Pacing::Unpaced);
    let handle = start(cfg, replay);
    let occ = handle.occupancy();
    let product = handle.floor_product();
    let _ = handle.wait().unwrap();
    let (band, grid) = {
        let p = product.lock().unwrap();
        let py = p.uncalibrated_pyramid();
        let end = py.latest_frame_end().expect("history holds frames");
        let span = TimeRange::new(
            end.saturating_add_nanos(-60_000_000_000),
            end.saturating_add_nanos(1_000_000_000),
        );
        let band = FreqRange::centered(100.8e6, 0.8 * 2.4e6);
        (
            (band, span),
            LevelSource::level0(py, band, span).expect("grid"),
        )
    };
    let cfg = EngineConfig::default();
    let fl = cfg.local_floors(&grid);
    let guard = cfg.threshold.guard_db;
    let hz = |f: usize| (grid.f_first_cell + f as i64) as f64 * grid.f_cell_hz;
    let busy_frac = |thr: &dyn Fn(usize) -> f64| -> Vec<f64> {
        (0..grid.nf)
            .map(|f| {
                let (mut n, mut k) = (0u32, 0u32);
                for t in 0..grid.nt {
                    let c = grid.cell(t, f);
                    if c.observed() && c.mean_db.is_finite() {
                        n += 1;
                        k += u32::from(f64::from(c.mean_db) > thr(f));
                    }
                }
                if n == 0 {
                    0.0
                } else {
                    f64::from(k) / f64::from(n)
                }
            })
            .collect()
    };
    let local = busy_frac(&|f| fl.floor_db[f] + guard);
    let floors: Vec<f64> = grid.cells.iter().map(|c| f64::from(c.floor_db)).collect();
    let whole = eighty_percent_floor_db(&floors, 0.8).unwrap_or(f64::NAN);
    let old = busy_frac(&|_| whole + guard);
    let runs = |b: &[f64]| {
        let mut out = Vec::new();
        let mut start = None;
        for f in 0..=b.len() {
            match (f < b.len() && b[f] >= 0.5, start) {
                (true, None) => start = Some(f),
                (false, Some(s)) => {
                    out.push(format!("{:.3}-{:.3}", hz(s) / 1e6, hz(f) / 1e6));
                    start = None;
                }
                _ => {}
            }
        }
        out
    };
    let frac = |b: &[f64]| b.iter().filter(|x| **x >= 0.5).count() as f64 / b.len() as f64;
    let suspect = fl.suspect.iter().filter(|s| **s).count() as f64 / grid.nf as f64;
    eprintln!(
        "[T-118 FM] {} rows x {} cols; whole-band floor {whole:.1} dB -> {:.0} % columns occupied; \
         local floors {:.1}..{:.1} dB -> {:.0} % occupied; suspect {:.0} % of columns",
        grid.nt,
        grid.nf,
        100.0 * frac(&old),
        fl.floor_db.iter().copied().fold(f64::INFINITY, f64::min),
        fl.floor_db
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max),
        100.0 * frac(&local),
        100.0 * suspect
    );
    eprintln!("[T-118 FM] local occupied runs (MHz): {:?}", runs(&local));
    eprintln!(
        "[T-118 FM] whole-band occupied runs (MHz): {:?}",
        runs(&old)
    );
    let station = ((101.3e6 / grid.f_cell_hz).round() as i64 - grid.f_first_cell) as usize;
    if local.get(station).is_none_or(|x| *x < 0.5) {
        eprintln!("[T-118 FM] SANITY WARN: 101.3 MHz station not occupied");
    }
    let (band, span) = band;
    let rows = occ.span_stats(band, span).expect("span stats");
    let (_, f_cell) = occ.plan_info();
    let mut channels = Vec::new();
    let mut band_row = None;
    for r in &rows {
        let extent = match r.subject {
            OccupancySubject::Channel { key } => {
                let f = key.freq(f_cell);
                channels.push((f, r.fco));
                format!("ch {:.3}-{:.3}", f.lo_hz / 1e6, f.hi_hz / 1e6)
            }
            OccupancySubject::Band { .. } => {
                band_row = Some(r.clone());
                "band".into()
            }
        };
        eprintln!(
            "[T-129 FM] {extent:<18} fco {:?} all-visits {:?} fbo {:?} floor {:?} {:?} suspect {:?}/{} occ p50 {:?} idle {:?}",
            r.fco,
            r.fco_all_visits,
            r.fbo,
            r.floor_db.map(|x| (x * 10.0).round() / 10.0),
            r.floor_source,
            r.floor_suspect,
            r.n_suspect,
            r.level_occupied_p50_db.map(|x| (x * 10.0).round() / 10.0),
            r.level_idle_db.map(|x| (x * 10.0).round() / 10.0),
        );
    }
    // T-129 against the fixture's hidden truth (annotations partial: the 101.3 MHz station, the
    // 100.000 MHz spur, DC): one channel of the station's occupied band, no in-band fragments.
    let stations = emission_truth(&meta);
    assert!(!stations.is_empty(), "fixture carries station truth");
    assert_station_channels("T-129 FM", &channels, &stations, 2.0);
    let band_row = band_row.expect("a band row");
    assert!(
        band_row.fco.is_some() && band_row.fco_all_visits.is_some(),
        "band row fco {:?} / all visits {:?}",
        band_row.fco,
        band_row.fco_all_visits
    );
}
