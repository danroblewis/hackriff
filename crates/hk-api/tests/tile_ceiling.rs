//! **The readable ceiling `GET /api/tiles` declares is TRUE** (T-482): every address inside
//! `axes.{frequency,time}.max_level` is servable, demonstrated by walking the declared box rather
//! than asserted from the shape of the code.
//!
//! # What each assertion here is a property of
//!
//! - **`walks_the_declared_ceiling_and_finds_no_refusal`** is a property of `tile_read` — the real
//!   read path, including `hk-store`'s on-demand fold — over **every** `(level_f, level_t)` inside
//!   the declared box, on a store holding **nothing**. An empty store is the worst case and not a
//!   convenience: a tile that already exists costs no fold budget, so capture can only make a read
//!   cheaper than this, and `tile_read` walks *every* candidate level when none of them holds data
//!   (with data it stops at the first that does). A ceiling true here is true on a full store.
//! - **`the_ceiling_is_maximal_for_this_geometry`** is a property of the *choice*: the pair one
//!   level further up on either axis is genuinely refused, so the box is not conservative for the
//!   sake of it.
//! - **`the_predicate_agrees_with_the_read_over_the_whole_lattice`** is the load-bearing one. The
//!   ceiling is computed from a pure predicate (`hk_api::tiles::servable`); this asserts that
//!   predicate against what `tile_read` actually does, cell by cell, over the whole 12 × 15
//!   declared lattice. A predicate that answered an *adjacent* question — "is there an affordable
//!   level?" without the fold budget, say — would pass every other test in this file and fail this
//!   one.
//!
//! The geometries are parameterised because the ceiling is a claim about *whatever store is open*,
//! not about one constant: T-439's shipped view lattice is one case, and a shallower and a
//! narrower-floor lattice are the others.

use std::sync::{Arc, Mutex};

use hk_api::http::ApiState;
use hk_api::tiles::{TileLattice, TileStore, parse_key, readable_ceiling, servable, tile_read};
use hk_store::history::{HistogramConfig, PyramidConfig, ViewLattice};

