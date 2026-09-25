//! What one `/api/tiles` answer **costs**, measured on the wire and on the clock (T-467, T-461).
//!
//! # What each number here is a property of
//!
//! - **`body_bytes`** is `serde_json::to_string(&tiles_json(..)).len()` — the exact bytes the HTTP
//!   layer would write, uncompressed, with no `Accept-Encoding` negotiated. It is a property of the
//!   *serialised answer*, not of an HTTP round trip: there is no socket, no header and no gzip in
//!   it. The demo backend's measured 19.34 MB body was the same quantity read off the wire, so the
//!   two are comparable in kind.
//! - **`build_ms`** is wall clock around `tiles_json` on **this** machine, in a **test** profile
//!   binary (not release), including the pyramid read, the coverage computation *and* the JSON
//!   value construction, but **excluding** `to_string`. The route's own `cost.build_ms` measures a
//!   strictly smaller interval (it stops before `coverage` is inserted), so this figure is the
//!   larger, more honest one and is not directly comparable to the demo's 134/157 ms — that was a
//!   release binary reporting its own narrower instrument. **Compare before-and-after within this
//!   file**, which is what it exists for.
//! - **the per-key decomposition** is `to_string` of each top-level member. It sums to slightly
//!   less than `body_bytes` (the object's own braces, keys and commas are not attributed).
//!
//! The fixture is deliberately two tiles at the **same** address shape over the **same** window:
//! one over the band the fixture's front end observed, one over a band it never tuned. That is the
//! comparison T-461 is about — an empty tile costing 86 % of an observed one — and it is measured
//! here rather than asserted from the shape of the code.

use std::sync::{Arc, Mutex};

use hk_api::http::ApiState;
use hk_model::{PowerUnit, Timestamp};
use hk_store::history::{FrameInput, PyramidConfig};
use serde_json::Value;

/// The canvas's own tile unit (`docs/16` §6.2), which is the size every measured figure in the
/// assessment was taken at.
const CELLS: usize = 256;
/// Tile index on the time axis at `level_t = 0`, so the tile's extent is 256 s ending near now.
const T_INDEX: i64 = 27_958_762 * 64 / 256;
/// Tile index on the frequency axis at `level_f = 0`: 6.25 kHz × 256 = 1.6 MHz per tile.
const F_INDEX: i64 = 1_116 * 64 / 256;
/// A frequency tile the fixture's front end never tuned to — 1 000 tiles away, 1.6 GHz off.
const F_INDEX_UNOBSERVED: i64 = F_INDEX + 1_000;

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "hk-api-tile-cost-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The front end the fixture's records name, so coverage is device-local rather than `Unknown`.
const DEVICE: &str = "hackrf:0000000000000000a06063c8234e925f";

/// One dwell over `lo..hi` for `window`, by [`DEVICE`] — the record that makes the observed band
/// *observed* and, just as importantly, puts the record horizon **before** the tile so the
/// never-tuned band reads `unobserved` rather than `unknown`.
fn dwell(
    lo: f64,
    hi: f64,
    t0_ns: i64,
    t1_ns: i64,
) -> hk_model::attention::observation::ObservationRecord {
    use hk_model::attention::baseline::SiteKey;
    use hk_model::attention::observation::{
        DwellRecord, ObservationRecord, ObservedWindow, Reason, Tier,
    };
    let w = hk_model::TimeRange::new(
        Timestamp::from_unix_nanos(t0_ns),
        Timestamp::from_unix_nanos(t1_ns),
    );
    ObservationRecord::Dwell(DwellRecord {
        schema: hk_model::attention::ATTENTION_SCHEMA_VERSION,
        survey_id: None,
        seq: 1,
        plan_version: 1,
        site: SiteKey::Unassigned,
        device_id: Some(DEVICE.to_string()),
        reason: Reason::RegionDwell { hop: 0 },
        tier: Tier::ScheduledPlan,
        window: ObservedWindow {
            center_hz: (lo + hi) / 2.0,
            sample_rate_hz: hi - lo,
            usable: hk_model::FreqRange::new(lo, hi),
            dc_excluded: None,
            rbw_hz: 1e3,
        },
        rf_path: 0,
        planned: w,
        observed: w,
        preempted: false,
        dropped_samples: 0,
        overload: false,
        provenance_ref: None,
    })
}

