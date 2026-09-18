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
