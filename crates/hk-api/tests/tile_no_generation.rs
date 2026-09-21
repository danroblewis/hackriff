//! **T-571: `GET /api/tiles` generates NOTHING.** The R1 guard from the user's tile-performance
//! review (2026-09-21), at the route rather than in the store.
//!
//! The default canvas surface is `scheme=view`, and it used to be `coarse_on_demand`: the first
//! reader of any coarse node paid a fold of up to 1024 producer tiles **inside the request**
//! (~500 ms a tile, 5.2 s for a coarse map tile), and with 135–290 tiles a screen that is the
//! 50–110 s cold fill the user sees. T-571 maintains the coarse nodes live instead, so a tile
//! request is a tile read.
//!
//! # How this is asserted
//!
//! By the user's direct instruction (2026-09-21): **count assertions and a response-size cap,
//! never a wall-clock budget.** A wall-clock bound measures the machine, not the code — T-537's
//! 700 ms budget bought exactly one frame under load. `cost.build_ms` is printed here and tracked;
//! it is not a gate.
//!
//! # Why it cannot go vacuously green
//!
//! Every count is taken over a viewport that is first asserted to have **answered with data**, and
//! the same viewport is run against a second server holding the same frames under the old
//! `coarse_on_demand` lattice. The control is asserted to generate, so a run in which nothing was
//! requested — or in which both stores answered grey — fails before the guard is reached. The pair
//! is also the standing red-when-broken evidence.

use std::sync::{Arc, Mutex};

use hk_api::http::ApiState;
use hk_api::tiles::tiles_json;
use hk_model::{PowerUnit, Timestamp};
use hk_store::history::{FrameInput, HistogramConfig, PyramidConfig, ViewLattice};

struct TempDir(std::path::PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "hk-api-tile-nogen-{tag}-{}-{:?}",
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

/// The canvas's own tile unit (`docs/16` §6.2).
const CELLS: usize = 256;
/// A run long enough that the coarser time nodes have committed rows of their own.
const SECS: i64 = 2048;
/// 6.25 kHz cells × 1024 per block × 4 blocks: a 25.6 MHz "live edge".
const N_BINS: usize = 4096;
const F_CELL: f64 = 6250.0;
const F_LO: f64 = 102.4e6;
/// Aligned to every block boundary in this geometry.
const T0_NS: i64 = 1_789_300_800 * 1_000_000_000;

/// The shipped view lattice, rebuilt here because hk-api cannot depend on hk-pipeline.
fn view(live: bool) -> PyramidConfig {
    let base = PyramidConfig {
        f_cells_per_block: 1024,
        histogram: HistogramConfig {
            lo_db: -200.0,
            step_db: 5.0,
            bins: 44,
        },
        ..PyramidConfig::view_lattice(ViewLattice {
            scheme: 2,
            f_cell_hz: F_CELL,
            t_cell: std::time::Duration::from_secs(1),
            cells_per_block: 64,
            f_levels: 4,
            t_levels: 4,
        })
    };
    PyramidConfig {
        coarse_live: live,
        coarse_on_demand: !live,
        ..base
    }
}

fn server(dir: &std::path::Path, live: bool) -> ApiState {
    let mut p = hk_store::Pyramid::open(dir.join("view"), view(live)).unwrap();
    let mut x = 0x1234_5678_9ABC_DEF0u64;
    let mut psd = vec![0f32; N_BINS];
    for s in 0..SECS {
        for (b, v) in psd.iter_mut().enumerate() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let noise = -100.0 + (x % 1000) as f32 * 0.01;
            *v = 10f32.powf((noise + if b % 137 == 11 { 40.0 } else { 0.0 }) / 10.0);
        }
        p.ingest(&FrameInput::new(
            Timestamp::from_unix_nanos(T0_NS + s * 1_000_000_000),
            1_000_000_000,
            F_LO,
            F_CELL,
            PowerUnit::Dbfs,
            &psd,
        ))
        .unwrap();
    }
    p.seal_through(Timestamp::from_unix_nanos(T0_NS + SECS * 1_000_000_000))
        .unwrap();
    ApiState {
        view_history: Some(Arc::new(Mutex::new(p))),
        ..ApiState::default()
    }
}

/// The addresses one screen asks for: every `(level_f, level_t)` node over the run's own extent.
fn viewport() -> Vec<(usize, usize)> {
    (0..4)
        .flat_map(|lf| (0..4).map(move |lt| (lf, lt)))
        .collect()
}

fn params(lf: usize, lt: usize) -> Vec<(String, String)> {
    let f_cell = F_CELL * 2f64.powi(lf as i32);
    let t_cell_s = 2f64.powi(lt as i32);
    let t_mid = (T0_NS + SECS * 1_000_000_000 / 2) as f64 / 1e9;
    [
        ("scheme", "view".to_string()),
        ("level_f", lf.to_string()),
        ("level_t", lt.to_string()),
        (
            "f_index",
            (((F_LO + 1e6) / (f_cell * CELLS as f64)).floor() as i64).to_string(),
        ),
        (
            "t_index",
            ((t_mid / (t_cell_s * CELLS as f64)).floor() as i64).to_string(),
        ),
        ("cells", CELLS.to_string()),
    ]
    .into_iter()
    .map(|(a, b)| (a.to_string(), b))
    .collect()
}