/// A server holding: a pyramid with one carrier over the observed tile's band for the whole tile
/// window, and an observation log saying that band — and only that band — was tuned.
fn fixture(dir: &std::path::Path) -> ApiState {
    let mut p = hk_store::Pyramid::open(dir.join("history"), PyramidConfig::default()).unwrap();
    let g = p.geometry().clone();
    let t_cell = g.levels[0].t_cell_ns;
    let f_cell = g.levels[0].f_cell_hz;
    let t0 = T_INDEX * t_cell * CELLS as i64;
    let f_lo = F_INDEX as f64 * f_cell * CELLS as f64;
    let f_hi = f_lo + f_cell * CELLS as f64;
    const NB: usize = 256;
    let bin_hz = (f_hi - f_lo) / NB as f64;
    for k in 0..CELLS as i64 {
        let mut psd = [1e-12f32; NB];
        psd[40] = 1e-6;
        psd[(k as usize) % NB] = 1e-7;
        p.ingest(&FrameInput::new(
            Timestamp::from_unix_nanos(t0 + k * t_cell),
            t_cell,
            f_lo,
            bin_hz,
            PowerUnit::Dbfs,
            &psd,
        ))
        .unwrap();
    }
    let obs = hk_store::observation::ObservationStore::open(
        hk_store::observation::ObservationLogConfig::new(dir.join("observations")),
    )
    .unwrap();
    obs.append(&dwell(f_lo, f_hi, t0, t0 + t_cell * CELLS as i64));
    obs.flush();
    ApiState {
        history: Some(Arc::new(Mutex::new(p))),
        observations: Some(obs),
        ..ApiState::default()
    }
}

fn params(f_index: i64, device: &str) -> Vec<(String, String)> {
    [
        ("level_f", "0".to_string()),
        ("level_t", "0".to_string()),
        ("f_index", f_index.to_string()),
        ("t_index", T_INDEX.to_string()),
        ("cells", CELLS.to_string()),
        ("device", device.to_string()),
    ]
    .into_iter()
    .map(|(a, b)| (a.to_string(), b))
    .collect()
}

/// Times one address `n` times and reports the best (least-noisy) build, the body size, and where
/// the bytes went.
fn measure(state: &ApiState, label: &str, f_index: i64, device: &str) -> (f64, usize, Value) {
    let q = params(f_index, device);
    // One warm-up so the measurement is not paying for first-touch page faults in the store.
    let _ = hk_api::tiles::tiles_json(state, &q).unwrap();
    let n = 5;
    let mut best = f64::INFINITY;
    let mut last = Value::Null;
    for _ in 0..n {
        let started = std::time::Instant::now();
        let v = hk_api::tiles::tiles_json(state, &q).unwrap();
        best = best.min(started.elapsed().as_secs_f64() * 1e3);
        last = v;
    }
    let body = serde_json::to_string(&last).unwrap();
    let mut parts: Vec<(String, usize)> = last
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::to_string(v).unwrap().len()))
        .collect();
    parts.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    eprintln!(
        "\n=== {label}: {CELLS}x{CELLS} tile, device={device} ===\n  build_ms (best of {n}) = {best:.2}\n  body_bytes            = {} ({:.3} MB)",
        body.len(),
        body.len() as f64 / 1e6
    );
    for (k, n) in &parts {
        eprintln!(
            "    {k:<12} {n:>10} B  ({:>5.1} %)",
            *n as f64 * 100.0 / body.len() as f64
        );
    }
    if let Some(c) = last.get("coverage").and_then(Value::as_object) {
        for (k, v) in c {
            let n = serde_json::to_string(v).unwrap().len();
            if n > 512 {
                eprintln!("      coverage.{k:<10} {n:>10} B");
            }
        }
    }
    // The route's OWN instrument, which stops before `coverage` is built and inserted — so it is a
    // strictly smaller interval than `build_ms` above, and it is the quantity the assessment's
    // 134 ms / 157 ms were. Both are printed so neither can be mistaken for the other.
    eprintln!(
        "  cost.build_ms (route) = {}\n  cost.source_cells     = {}\n  grid.observed_cells   = {}",
        last["cost"]["build_ms"], last["cost"]["source_cells"], last["grid"]["observed_cells"]
    );
    (best, body.len(), last)
}

