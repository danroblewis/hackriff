//! T-1021 — **what a viewport of `/api/tiles` reads costs the live edge, measured.**
//!
//! The tile-latency review (2026-09-25) found that each `/api/tiles` chunk holds the view-history
//! mutex for up to 500 k source cells (~70 ms at 0.14 µs/cell) and asked, once T-1018's exact-node
//! read had landed, for the hold and the ingest latency to be **measured** under a realistic load
//! before anyone changed the chunking.
//!
//! What competes for that mutex, read off the code rather than assumed:
//!
//! - **The capture thread does not.** It pushes blocks into the ring; nothing on it locks a
//!   pyramid.
//! - **The ring readers do not wait on it.** The spectrum reader hands each display row to the
//!   view writer through a queue that never blocks (`ViewQueue::push`, drop-oldest past 1500 rows);
//!   the history reader feeds scheme 1 through `FloorIngestQueue` and only *tries* the view lock
//!   for its tile counters.
//! - **The view writer (`hk-view`) does** — one hold per display row, to fold it. So a tile read's
//!   hold delays the *growing edge of the view lattice* (the rows `/api/tiles` and
//!   `/ws/tiles/rows` serve), and past the queue's depth it would drop rows.
//!
//! So the measurement is of those three things, while a paced 2.4 Msps mock SDR (the review's rate)
//! runs live and four readers (`TILE_MAX_IN_FLIGHT`) read a viewport of ~250 tiles across the
//! levels the review measured:
//!
//! 1. **per-request**: `cost.build_ms`, `cost.chunks` (one hold each) and `cost.source_cells`, as
//!    the route reports them;
//! 2. **per-hold, as ingest sees it**: a probe that takes the view mutex the way the writer does —
//!    one short hold, then let go — and times how long it waited. Its worst wait bounds how long a
//!    row fold waited behind a tile chunk;
//! 3. **the live edge**: stream time minus the view store's `latest_frame_end` at each probe
//!    (baseline vs under load), view rows dropped, the lossless gate's waits, ring overruns.
//!
//! **Timing tier** (`.config/nextest.toml`, `just timing`): the assertions are latency bounds, a
//! property of the box's headroom as much as of the code.

mod common;
#[path = "support/radio.rs"]
mod radio;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use hk_model::Timestamp;
use hk_pipeline::class::window_class;
use hk_pipeline::{Pipeline, PipelineConfig, SourceInfo, TrackInventory, replay_plan};
use hk_store::Pyramid;
use serde_json::json;

const CENTER: f64 = 100.8e6;
/// The review's staging rate (live HackRF at 2.4 Msps).
const FS: f64 = 2.4e6;
const OFFSET_HZ: f64 = 200e3;
const BLOCK: usize = 16_384;
/// Capture seconds of history laid down (unpaced) before the measurement: the review's staging
/// server had ~4 min.
const FILL_S: f64 = 180.0;
const LIMIT: Duration = Duration::from_secs(600);
/// Tiles in the viewport — the review's "135–290 per screen".
const VIEWPORT_TILES: usize = 250;
/// How far the view lattice's growing edge may fall behind capture under the viewport, beyond the
/// baseline's own worst: 1.5 s. Measured on the dev Mac, over baseline: with the yield ~0.1 s at
/// load ~12 and ~1.2 s at load ~32; without it ~1.9 s at load ~15 and 14-31 s at load ~33-37.
const LAG_BOUND_MS: f64 = 1500.0;
/// The route's own producer cap: four reads in flight at once.
const READERS: usize = hk_api::tiles::TILE_MAX_IN_FLIGHT;

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * p).round() as usize]
}

/// One probe sample: `(lock wait ms, live-edge lag ms)`.
fn probe(view: &Arc<Mutex<Pyramid>>, stream_now: &dyn Fn() -> i64) -> (f64, f64) {
    let s = stream_now();
    let t = Instant::now();
    let p = view.lock().unwrap();
    let waited = t.elapsed().as_secs_f64() * 1e3;
    let edge = p.latest_frame_end().map_or(0, Timestamp::as_unix_nanos);
    drop(p);
    (waited, (s - edge) as f64 / 1e6)
}