/// `(answers with data, total body bytes, largest body, total build_ms)`.
fn sweep(state: &ApiState, label: &str) -> (usize, usize, usize, f64) {
    let (mut answered, mut total, mut largest, mut ms) = (0usize, 0usize, 0usize, 0.0);
    for (lf, lt) in viewport() {
        let v = tiles_json(state, &params(lf, lt)).expect("the viewport must be servable");
        let body = serde_json::to_string(&v).unwrap();
        total += body.len();
        largest = largest.max(body.len());
        ms += v["cost"]["build_ms"].as_f64().unwrap_or(0.0);
        if v["grid"]["observed_cells"].as_u64().unwrap_or(0) > 0 {
            answered += 1;
        }
    }
    eprintln!(
        "  {label}: {answered}/16 nodes answered with data, {total} B total, {largest} B largest, \
         build_ms total {ms:.1} (printed, never asserted)"
    );
    (answered, total, largest, ms)
}

fn store_counts(state: &ApiState) -> (u64, u64) {
    let p = state.view_history.as_ref().unwrap().lock().unwrap();
    let st = p.stats();
    (st.tiles_materialized, st.producer_tiles_folded)
}

#[test]
fn a_viewport_of_tile_requests_generates_nothing_and_stays_inside_the_response_cap() {
    // ——— the control: the on-demand lattice the route used to serve ———
    let ctl_dir = TempDir::new("ctl");
    let ctl = server(&ctl_dir.0, false);
    assert_eq!(
        store_counts(&ctl),
        (0, 0),
        "capture must not have built anything under the on-demand lattice"
    );
    let (ctl_answered, ..) = sweep(&ctl, "control (coarse_on_demand)");
    let (ctl_built, ctl_folded) = store_counts(&ctl);
    eprintln!("  control generated {ctl_built} tiles from {ctl_folded} producer tiles");
    assert!(
        ctl_answered > 1,
        "the control answered {ctl_answered} nodes with data: the fixture is not exercising the \
         coarse nodes, so the guard below would judge nothing"
    );
    assert!(
        ctl_built > 0 && ctl_folded > 0,
        "the control must generate on the read path, or this test proves nothing"
    );

    // ——— T-571: the same viewport, live ———
    let dir = TempDir::new("live");
    let state = server(&dir.0, true);
    let (answered, total, largest, _) = sweep(&state, "T-571 (coarse_live)");
    let (built, folded) = store_counts(&state);

    assert_eq!(
        answered, ctl_answered,
        "the live lattice must answer the same nodes with data as the fold did — fewer would mean \
         a node reads grey over data the store holds"
    );
    assert_eq!(built, 0, "the read path generated {built} tiles");
    assert_eq!(folded, 0, "the read path folded {folded} producer tiles");

    // **The response-size cap, both halves.**
    //
    // A *fully observed* 256 × 256 tile is genuinely 65 536 measured cells and serialises to a
    // couple of MB; the cap that matters there is the demo's measured 19.34 MB regression
    // (T-467), so the bound is 4 MiB — real headroom over a dense answer, and an order of
    // magnitude under the defect.
    const DENSE_CAP: usize = 4 << 20;
    assert!(
        largest <= DENSE_CAP,
        "a fully observed {CELLS}x{CELLS} tile answer was {largest} B, over the {DENSE_CAP} B cap"
    );
    assert!(
        total > 16 * 1024,
        "the whole viewport serialised to {total} B, which is too little to be 16 real answers"
    );

    // The half T-467 was actually about: the **coverage** plane. A tile over spectrum the front
    // end never tuned says so in a run-length encoding, so that member stays in the kilobytes
    // however many cells the tile has — 19.34 MB was what it cost before, and 1.5 MB of
    // `"unobserved"` repeated 65 536 times is the regression that would follow it.
    //
    // The `grid` member is NOT part of that win and is not claimed to be: it serialises a value
    // per cell whether or not the cell was observed (~1.05 MB at 256 × 256, measured below and
    // printed), which is the review's S3, not this ticket. It is capped here so it cannot grow.
    const COVERAGE_CAP: usize = 8 * 1024;
    const EMPTY_CAP: usize = 3 << 20;
    let (mut empty_largest, mut coverage_largest, mut empty_checked) = (0usize, 0usize, 0usize);
    for (lf, lt) in viewport() {
        let mut q = params(lf, lt);
        // 1 000 tiles away on the frequency axis: 1.6 GHz off anything this fixture tuned.
        let f = q.iter_mut().find(|(k, _)| k == "f_index").unwrap();
        f.1 = (f.1.parse::<i64>().unwrap() + 1_000).to_string();
        let v = tiles_json(&state, &q).expect("an unobserved address is still a valid address");
        assert_eq!(
            v["grid"]["observed_cells"],
            serde_json::json!(0),
            "the never-tuned address held data: it is not the case this cap is about"
        );
        coverage_largest =
            coverage_largest.max(serde_json::to_string(&v["coverage"]).unwrap().len());
        empty_largest = empty_largest.max(serde_json::to_string(&v).unwrap().len());
        empty_checked += 1;
    }
    eprintln!(
        "  never-tuned addresses: {empty_checked} checked, {empty_largest} B largest body, \
         {coverage_largest} B largest coverage plane"
    );
    assert_eq!(
        empty_checked,
        viewport().len(),
        "the empty-tile cap judged the wrong number of answers"
    );
    assert!(
        coverage_largest <= COVERAGE_CAP,
        "an unobserved {CELLS}x{CELLS} coverage plane was {coverage_largest} B, over the \
         {COVERAGE_CAP} B cap — the run-length encoding has stopped run-length encoding"
    );
    assert!(
        empty_largest <= EMPTY_CAP && empty_largest < largest,
        "a never-tuned {CELLS}x{CELLS} answer was {empty_largest} B (cap {EMPTY_CAP}, and it must \
         stay under the {largest} B an OBSERVED answer costs)"
    );
}