/// **The measurement.** Two tiles, same shape, same window: one over the band the fixture observed,
/// one over a band it never tuned. Printed, not asserted — the assertions that guard behaviour live
/// in `hk-api`'s own tests; this exists so the before/after of T-467 and T-461 is a number taken
/// the same way twice.
#[test]
fn a_tile_answers_cost_is_measured_on_the_wire_and_on_the_clock() {
    let dir = TempDir::new("cost");
    let state = fixture(&dir.0);

    let (obs_ms, obs_bytes, obs) = measure(&state, "OBSERVED band", F_INDEX, "any");
    let (emp_ms, emp_bytes, emp) = measure(&state, "NEVER-TUNED band", F_INDEX_UNOBSERVED, "any");

    eprintln!(
        "\n=== summary ===\n  observed : {obs_ms:>8.2} ms  {obs_bytes:>9} B\n  empty    : {emp_ms:>8.2} ms  {emp_bytes:>9} B  ({:.0} % of the observed build, {:.0} % of its bytes)\n",
        emp_ms * 100.0 / obs_ms,
        emp_bytes as f64 * 100.0 / obs_bytes as f64
    );

    // The fixture must actually be what it claims, or every number above is about something else.
    assert!(
        obs["grid"]["observed_cells"].as_u64().unwrap() > 0,
        "the observed tile must hold measurements: {}",
        obs["grid"]["observed_cells"]
    );
    assert_eq!(
        emp["grid"]["observed_cells"],
        serde_json::json!(0),
        "the never-tuned tile must hold none"
    );
}

// ——— T-523: what the SHADOW search costs on the coverage short-circuit ———————————————————————
//
// T-461 made a tile whose coverage plane is `unobserved` end to end cost the coverage rasterisation
// and nothing else — no pyramid read at all. T-519 then hung the last-known ("shadow") search off
// that same path, because a tile the coverage map greys end to end is exactly where a departed
// band's shadow lives. The search is real work, and it was given the **tile read's own** budget
// (`TILE_MAX_TOTAL_SOURCE_CELLS`, 2 000 000 source cells), so the cheapest answer the route has
// became one of its more expensive ones. The user saw it as 502s from the tunnel during a zoom.
//
// This measures it the way the user provokes it: the set of addresses one zoom burst asks for —
// the same place at every frequency level, coarse to fine — over a **departed band**, which is the
// shape that makes the search do work rather than return immediately.

/// A server with a departed band: history and a dwell over `B` for one tile window, then a dwell
/// over a distant band for the NEXT window — so the record horizon reaches past the tile under
/// test while `B` itself reads `unobserved` there. That is T-519's case, and the only shape in
/// which the short-circuit and a non-trivial shadow search happen together.
fn departed_band_fixture(dir: &std::path::Path) -> (ApiState, f64, i64) {
    let state = fixture(dir);
    let (t_cell, f_cell) = {
        let p = state.history.as_ref().unwrap().lock().unwrap();
        let g = p.geometry();
        (g.levels[0].t_cell_ns, g.levels[0].f_cell_hz)
    };
    let t0 = T_INDEX * t_cell * CELLS as i64;
    let f_lo = F_INDEX as f64 * f_cell * CELLS as f64;
    let window = t_cell * CELLS as i64;
    // The radio moved on: for the NEXT window it is parked at 5 GHz — far outside every tile this
    // test asks for (they all start at 0 Hz and are at most 819.2 MHz wide), so it moves the record
    // horizon and the store's newest frame past the tile under test without making any of those
    // tiles partly observed. Both halves matter: without the record the tile reads `unknown` and
    // never short-circuits; without the frames the tile is at or after the store's newest frame and
    // carries no shadow run by definition.
    const AWAY_LO: f64 = 5.0e9;
    const NB: usize = 64;
    {
        let mut p = state.history.as_ref().unwrap().lock().unwrap();
        for k in 0..CELLS as i64 {
            let mut psd = [1e-12f32; NB];
            psd[7] = 1e-6;
            p.ingest(&FrameInput::new(
                Timestamp::from_unix_nanos(t0 + window + k * t_cell),
                t_cell,
                AWAY_LO,
                f_cell,
                PowerUnit::Dbfs,
                &psd,
            ))
            .unwrap();
        }
    }
    let obs = state.observations.as_ref().unwrap();
    obs.append(&dwell(
        AWAY_LO,
        AWAY_LO + f_cell * NB as f64,
        t0 + window,
        t0 + 2 * window,
    ));
    obs.flush();
    (state, f_lo, t0 + window)
}