struct TempDir(std::path::PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "hk-api-tile-ceiling-{tag}-{}-{:?}",
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

/// The tile edge the canvas actually uses (`docs/16` §6.2), which is the size the defect was
/// measured at — and the size the constraint's **area** nature is visible at, since T-480 found
/// `level_f = 10` refused at `cells = 256` and served at `cells = 128`.
const CELLS: usize = 256;

/// A view-lattice pyramid config, rebuilt here because hk-api cannot depend on hk-pipeline.
///
/// These are **fixtures, not the shipped shape**, and deliberately so: the ceiling is a property of
/// a geometry, and pinning it to whatever `hk_pipeline::history::view_config` currently produces
/// would make this test restate that constant rather than check the predicate against the read.
/// T-484 moved the shipped floor to the display plan's own bin and row and the depth to 7 x 7, and
/// none of the numbers below moved with it — which is the point. `view(6250.0, 4, 4)` was the
/// shipped shape from T-439 to T-484 and is kept as the case whose answer, (9, 1), is written down.
fn view(f_cell_hz: f64, f_levels: usize, t_levels: usize) -> PyramidConfig {
    PyramidConfig {
        f_cells_per_block: 1024,
        histogram: HistogramConfig {
            lo_db: -200.0,
            step_db: 5.0,
            bins: 44,
        },
        ..PyramidConfig::view_lattice(ViewLattice {
            scheme: 2,
            f_cell_hz,
            t_cell: std::time::Duration::from_secs(1),
            cells_per_block: 64,
            f_levels,
            t_levels,
        })
    }
}

/// The geometries under test; T-439's shipped floor first, as the case with a written-down answer.
fn cases() -> Vec<(&'static str, PyramidConfig)> {
    vec![
        (
            "T-439's shipped floor, 6.25 kHz x 1 s, 4 x 4",
            view(6250.0, 4, 4),
        ),
        ("shallower 6.25 kHz x 1 s, 2 x 2", view(6250.0, 2, 2)),
        ("coarser floor 25 kHz x 1 s, 4 x 4", view(25_000.0, 4, 4)),
    ]
}

fn server(dir: &std::path::Path, cfg: PyramidConfig) -> (ApiState, hk_store::history::Geometry) {
    let p = hk_store::Pyramid::open(dir.join("view"), cfg).unwrap();
    let geom = p.geometry().clone();
    (
        ApiState {
            view_history: Some(Arc::new(Mutex::new(p))),
            ..ApiState::default()
        },
        geom,
    )
}

/// The address a real client would build for `(level_f, level_t)`: centred near 100 MHz and near
/// now, never index 0, so nothing here can be an artefact of the origin.
fn params(geom: &hk_store::history::Geometry, lf: usize, lt: usize) -> Vec<(String, String)> {
    let f_cell = geom.levels[0].f_cell_hz * 2f64.powi(lf as i32);
    let t_cell_s = (geom.levels[0].t_cell_ns as f64 / 1e9) * 2f64.powi(lt as i32);
    [
        ("level_f", lf.to_string()),
        ("level_t", lt.to_string()),
        (
            "f_index",
            ((100e6 / (f_cell * CELLS as f64)).floor() as i64).to_string(),
        ),
        (
            "t_index",
            ((1_789_300_000.0 / (t_cell_s * CELLS as f64)).floor() as i64).to_string(),
        ),
        ("cells", CELLS.to_string()),
    ]
    .into_iter()
    .map(|(a, b)| (a.to_string(), b))
    .collect()
}

fn read_ok(
    state: &ApiState,
    geom: &hk_store::history::Geometry,
    lf: usize,
    lt: usize,
) -> Result<(), String> {
    let key = parse_key(geom, &params(geom, lf, lt))
        .map_err(|e| format!("{}: {}", e.status, e.message))?;
    tile_read(state, TileStore::View, &key)
        .map(|_| ())
        .map_err(|e| format!("{}: {}", e.status, e.message))
}

fn ceiling_of(state: &ApiState, geom: &hk_store::history::Geometry) -> (usize, usize) {
    let p = state.view_history.as_ref().unwrap().lock().unwrap();
    readable_ceiling(&p, &TileLattice::view(geom))
}

/// **The ceiling is true**: every address inside the declared box is served, with no 4xx.
#[test]
fn walks_the_declared_ceiling_and_finds_no_refusal() {
    for (name, cfg) in cases() {
        let dir = TempDir::new("true");
        let (state, geom) = server(&dir.0, cfg);
        let (max_f, max_t) = ceiling_of(&state, &geom);
        let lattice = TileLattice::view(&geom);
        assert!(
            max_f < lattice.f_cells_hz.len() && max_t < lattice.t_cells_ns.len(),
            "{name}: a ceiling must name a level the lattice has"
        );
        let mut refused = Vec::new();
        for lf in 0..=max_f {
            for lt in 0..=max_t {
                if let Err(e) = read_ok(&state, &geom, lf, lt) {
                    refused.push(format!("({lf},{lt}) -> {e}"));
                }
            }
        }
        assert!(
            refused.is_empty(),
            "{name}: the declared ceiling ({max_f}, {max_t}) is a LIE — a ceiling that still \
             refuses is the same defect one notch down:\n  {}",
            refused.join("\n  ")
        );
        eprintln!(
            "{name}: lattice {} x {}, ceiling ({max_f}, {max_t}) — {} addresses, 0 refused",
            lattice.f_cells_hz.len(),
            lattice.t_cells_ns.len(),
            (max_f + 1) * (max_t + 1)
        );
    }
}

/// **And it is not conservative for the sake of it**: one level further on either axis refuses.
#[test]
fn the_ceiling_is_maximal_for_this_geometry() {
    let dir = TempDir::new("maximal");
    let (state, geom) = server(&dir.0, view(6250.0, 4, 4));
    let (max_f, max_t) = ceiling_of(&state, &geom);
    // This fixture's answer, stated so a change to the predicate or to either bound is visible
    // here. It is T-439's floor, not T-484's shipped one (see `view`).
    assert_eq!(
        (max_f, max_t),
        (9, 1),
        "a 12 x 15 lattice over a 4 x 4 store reads to (9, 1)"
    );
    for (lf, lt) in [(max_f + 1, max_t), (max_f, max_t + 1)] {
        assert!(
            read_ok(&state, &geom, lf, lt).is_err(),
            "({lf},{lt}) is servable, so the ceiling ({max_f}, {max_t}) is leaving reach unused"
        );
    }
    // The area nature of the bound, on the wire's own terms: `(5, 5)` is servable and is NOT in the
    // box, which is exactly what a per-axis pair cannot express (see `readable_ceiling`).
    assert!(
        read_ok(&state, &geom, 5, 5).is_ok(),
        "(5, 5) should be servable: the bound is an anti-diagonal, and a box loses its far corners"
    );
}

/// **The ceiling does not move with `cells`, and that is deliberate** — the bound does.
///
/// A client bootstraps its lattice from a cheap `cells = 8` probe and renders at 256
/// (`ui/src/surface/tile.ts`), so a ceiling quoted for the *answer's* tile size would be cached
/// against tiles 32x wider on each axis. Measured in the browser tier before this was fixed: the
/// probe read back `(11, 9)` and 69 addresses inside that box were refused at 256. So the pair is
/// a property of the **surface**, stated at this route's own 256-cell unit — under-claiming for a
/// small-tile caller, never over-claiming for anyone.
#[test]
fn the_ceiling_is_stated_for_the_routes_own_tile_unit_and_not_for_the_probes() {
    let dir = TempDir::new("cells");
    let (state, geom) = server(&dir.0, view(6250.0, 4, 4));
    let (max_f, max_t) = ceiling_of(&state, &geom);
    // Same answer whatever the probe would have cost — there is no `cells` in the computation.
    assert_eq!((max_f, max_t), (9, 1));
    // And the bound itself really is on area: at a smaller tile the SAME address that is refused at
    // 256 is served, which is the reach a cacheable pair gives up.
    let small = |lf: usize, lt: usize| {
        let mut q = params(&geom, lf, lt);
        q.retain(|(k, _)| k != "cells");
        q.push(("cells".into(), "32".into()));
        let key = parse_key(&geom, &q).unwrap();
        tile_read(&state, TileStore::View, &key).is_ok()
    };
    assert!(
        read_ok(&state, &geom, max_f + 1, max_t).is_err() && small(max_f + 1, max_t),
        "({}, {max_t}) must be refused at 256 and served at 32, or this proves nothing about area",
        max_f + 1
    );
}

/// **The predicate the ceiling is computed from agrees with the read it is a claim about**, over
/// the whole declared lattice — not only inside the box.
///
/// One direction is soundness (`servable` must never say yes where the read says no, or the ceiling
/// could be a lie); the other is tightness (`servable` saying no where the read succeeds is
/// allowed, but every such cell is reach thrown away, so they are counted and printed).
#[test]
fn the_predicate_agrees_with_the_read_over_the_whole_lattice() {
    let dir = TempDir::new("agree");
    let (state, geom) = server(&dir.0, view(6250.0, 4, 4));
    let lattice = TileLattice::view(&geom);
    let (nf, nt) = (lattice.f_cells_hz.len(), lattice.t_cells_ns.len());
    let mut unsound = Vec::new();
    let mut slack = 0usize;
    let mut grid = String::new();
    for lf in 0..nf {
        for lt in 0..nt {
            let key = parse_key(&geom, &params(&geom, lf, lt)).unwrap();
            let said = {
                let p = state.view_history.as_ref().unwrap().lock().unwrap();
                servable(&p, &key)
            };
            let did = read_ok(&state, &geom, lf, lt).is_ok();
            match (said, did) {
                (true, false) => unsound.push(format!("({lf},{lt})")),
                (false, true) => slack += 1,
                _ => {}
            }
            grid.push(match (said, did) {
                (true, true) => '.',
                (false, false) => 'x',
                (true, false) => '!',
                (false, true) => '~',
            });
        }
        grid.push('\n');
    }
    eprintln!("predicate vs read over the {nf} x {nt} lattice (rows = level_f):\n{grid}");
    assert!(
        unsound.is_empty(),
        "`servable` said yes where the read refused, so a ceiling built on it could be a lie: {}",
        unsound.join(", ")
    );
    eprintln!("conservative at {slack} of {} addresses", nf * nt);
}