#[derive(Default, Debug)]
struct Phase {
    waits: Vec<f64>,
    lags: Vec<f64>,
}

impl Phase {
    fn report(&mut self, what: &str) -> (f64, f64) {
        let n = self.waits.len();
        let w = (
            pct(&mut self.waits, 0.5),
            pct(&mut self.waits, 0.99),
            pct(&mut self.waits, 1.0),
        );
        let l = (
            pct(&mut self.lags, 0.5),
            pct(&mut self.lags, 0.99),
            pct(&mut self.lags, 1.0),
        );
        eprintln!(
            "T-1021 {what}: {n} probes; view-mutex wait ms p50 {:.3} p99 {:.3} max {:.3}; \
             live-edge lag ms p50 {:.1} p99 {:.1} max {:.1}",
            w.0, w.1, w.2, l.0, l.1, l.2
        );
        (w.2, l.2)
    }
}

#[test]
fn a_viewport_of_tile_reads_never_stalls_the_view_lattices_live_edge() {
    use hk_api::http::ApiState;
    use hk_api::tiles::{TILE_CELLS, tiles_json};

    // The product wires the yield (`hk serve`); `T1021_NO_YIELD=1` measures the route without it —
    // the before half, and the red proof that the bound below is the yield's to keep.
    let yield_to_ingest = std::env::var_os("T1021_NO_YIELD").is_none();
    eprintln!("T-1021 tile reads yield to the view writer: {yield_to_ingest}");
    let dir = TempDir::new("tile-lock-hold");
    let (rx, ctl) = radio::Radio::new(CENTER, FS, BLOCK, radio::tone(|_| OFFSET_HZ));
    let t0 = Timestamp::from_unix_nanos(radio::T0_NS);
    let mut plan = replay_plan(CENTER, FS, t0);
    plan.extra = json!({ "pipeline": { "ring_s": 2.0 } });
    let mut cfg = PipelineConfig::new(&dir.0, plan).unwrap();
    cfg.source_class = window_class(CENTER, FS);
    cfg.live_window_class = true;
    // Lossless: a ring reader that falls half a ring behind HOLDS the capture thread and counts a
    // `gate_waits` — which is exactly the stall this test is looking for, made countable.
    cfg.lossless = true;
    cfg.settings.chains = Some(Vec::new());
    let handle = Pipeline::start(
        cfg,
        Box::new(rx),
        SourceInfo {
            sample_rate_hz: FS,
            center_hz: CENTER,
            start_time: t0,
        },
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let counters = handle.counters();
    let view = handle.view_history().expect("a view-scheme pyramid");
    let stream_now = || {
        counters
            .stream_time_ns
            .load(Ordering::Relaxed)
            .max(radio::T0_NS)
    };

    // ---- history: FILL_S of capture, unpaced, then paced to real time from there ----
    let fill = (FILL_S * FS) as u64;
    let started = Instant::now();
    ctl.hold_at(fill);
    assert!(ctl.wait_emitted(fill, LIMIT), "the fill never completed");
    eprintln!(
        "T-1021 fill: {FILL_S} s of capture at {:.1} Msps in {:.1} s wall",
        FS / 1e6,
        started.elapsed().as_secs_f64()
    );
    // Let the chain drain the fill (the store's edge within a second of stream time).
    let deadline = Instant::now() + LIMIT;
    loop {
        let (_, lag) = probe(&view, &stream_now);
        if lag < 1000.0 {
            break;
        }
        assert!(Instant::now() < deadline, "the view store never caught up");
        std::thread::sleep(Duration::from_millis(50));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let pacer = {
        let (ctl, stop) = (Arc::clone(&ctl), Arc::clone(&stop));
        std::thread::spawn(move || {
            let base = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                ctl.hold_at(fill + (base.elapsed().as_secs_f64() * FS) as u64);
                std::thread::sleep(Duration::from_millis(2));
            }
        })
    };
    // Settle into real time.
    std::thread::sleep(Duration::from_secs(2));

    let snap = || {
        (
            counters.history.view_dropped.load(Ordering::Relaxed),
            counters.source.gate_waits.load(Ordering::Relaxed),
            counters.detect_reader.lost_samples.load(Ordering::Relaxed)
                + counters.history_reader.lost_samples.load(Ordering::Relaxed)
                + counters
                    .spectrum_reader
                    .lost_samples
                    .load(Ordering::Relaxed),
            counters.history.view_frames.load(Ordering::Relaxed),
        )
    };
    let run_probe = |until: &AtomicBool, into: &Mutex<Phase>| {
        while !until.load(Ordering::Relaxed) {
            let (w, l) = probe(&view, &stream_now);
            let mut ph = into.lock().unwrap();
            ph.waits.push(w);
            ph.lags.push(l);
            drop(ph);
            std::thread::sleep(Duration::from_millis(1));
        }
    };

    // ---- baseline: live ingest, no tile reads ----
    let base_phase = Mutex::new(Phase::default());
    let base_snap = snap();
    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| run_probe(&done, &base_phase));
        std::thread::sleep(Duration::from_secs(5));
        done.store(true, Ordering::Relaxed);
    });
    let base_end = snap();

    // ---- the viewport ----
    let (f0, t0_cell) = {
        let p = view.lock().unwrap();
        let g = p.geometry();
        (g.levels[0].f_cell_hz, g.levels[0].t_cell_ns)
    };
    let now_ns = stream_now();
    let levels = [(0, 0), (1, 0), (1, 1), (2, 1), (3, 1), (4, 1), (6, 1)];
    let mut addrs: Vec<(usize, usize, i64, i64)> = Vec::new();
    // A pane 12 MHz wide over the recorded history at each level above — newest time first, the
    // way a live pane asks — an equal share of the viewport per level (the review's measurements
    // span (0,0) to (6,1)).
    let per_level = VIEWPORT_TILES.div_ceil(levels.len());
    for &(lf, lt) in &levels {
        let f_span = f0 * 2f64.powi(lf as i32) * TILE_CELLS as f64;
        let t_span = t0_cell * (1i64 << lt) * TILE_CELLS as i64;
        let (fa, fb) = (
            ((CENTER - 6e6) / f_span).floor() as i64,
            ((CENTER + 6e6) / f_span).floor() as i64,
        );
        let (ta, tb) = (radio::T0_NS.div_euclid(t_span), now_ns.div_euclid(t_span));
        let mut n = 0;
        'level: for t in (ta..=tb).rev() {
            for f in fa..=fb {
                if n == per_level {
                    break 'level;
                }
                addrs.push((lf, lt, f, t));
                n += 1;
            }
        }
    }
    // What `hk serve` gives the route for this: the view lattice, the observation log (the
    // coverage map's tune history, so an untuned tile is answered from coverage alone, T-461) and
    // the hot-tile cache (T-572).
    let state = ApiState {
        view_history: Some(Arc::clone(&view)),
        observations: handle.observation_store(),
        tile_cache: Some(Arc::new(hk_api::tiles::HotTileCache::default())),
        view_ingest_backlog: yield_to_ingest.then(|| {
            let c = Arc::clone(&counters);
            Arc::new(move || c.history.view_backlog.load(Ordering::Relaxed))
                as Arc<dyn Fn() -> u64 + Send + Sync>
        }),
        ..ApiState::default()
    };
    // (level_f, level_t, build_ms, chunks, source_cells, answered level, exact node, from cache,
    //  lock holds, hold ms total, hold ms max, shadow holds, hold CPU ms total, yielded ms)
    type Row = (
        usize,
        usize,
        f64,
        u64,
        u64,
        i64,
        bool,
        bool,
        u64,
        f64,
        f64,
        u64,
        f64,
        f64,
    );
    let results: Mutex<Vec<Row>> = Mutex::new(Vec::new());
    let mut passes = Vec::new();
    for pass in 0..2 {
        let hold_max = Mutex::new(0f64);
        let load_phase = Mutex::new(Phase::default());
        let before = snap();
        let next = std::sync::atomic::AtomicUsize::new(0);
        let done = AtomicBool::new(false);
        let wall = Instant::now();
        std::thread::scope(|s| {
            s.spawn(|| run_probe(&done, &load_phase));
            let readers: Vec<_> = (0..READERS)
                .map(|_| {
                    s.spawn(|| {
                        loop {
                            let k = next.fetch_add(1, Ordering::Relaxed);
                            let Some(&(lf, lt, fi, ti)) = addrs.get(k) else {
                                break;
                            };
                            let q: Vec<(String, String)> = [
                                ("scheme", "view".to_string()),
                                ("level_f", lf.to_string()),
                                ("level_t", lt.to_string()),
                                ("f_index", fi.to_string()),
                                ("t_index", ti.to_string()),
                                ("cells", TILE_CELLS.to_string()),
                                ("planes", "f16".to_string()),
                            ]
                            .into_iter()
                            .map(|(a, b)| (a.to_string(), b))
                            .collect();
                            // A 503 (every slot out) is the route's backpressure; retry as a
                            // client would.
                            let v = loop {
                                match tiles_json(&state, &q) {
                                    Ok(v) => break v,
                                    Err(e) if e.status == 503 => {
                                        std::thread::sleep(Duration::from_millis(1));
                                    }
                                    Err(e) => panic!("tile ({lf},{lt},{fi},{ti}): {e:?}"),
                                }
                            };
                            let c = &v["cost"];
                            if pass == 0 {
                                results.lock().unwrap().push((
                                    lf,
                                    lt,
                                    c["build_ms"].as_f64().unwrap_or(0.0),
                                    c["chunks"].as_u64().unwrap_or(0),
                                    c["source_cells"].as_u64().unwrap_or(0),
                                    v["resolution"]["answered"]["level"].as_i64().unwrap_or(-1),
                                    v["resolution"]["answered"]["exact_node"]
                                        .as_bool()
                                        .unwrap_or(false),
                                    c["served_from"].as_str() == Some("hot-tile-cache"),
                                    c["lock"]["holds"].as_u64().unwrap_or(0),
                                    c["lock"]["hold_ms_total"].as_f64().unwrap_or(0.0),
                                    c["lock"]["hold_ms_max"].as_f64().unwrap_or(0.0),
                                    v["shadow"]["search"]["chunks"].as_u64().unwrap_or(0),
                                    c["lock"]["hold_cpu_ms_total"].as_f64().unwrap_or(0.0),
                                    c["lock"]["yielded_ms"].as_f64().unwrap_or(0.0),
                                ));
                            }
                            let hm = c["lock"]["hold_ms_max"].as_f64().unwrap_or(0.0);
                            let mut w = hold_max.lock().unwrap();
                            *w = w.max(hm);
                        }
                    })
                })
                .collect();
            for r in readers {
                r.join().unwrap();
            }
            done.store(true, Ordering::Relaxed);
        });
        let wall_s = wall.elapsed().as_secs_f64();
        let after = snap();
        let mut ph = load_phase.into_inner().unwrap();
        let (max_wait, max_lag) = ph.report(&format!(
            "pass {pass} ({} tiles, {READERS} readers, {wall_s:.2} s wall)",
            addrs.len()
        ));
        eprintln!(
            "T-1021 pass {pass}: longest single history-lock hold by a tile request {:.1} ms; \
             view rows folded {}, view rows dropped {}, gate waits {}, ring samples lost {}",
            *hold_max.lock().unwrap(),
            after.3 - before.3,
            after.0 - before.0,
            after.1 - before.1,
            after.2 - before.2
        );
        passes.push((max_wait, max_lag, before, after));
    }
    stop.store(true, Ordering::Relaxed);
    pacer.join().unwrap();

    let mut base = base_phase.into_inner().unwrap();
    let (base_wait, base_lag) = base.report("baseline (no tile reads, 5 s)");
    eprintln!(
        "T-1021 baseline: view rows folded {}, dropped {}, gate waits {}, ring samples lost {}",
        base_end.3 - base_snap.3,
        base_end.0 - base_snap.0,
        base_end.1 - base_snap.1,
        base_end.2 - base_snap.2
    );

    // Per-request, per level: the route's own figures.
    let rows = results.into_inner().unwrap();
    for &(lf, lt) in &levels {
        let mut b: Vec<f64> = Vec::new();
        let mut hmax: Vec<f64> = Vec::new();
        let mut htot: Vec<f64> = Vec::new();
        let mut hcpu: Vec<f64> = Vec::new();
        let mut yl: Vec<f64> = Vec::new();
        let (mut n, mut exact, mut cached, mut chunks, mut cells) = (0, 0, 0, 0u64, 0u64);
        let (mut holds, mut shadow) = (0u64, 0u64);
        let mut answered = std::collections::BTreeSet::new();
        for r in rows.iter().filter(|r| (r.0, r.1) == (lf, lt)) {
            n += 1;
            b.push(r.2);
            hmax.push(r.10);
            htot.push(r.9);
            hcpu.push(r.12);
            yl.push(r.13);
            holds = holds.max(r.8);
            shadow = shadow.max(r.11);
            chunks = chunks.max(r.3);
            cells = cells.max(r.4);
            answered.insert(r.5);
            exact += usize::from(r.6);
            cached += usize::from(r.7);
        }
        if n == 0 {
            continue;
        }
        eprintln!(
            "T-1021 tiles ({lf},{lt}): n {n}, build_ms p50 {:.1} max {:.1}; lock holds/request \
             max {holds} (read chunks max {chunks}, shadow steps max {shadow}); hold ms per \
             request total p50 {:.1} max {:.1} (of it on CPU p50 {:.1} max {:.1}), single hold \
             p50 {:.1} max {:.1}; yielded ms p50 {:.1} max {:.1}; source_cells \
             max {cells}; answered levels {answered:?}; exact_node {exact}/{n}; cache hits \
             {cached}",
            pct(&mut b, 0.5),
            pct(&mut b, 1.0),
            pct(&mut htot, 0.5),
            pct(&mut htot, 1.0),
            pct(&mut hcpu, 0.5),
            pct(&mut hcpu, 1.0),
            pct(&mut hmax, 0.5),
            pct(&mut hmax, 1.0),
            pct(&mut yl, 0.5),
            pct(&mut yl, 1.0),
        );
    }

    // ---- the bounds ----
    //
    // Nothing the capture side owns stalls: no lossless-gate hold, no ring overrun, and the view
    // writer's queue (1500 rows) never dropped the growing edge.
    for (k, (_, _, before, after)) in passes.iter().enumerate() {
        assert_eq!(after.0 - before.0, 0, "pass {k}: view rows dropped");
        assert_eq!(after.1 - before.1, 0, "pass {k}: capture gated by a reader");
        assert_eq!(after.2 - before.2, 0, "pass {k}: ring samples lost");
    }
    let worst = passes.iter().map(|p| p.0).fold(0.0, f64::max);
    let worst_lag = passes.iter().map(|p| p.1).fold(0.0, f64::max);
    eprintln!(
        "T-1021 summary: worst view-mutex wait under load {worst:.1} ms (baseline \
         {base_wait:.1}); worst live-edge lag under load {worst_lag:.0} ms (baseline \
         {base_lag:.0})"
    );
    // The live edge keeps up with capture while a viewport is read: within LAG_BOUND of the
    // baseline's own worst. Live is never gated on tile work (CLAUDE.md's live-rendering
    // invariant); before the yield this was ~2 s on a quiet dev Mac and 14-31 s on a loaded one.
    assert!(
        worst_lag < base_lag + LAG_BOUND_MS,
        "the view lattice's live edge fell {worst_lag:.0} ms behind capture under a tile viewport \
         (baseline {base_lag:.0} ms)"
    );
    ctl.finish();
    ctl.run_free();
    let (summary, fired) = wait_guarded(handle, LIMIT);
    assert!(!fired, "the run had to be stopped by the watchdog");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
}