/// One `/api/tiles` answer at `(level_f, level_t)` covering `f_hz` at `t_ns`, timed best-of-`n`,
/// with the shadow search's own reported cost pulled out of the body.
fn shadow_cost(state: &ApiState, level_f: u32, f_hz: f64, t_ns: i64) -> (f64, Value) {
    let q = |f_index: i64, t_index: i64| -> Vec<(String, String)> {
        [
            ("level_f", level_f.to_string()),
            ("level_t", "0".to_string()),
            ("f_index", f_index.to_string()),
            ("t_index", t_index.to_string()),
            ("cells", CELLS.to_string()),
            ("device", "any".to_string()),
        ]
        .into_iter()
        .map(|(a, b)| (a.to_string(), b))
        .collect()
    };
    // The tile's own extent is the only honest way to turn a frequency and a time into indices:
    // the lattice's cell size per level is the geometry's business, not this test's.
    let probe = hk_api::tiles::tiles_json(state, &q(0, 0)).unwrap();
    let w_hz =
        probe["extent"]["f_hi_hz"].as_f64().unwrap() - probe["extent"]["f_lo_hz"].as_f64().unwrap();
    let w_s = probe["extent"]["t1_s"].as_f64().unwrap() - probe["extent"]["t0_s"].as_f64().unwrap();
    let query = q(
        (f_hz / w_hz).floor() as i64,
        (t_ns as f64 / 1e9 / w_s).floor() as i64,
    );
    let _ = hk_api::tiles::tiles_json(state, &query).unwrap();
    let n = 5;
    let mut best = f64::INFINITY;
    let mut last = Value::Null;
    for _ in 0..n {
        let started = std::time::Instant::now();
        let v = hk_api::tiles::tiles_json(state, &query).unwrap();
        best = best.min(started.elapsed().as_secs_f64() * 1e3);
        last = v;
    }
    (best, last)
}

/// **The measurement T-523 asked for**: per-tile cost across a zoom burst's addresses, with the
/// shadow search's share of it named. Printed, not thresholded on absolute milliseconds — this
/// machine is not the user's — but the *share* is asserted, because "the short-circuit is the cheap
/// path" is the property T-461 bought and T-519 spent.
#[test]
fn the_shadow_search_cost_on_the_coverage_short_circuit_is_measured_across_a_zoom_burst() {
    let dir = TempDir::new("shadow-cost");
    let (state, f_lo, t_ns) = departed_band_fixture(&dir.0);
    eprintln!(
        "\n=== T-523: a zoom burst's tiles over a DEPARTED band (coverage short-circuit) ===\n\
         {:>7}  {:>12}  {:>11}  {:>11}  {:>9}  {:>8}  {:>8}  {:>6}  {:>5}",
        "level_f",
        "tile width",
        "route ms",
        "shadow ms",
        "src cells",
        "own-lvl",
        "ladder",
        "chunks",
        "runs"
    );
    let mut worst: f64 = 0.0;
    let mut worst_level = 0;
    let mut short_circuited = 0;
    let mut shadowless = Vec::new();
    let mut overspent = Vec::new();
    for level_f in 0..=9u32 {
        let (ms, v) = shadow_cost(&state, level_f, f_lo, t_ns);
        let applied = v["resolution"]["short_circuit"]["applied"] == Value::Bool(true);
        if applied {
            short_circuited += 1;
        }
        let sh = &v["shadow"]["search"];
        let sh_ms = sh["build_ms"].as_f64().unwrap_or(0.0);
        // T-916: WHICH search spent the cells. The own-level search (T-911) runs first and the
        // ladder gets only what it leaves, so the two must be named apart or a regression that let
        // both run a full pass would show up only as a slightly slower total.
        let own_cells = sh["own_level"]["source_cells"].as_u64().unwrap_or(0);
        let width_mhz = (v["extent"]["f_hi_hz"].as_f64().unwrap()
            - v["extent"]["f_lo_hz"].as_f64().unwrap())
            / 1e6;
        let total_cells = sh["source_cells"].as_u64().unwrap_or(0);
        let ladder_cells = total_cells.saturating_sub(own_cells);
        let (chunks, runs) = (
            sh["chunks"].as_u64().unwrap_or(0),
            v["shadow"]["runs"].as_u64().unwrap_or(0),
        );
        let tail = if applied { "" } else { "   (no short-circuit)" };
        eprintln!(
            "{level_f:>7}  {width_mhz:>8.1} MHz  {ms:>8.2} ms  {sh_ms:>8.2} ms  {total_cells:>9}  {own_cells:>8}  {ladder_cells:>8}  {chunks:>6}  {runs:>5}{tail}"
        );
        if applied && sh_ms > worst {
            worst = sh_ms;
            worst_level = level_f;
        }
        if applied && v["shadow"]["runs"].as_u64() == Some(0) {
            shadowless.push(level_f);
        }
        // T-911 review: the bound is on what was READ, not only what is stated. The own-level
        // search and the ladder share one budget; a ladder given a second whole budget read
        // 152 576 / 217 088 / 229 376 cells at level_f 1 / 3 / 9 under a stated 131 072.
        let (read, max) = (
            sh["source_cells"].as_u64().expect("source_cells"),
            sh["max_source_cells"].as_u64().expect("max_source_cells"),
        );
        if read > max {
            overspent.push((level_f, read, max));
        }
    }
    assert!(
        overspent.is_empty(),
        "the shadow search READ more source cells than its stated budget at (level_f, read, max) \
         {overspent:?}: T-523's bound is on the whole search"
    );
    eprintln!(
        "  worst short-circuited shadow search: {worst:.2} ms at level_f {worst_level}\n\
         budget now {} source cells (cells x 512 rows), was {} — the tile read's own, which is what T-519\n\
         shipped. Measured on this machine at that budget, same fixture: 117.97 ms on the 819.2 MHz\n\
         tile and 225.18 ms at level_f 7, against 10.70 ms and 13.28 ms here, with the SAME runs per\n\
         level. The user met the difference as 502s from the tunnel during a zoom.\n",
        hk_api::tiles::TILE_MAX_SHADOW_SOURCE_CELLS,
        hk_api::tiles::TILE_MAX_TOTAL_SOURCE_CELLS
    );
    assert!(
        short_circuited >= 8,
        "only {short_circuited} of 10 levels took the coverage short-circuit — the fixture is not \
         the departed-band shape this measures, so every number above is about something else"
    );
    // **The budget must not have bought the speed by deleting the shadow.** Every one of these tiles
    // contains the departed band, so every one of them owes at least one run; the number of runs is
    // the number of the tile's columns the band covers, and it was identical before and after the
    // cut (256, 128, 64, 32, 16, 8, 4, 2, 1, 1 down the levels). A budget is paid in the shadow's
    // resolution, never in its reach, and this is the assertion that holds that claim to it.
    assert!(
        shadowless.is_empty(),
        "level_f {shadowless:?} short-circuited but carried NO shadow run, though the departed band \
         is inside every tile here: the budget cut reach, not resolution"
    );
    // The budget, not the clock: a machine-independent statement of the same fact. The shadow may
    // not read more source cells than the tile read it replaced was ever allowed. Read off the
    // wire rather than off the constants, so it is the number the route actually searched under.
    let (_, widest) = shadow_cost(&state, 9, f_lo, t_ns);
    let searched = widest["shadow"]["search"]["max_source_cells"]
        .as_u64()
        .expect("the shadow search must state its budget on the wire");
    assert!(
        searched < hk_api::tiles::TILE_MAX_TOTAL_SOURCE_CELLS as u64,
        "the shadow search on a short-circuited tile ran under {searched} source cells — as \
         generously as the full pyramid read the short-circuit exists to avoid"
    );
    assert_eq!(
        searched,
        (CELLS * hk_api::tiles::SHADOW_SEARCH_ROWS) as u64,
        "the budget is the TILE's scale: cells x SHADOW_SEARCH_ROWS (T-523)"
    );
}

// ——— T-916: the case where the LADDER answers, what it is labelled, and what the two searches cost —
//
// T-911 put an own-level search in front of the ladder so a recently-departed band's shadow is the
// very cell its last live row was drawn with. That search is deliberately SHORT-REACHED
// (`hk_store::history::PINNED_REACH_BLOCKS` = 256 blocks of its own level, ~4 h at scheme 1's level
// 0): the time a band spent unobserved costs no cell read to cross, but the index scan that crosses
// it is bounded. A band that departed longer ago than that reach still gets a shadow — from the
// ladder, whose cells are coarser in both axes, so its max-hold reads at or HOTTER than the row the
// band was last live on. Two things follow, and both are measured here rather than assumed:
//
//   1. **The answer must SAY which search found each run**, or the client cannot state the
//      resolution it drew (the pane readout does, T-916). `sources[].search` is that label.
//   2. **Two searches must still cost one budget** (T-911's review blocker 1). This is the shape
//      where they BOTH run — the own-level search spends cells and finds nothing, then the ladder
//      answers on the remainder — so it is the worst case for T-523's bound, and the one the
//      zoom-burst fixture above never exercises.

/// The mirror of [`departed_band_fixture`] with the departure pushed **past the own-level reach**:
/// band `B` is recorded for one window, the radio then sits at 5 GHz for hours, and the tile under
/// test is `GAP_WINDOWS` tile windows later. Returns the state, `B`'s low edge and the tile's start.
fn stale_departed_band_fixture(dir: &std::path::Path) -> (ApiState, f64, i64) {
    let state = fixture(dir);
    let (t_cell, f_cell) = {
        let p = state.history.as_ref().unwrap().lock().unwrap();
        let g = p.geometry();
        (g.levels[0].t_cell_ns, g.levels[0].f_cell_hz)
    };
    let t0 = T_INDEX * t_cell * CELLS as i64;
    let f_lo = F_INDEX as f64 * f_cell * CELLS as f64;
    let window = t_cell * CELLS as i64;
    // 100 tile windows = 25 600 s at scheme 1's level 0, against a pinned reach of 256 blocks of
    // 60 s = 15 360 s. Comfortably past it, and still a plain integer number of tile addresses so
    // the tile under test is a whole `t_index` rather than a straddle.
    const GAP_WINDOWS: i64 = 100;
    const AWAY_LO: f64 = 5.0e9;
    const NB: usize = 64;
    let tile_t0 = t0 + window + GAP_WINDOWS * window;
    {
        let mut p = state.history.as_ref().unwrap().lock().unwrap();
        for k in 0..CELLS as i64 {
            let mut psd = [1e-12f32; NB];
            psd[7] = 1e-6;
            p.ingest(&FrameInput::new(
                Timestamp::from_unix_nanos(tile_t0 + k * t_cell),
                t_cell,
                AWAY_LO,
                f_cell,
                PowerUnit::Dbfs,
                &psd,
            ))
            .unwrap();
        }
    }
    // ONE record for the whole time away, gap included: the radio was elsewhere the entire time, so
    // `B` reads `unobserved` over the tile and the record horizon reaches past it.
    let obs = state.observations.as_ref().unwrap();
    obs.append(&dwell(
        AWAY_LO,
        AWAY_LO + f_cell * NB as f64,
        t0 + window,
        tile_t0 + window,
    ));
    obs.flush();
    (state, f_lo, tile_t0)
}

/// **T-916 (1) and (3).** A departure older than the own-level reach is answered by the ladder, the
/// answer **says so per run**, and the two searches together stay inside the one T-523 budget.
#[test]
fn a_departure_older_than_the_own_level_reach_is_labelled_ladder_and_costs_one_budget() {
    let dir = TempDir::new("stale-shadow");
    let (state, f_lo, t_ns) = stale_departed_band_fixture(&dir.0);
    let (ms, v) = shadow_cost(&state, 0, f_lo, t_ns);
    let sh = &v["shadow"];
    let search = &sh["search"];
    assert!(
        sh["runs"].as_u64().unwrap_or(0) > 0,
        "a band this far past the own-level reach must still carry a shadow — the ladder is what \
         reaches it, and grey would be the lie T-881 closed: {sh}"
    );
    // Every value carried in from before the tile, and where it came from. `src = 0` is the tile's
    // own grid (the carry / backward fill), which makes no claim about an older cell.
    let sources = sh["sources"].as_array().expect("a source table");
    let carried: Vec<&Value> = sh["src"]
        .as_array()
        .expect("per-run source indices")
        .iter()
        .filter_map(|i| i.as_u64())
        .filter(|&i| i > 0)
        .map(|i| &sources[i as usize])
        .collect();
    assert!(
        !carried.is_empty(),
        "no run came from before the tile: {sh}"
    );
    for s in &carried {
        assert_eq!(
            s["search"],
            serde_json::json!("ladder"),
            "past the own-level reach only the ladder can answer, and the run must say so: {s}"
        );
        assert!(
            s["f_cell_hz"].as_f64().unwrap_or(0.0) > 0.0
                && s["t_cell_s"].as_f64().unwrap_or(0.0) > 0.0,
            "a ladder-sourced run must state the cell it was measured over — it is the number the \
             client tells the user, and it is coarser than this tile's own: {s}"
        );
    }
    // The own-level search ran and found nothing: that is what makes this the two-search case.
    let own = &search["own_level"];
    assert!(
        !own.is_null(),
        "the own-level search must have been attempted: {search}"
    );
    assert_eq!(
        own["columns_used"],
        serde_json::json!(0),
        "own-level found nothing here: {own}"
    );
    let (total, own_cells, max) = (
        search["source_cells"].as_u64().expect("source_cells"),
        own["source_cells"].as_u64().unwrap_or(0),
        search["max_source_cells"]
            .as_u64()
            .expect("max_source_cells"),
    );
    eprintln!(
        "\n=== T-916: a departure PAST the own-level reach (the ladder's case) ===\n\
         route {ms:.2} ms, shadow {:.2} ms, {} runs\n\
         source cells: {total} of {max} allowed — own-level {own_cells}, ladder {}\n\
         carried runs' source cell: {} Hz x {} s (this tile's own is {} Hz x {} s)\n",
        search["build_ms"].as_f64().unwrap_or(0.0),
        sh["runs"],
        total.saturating_sub(own_cells),
        carried[0]["f_cell_hz"],
        carried[0]["t_cell_s"],
        v["axes"]["frequency"]["cell_hz"],
        v["axes"]["time"]["cell_s"],
    );
    assert!(
        total <= max,
        "the own-level search and the ladder read {total} source cells under a stated budget of \
         {max}: T-523's bound is on the WHOLE search, and this is the shape where both run"
    );
    assert!(
        own_cells < total,
        "the ladder read nothing ({own_cells} of {total}) though every run says it answered: the \
         accounting and the labels disagree"
    );
}

// ——— T-527: what the BACKWARD fill costs, and how often its case actually occurs ————————————————
//
// T-519 carried a column's last-known value FORWARD. That leaves a column first seen part-way down
// the view grey ABOVE its samples, although it was observed — the gap T-527 fills with the
// column's first-ever sample. Two things want measuring, in the order that decides whether the
// ticket was worth building:
//
//   1. **Does the case occur?** A head gap needs a column whose record BEGINS inside the viewed
//      window and has nothing older: the start of a capture, or a retune onto new spectrum. Where a
//      band was tuned for the whole window there is no gap to fill, and the count is zero. So the
//      number below is per address, not a global claim.
//   2. **What does it cost?** Nothing on the coverage short-circuit (that path passes no grid, so
//      there is no first-ever sample to read back from — T-523's budgeted search is untouched), and
//      on the full path only the row walk `carry_forward` already makes over the tile's own cells.

/// The mirror of [`departed_band_fixture`]: the radio is parked at 5 GHz for one tile window and
/// only THEN arrives on `B`, so `B`'s record begins half-way down the tile under test and nothing
/// older than it exists. That is the only shape in which a head gap can occur at all.
fn arrived_band_fixture(dir: &std::path::Path) -> (ApiState, f64, i64) {
    let mut p = hk_store::Pyramid::open(dir.join("history"), PyramidConfig::default()).unwrap();
    let g = p.geometry().clone();
    let (t_cell, f_cell) = (g.levels[0].t_cell_ns, g.levels[0].f_cell_hz);
    let t0 = T_INDEX * t_cell * CELLS as i64;
    let f_lo = F_INDEX as f64 * f_cell * CELLS as f64;
    let f_hi = f_lo + f_cell * CELLS as f64;
    let window = t_cell * CELLS as i64;
    const AWAY_LO: f64 = 5.0e9;
    const NB: usize = 256;
    let bin_hz = (f_hi - f_lo) / NB as f64;
    // Window 1: 5 GHz only. Window 2: its first half at 5 GHz, then B for the rest — so the tile
    // over window 2 holds B from row CELLS/2 down, and nothing above it.
    for k in 0..2 * CELLS as i64 {
        let mut away = [1e-12f32; 64];
        away[7] = 1e-6;
        p.ingest(&FrameInput::new(
            Timestamp::from_unix_nanos(t0 + k * t_cell),
            t_cell,
            AWAY_LO,
            f_cell,
            PowerUnit::Dbfs,
            &away,
        ))
        .unwrap();
        if k >= CELLS as i64 + CELLS as i64 / 2 {
            let mut psd = [1e-12f32; NB];
            psd[40] = 1e-6;
            psd[(k as usize) % NB] = 1e-7;
            p.ingest(&FrameInput::new(
                Timestamp::from_unix_nanos(t0 + k * t_cell),
                t_cell,
                f_lo,
                bin_hz,
                PowerUnit::Dbfs,
                &psd,
            ))
            .unwrap();
        }
    }
    let obs = hk_store::observation::ObservationStore::open(
        hk_store::observation::ObservationLogConfig::new(dir.join("observations")),
    )
    .unwrap();
    obs.append(&dwell(
        AWAY_LO,
        AWAY_LO + f_cell * 64.0,
        t0,
        t0 + 2 * window,
    ));
    obs.append(&dwell(
        f_lo,
        f_hi,
        t0 + window + window / 2,
        t0 + 2 * window,
    ));
    obs.flush();
    (
        ApiState {
            history: Some(Arc::new(Mutex::new(p))),
            observations: Some(obs),
            ..ApiState::default()
        },
        f_lo,
        t0 + window,
    )
}

/// **T-527's measurement**: per zoom-burst address over an arriving band, the cells the shadow now
/// fills that T-519 left grey, beside what the answer costs. Printed, not thresholded on absolute
/// milliseconds; the assertions are about the shape of the answer.
#[test]
fn the_backward_fill_is_measured_and_its_case_counted_across_a_zoom_burst() {
    let dir = TempDir::new("backward-fill");
    let (state, f_lo, t_ns) = arrived_band_fixture(&dir.0);
    eprintln!(
        "\n=== T-527: a zoom burst's tiles over an ARRIVING band (full path) ===\n\
         {:>7}  {:>11}  {:>9}  {:>10}  {:>6}  {:>9}  {:>12}  {:>10}",
        "level_f",
        "tile width",
        "route ms",
        "shadow ms",
        "runs",
        "backward",
        "cells filled",
        "still grey"
    );
    let mut total_backward = 0u64;
    let mut with_head_gap = 0;
    for level_f in 0..=9u32 {
        let (ms, v) = shadow_cost(&state, level_f, f_lo, t_ns);
        let sh = &v["shadow"];
        let n = v["extent"]["nf"].as_u64().unwrap() as usize;
        let back: Vec<usize> = (0..sh["runs"].as_u64().unwrap() as usize)
            .filter(|&i| sh["fill"][i].as_u64() == Some(1))
            .collect();
        let filled: u64 = back.iter().map(|&i| sh["rows"][i].as_u64().unwrap()).sum();
        // What T-519 would have left grey here is exactly the cells the backward runs cover: every
        // other run is a forward carry, which T-519 already drew.
        let grid = v["grid"]["max_db"].as_array();
        let unmeasured = grid.map_or(n * n, |g| g.iter().filter(|c| c.is_null()).count());
        let covered: u64 = (0..sh["runs"].as_u64().unwrap() as usize)
            .map(|i| sh["rows"][i].as_u64().unwrap())
            .sum();
        eprintln!(
            "{level_f:>7}  {:>8.1} MHz  {ms:>6.2} ms  {:>7.2} ms  {:>6}  {:>9}  {filled:>12}  {:>10}",
            (v["extent"]["f_hi_hz"].as_f64().unwrap() - v["extent"]["f_lo_hz"].as_f64().unwrap())
                / 1e6,
            sh["search"]["build_ms"].as_f64().unwrap_or(0.0),
            sh["runs"],
            back.len(),
            unmeasured as u64 - covered,
        );
        if !back.is_empty() {
            with_head_gap += 1;
        }
        total_backward += filled;
        // Every backward run starts at row 0 and stops where the column's first sample begins: it
        // is the HEAD's rule, never a second way to answer an ordinary gap.
        for &i in &back {
            assert_eq!(sh["row"][i], serde_json::json!(0), "run {i}: {sh}");
            let c = sh["f"][i].as_u64().unwrap() as usize;
            let r = sh["rows"][i].as_u64().unwrap() as usize;
            if let Some(g) = grid {
                assert!(g[(r - 1) * n + c].is_null(), "run {i} covers a measurement");
                assert!(
                    g[r * n + c].is_number(),
                    "run {i} must stop at the first sample it reads: {}",
                    g[r * n + c]
                );
            }
        }
    }
    eprintln!(
        "  {with_head_gap} of 10 addresses had a column whose record BEGINS inside the window; \
         {total_backward} cells now carry the first-ever sample that T-519 left grey.\n\
         The case is not universal and is not claimed to be: a band tuned for the whole window has \
         no head gap, and a column never observed at all still carries no run.\n"
    );
    assert!(
        with_head_gap > 0,
        "the fixture must actually arrive mid-window, or this measures nothing"
    );
    // Grey's meaning is unchanged: the band the radio never tuned carries no run at all, backward
    // or forward, on the same server.
    let (_, never) = shadow_cost(&state, 0, f_lo + 1.6e9, t_ns);
    assert_eq!(
        never["shadow"]["runs"],
        serde_json::json!(0),
        "a never-observed column carries NO run: {}",
        never["shadow"]
    );
    assert_eq!(never["shadow"]["backward_runs"], serde_json::json!(0));
}
