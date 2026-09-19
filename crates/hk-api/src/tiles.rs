//! `GET /api/tiles` — one tile of the unified surface, addressed by **independent**
//! `(level_f, level_t)` (T-438, `docs/16` §7 step 5 re-scoped by §8).
//!
//! # Why one route and not two
//!
//! `docs/16` §7 step 5's own reason: the panes, the minimap and the live edge are **projections of
//! the same pyramid**, so one route is what stops them ever disagreeing on one screen. Under §8
//! that argument gets stronger, not weaker — there is no live-versus-history split left to keep
//! consistent, only viewports at different levels onto one surface.
//!
//! # The addressing, and what `device` and `scheme` do in it
//!
//! The key is `(device, scheme, level_f, level_t, f_index, t_index)`. §8.3 named only the last
//! four; §6.3 already required `device`, and retrofitting a key is the expensive kind of change,
//! so both are here from the start.
//!
//! - **`scheme` is the lattice the address is expressed in — and, since T-439, which store answers
//!   it.** The two cannot come apart: asking for the view lattice while reading scheme 1's ladder
//!   is exactly the gap T-438 left, where the coarse nodes had nothing behind them and fell back on
//!   a level whose time cell is a day. [`tile_store`] is the one mapping, and a server with no view
//!   pyramid open behaves as it did before.
//!   `scheme=view` (the default) is the de-welded view lattice: node `(0, 0)` is the
//!   open pyramid's **own level-0 cell** and each axis doubles independently, so every
//!   `(level_f, level_t)` in range is a node. `scheme=<n>` addresses a store scheme's own ladder
//!   through [`Geometry::axes_of`]/[`Geometry::level_at`] — and a **welded ladder is the diagonal**
//!   of that lattice, so `level_at(3, 0)` is `None` and this route answers *this scheme has no such
//!   node* (404) rather than snapping to a level whose time cell is a day (T-434).
//! - **`device` is whose coverage decides this tile's grey.** Coverage is device-local
//!   (T-259/T-305, §6.3), so it belongs in the key and not in a cell. `any` is the union and keeps
//!   `"named": false`, so a merged plane can never wear one radio's identity.
//!
//! **The view lattice's floor is the store's, not `docs/16` §6.2's.** §6.2 put node (0, 0) at
//! 100 kHz × **128 s** against a 120 s IQ retention — the entire live view inside one time cell,
//! and every realistic pane finer than it, so `level_t` pinned at 0 and the de-welding bought
//! nothing on the axis it was introduced for (T-437 finding F1). Anchoring at the open pyramid's
//! level-0 cell is that fix, and it is the geometry the spike actually ran on.
//!
//! # Where the tiles come from (T-439)
//!
//! From the **live chain**, and from nothing else. `hk_pipeline::history` folds each history frame
//! into the view pyramid's level 0 as it folds it into scheme 1, so the finest node *is* the
//! growing edge where hardware is currently tuned — `docs/16` §8.1's leap, which is why this route
//! has no live-specific branch and no live-versus-history seam to keep consistent. A tile at that
//! edge covers exactly what was sampled: the cells the front end's band and the frame's own
//! duration reach, and no others (T-406's rule). The newest cells of a growing tile are routinely
//! *observed but not yet measured* — the radio is demonstrably tuned there and the fold has not
//! caught up — and that is neither grey nor T-423's `"unknown"`: `coverage` says observed, this
//! route's `grid` has no cell yet, and the difference is visible because the two planes are served
//! separately.
//!
//! # How this avoids `/api/history`'s budget-as-level-selector defect (F2)
//!
//! T-437 measured it: same window, same band, only `max_f` changed — `max_f=384` served
//! 38 784/38 784 cells observed, `max_f=256` served 384/576 (**67 %**). Tightening the *frequency*
//! budget 1.5× cost **34× of time resolution and greyed a third of the window**. That is a
//! grey-honesty violation caused by **level choice**, which `docs/16` §4 does not name: §4 guards
//! the fold, and the fold is fine — here a cell reads *unobserved* while level 0 holds the
//! measurement.
//!
//! This route cannot express that bug, because **it has no caller-supplied per-axis budget at
//! all**:
//!
//! 1. The tile's grid is always exactly `cells × cells` laid on the tile's own extent. The address
//!    *is* the budget, so `level_f` and `level_t` are structurally independent — changing one
//!    cannot move the other's cell size by so much as a rounding (asserted in
//!    `changing_one_axis_level_never_moves_the_other_axis_cell`).
//! 2. The store level is chosen **finest-affordable-first**, never coarsest-adequate. Folding a
//!    finer level onto the tile's grid can never grey a cell the finer level holds — a fold is a
//!    max and a sum, so an output cell is observed if *any* source cell in it was. Only the
//!    opposite direction (a source coarser than the tile's cell, which **replicates** a measured
//!    value) is a claim, and it is stated per axis in `resolution.fold` and downgrades the honesty
//!    tier to `survey-overview`.
//! 3. Candidates are ordered by **cell area**, explicitly, never by level index. T-434's warning:
//!    index order is a coarseness order only for a ladder — in a lattice node (1, 0) outranks
//!    (0, 3) in index while being *finer* in time.
//! 4. When the finest affordable level holds nothing, the read walks **coarser** through the
//!    remaining candidates (T-426's rule, in the direction this route's preference makes
//!    meaningful: the byte budget evicts the finest tiles first, §5.5). `resolution.answered`
//!    reports the level that **actually answered**, and `resolution.tried` every level consulted —
//!    a silent fallback would trade one lie for another.
//!
//! # Cost, and the two caps
//!
//! T-437 measured rendering at p95 2.2 ms for 48 panes and tile **production** at ~500 ms per
//! tile — three orders of magnitude apart. Production is what this route is designed against.
//!
//! - **Work per tile** is bounded by [`TILE_MAX_TOTAL_SOURCE_CELLS`], and **one lock hold** by
//!   [`TILE_MAX_SOURCE_CELLS`]: the read is chunked into whole output rows, re-acquiring the
//!   history lock per chunk, so a tile fan-out at the live edge can never lock ingest out for a
//!   whole tile (§5.5 cap 3, the report builder's discipline).
//! - **Concurrency** is bounded by [`TILE_MAX_IN_FLIGHT`] ([`TileSlot`]). Over the cap the answer
//!   is `503` naming the cap, not a queue that grows until ingest starves.
//!
//! # The coverage plane is a table of distinct planes, run-length encoded (T-467)
//!
//! `coverage` here is **not** `/api/coverage`'s per-cell form, and that is the single largest cost
//! ever measured on this route. The per-cell form serialises
//! `{"state":…,"duty":…,"observed_s":…,"last_s":…,"spans":…,"center_hz":…,"sample_rate_hz":…}` —
//! about 146 B — for each of 65 536 cells, and this route carried **two** such planes, `any` and
//! `devices[0]`, byte-identical on a one-device server. Measured against the demo backend that was
//! 99 % of a 19.34 MB tile body, to deliver the one field `ui/src/surface/tile.ts` reads.
//!
//! [`crate::coverage::TileOverlay`] serves each **distinct** plane once, as a run-length encoding
//! of per-cell state codes over an alphabet served beside it. Measured in-process on the same
//! 256 × 256 grid: 19 818 236 B → 2 906 B. The three states are untouched — `unobserved` is still
//! its own code, `unknown` (T-423) still its own, and no cell on the plane carries a measurement
//! key of any kind, so there is nothing on it a client could read as a level of zero. The per-cell
//! sampling detail is a **hover** question about one cell and `/api/coverage` still answers it;
//! `/api/timeline`'s overlay is unchanged.
//!
//! # What a tile never carries
//!
//! Emitters (§5.3). Identity gating is per-caller and a tile is not; a sealed tile is immutable and
//! an emitter set never is. The coarse-zoom form is a **count per cell**, served separately by
//! `GET /api/tiles/events` so the counts stay live while the tile stays cacheable.
//!
//! | Method | Path | Query | Answers |
//! |---|---|---|---|
//! | GET | `/api/tiles` | `?level_f&level_t&f_index&t_index[&scheme][&device][&cells]` | `{key, extent, axes, grid, coverage, resolution, cost}` |
//! | GET | `/api/tiles/events` | the same address | `{key, extent, counts, total, rule}` |

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use hk_model::{FreqRange, IdleGap, TimeRange, Timestamp};
use hk_store::history::Geometry;
use hk_store::{Overview, OverviewCell, RegionQuery, Resolution};
use serde_json::{Value, json};

use crate::http::ApiState;
use crate::navigation::DetailSource;
use crate::query::{ApiError, Params, Region, bad, count, param};

/// Cells on each edge of a tile — `docs/16` §6.2's uniform 256 × 256.
///
/// Uniformity is what makes §5.5's **count**-based client budget an honest proxy for bytes, and
/// 256 × 256 is also exactly [`hk_store::coverage::MAX_COVERAGE_GRID_CELLS`], so a tile's
/// record-derived coverage plane is always laid on the tile's own axes and never silently reduced.
pub const TILE_CELLS: usize = 256;
/// Smallest tile edge accepted. A smaller tile is a test or a probe, not the canvas's unit.
pub const MIN_TILE_CELLS: usize = 8;

/// Source cells read under **one** history lock hold.
///
/// The tile read is chunked into whole output rows so no single hold exceeds this: an unbounded
/// fan-out at the live edge taking the history lock for a whole tile is exactly the failure the
/// report builder's ≤ 256-row chunks exist to prevent (`docs/16` §5.5 cap 3).
pub const TILE_MAX_SOURCE_CELLS: usize = crate::query::MAX_API_CELLS;

/// Source cells read for **one whole tile**, across every chunk.
///
/// This is the work bound that decides which store levels are affordable. It is deliberately a
/// different number from [`TILE_MAX_SOURCE_CELLS`]: that one bounds a lock hold, this one bounds a
/// request. Four chunk-loads is the widest read that still finishes well inside a tile's budget on
/// the measured per-cell cost (`docs/16` §6.4).
pub const TILE_MAX_TOTAL_SOURCE_CELLS: usize = 4 * TILE_MAX_SOURCE_CELLS;

/// Tile reads in flight at once (`docs/16` §5.5 cap 3: **server backpressure**, not a browser's
/// connection limit).
///
/// Chosen against `hk-store`'s lock behaviour rather than against a client's appetite: the history
/// store is behind one mutex, so concurrent tile reads serialise on it anyway, and the only thing
/// a deeper queue buys is a longer stretch during which ingest is competing for that mutex. Four
/// keeps a pan's burst moving while leaving the lock free most of the time. Over the cap the answer
/// is `503`, which a client retries — §5.5's cap (1) is LIFO with viewport cancellation on the
/// client, and a refusal is what lets it cancel rather than wait.
pub const TILE_MAX_IN_FLIGHT: usize = 4;

/// The widest tile the view lattice needs: the whole 1 MHz–6 GHz device range in two tiles
/// (`docs/16` §6.2's V7, kept while its floor is replaced).
const VIEW_MAX_TILE_HZ: f64 = 3.0e9;
/// The tallest tile the view lattice needs: a month of retention in one tile (§6.2's V7).
const VIEW_MAX_TILE_NS: i64 = 30 * 86_400 * 1_000_000_000;
/// Hard cap on axis levels, so a misconfigured floor cannot produce an unbounded axis.
const MAX_VIEW_LEVELS: usize = 32;

/// One tile read in flight. Dropping it releases the slot, so an error path cannot leak one.
///
/// The counter lives on [`ApiState`], not in a `static`: two servers in one test process must not
/// share a cap, and a cap that leaks across tests is a cap nobody can assert.
#[derive(Debug)]
pub struct TileSlot(Arc<AtomicUsize>);

impl TileSlot {
    /// Takes a slot, or `None` when [`TILE_MAX_IN_FLIGHT`] are already out.
    pub fn acquire(counter: &Arc<AtomicUsize>) -> Option<Self> {
        let mut seen = counter.load(Ordering::Acquire);
        loop {
            if seen >= TILE_MAX_IN_FLIGHT {
                return None;
            }
            match counter.compare_exchange_weak(seen, seen + 1, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Some(Self(Arc::clone(counter))),
                Err(now) => seen = now,
            }
        }
    }

    /// Slots currently out, including this one.
    pub fn in_flight(&self) -> usize {
        self.0.load(Ordering::Acquire)
    }
}

impl Drop for TileSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The refusal served over the cap, so the body states the number rather than only the status.
fn too_many_in_flight() -> ApiError {
    ApiError::new(
        503,
        format!(
            "too many tile reads in flight (limit {TILE_MAX_IN_FLIGHT}): tile production takes the \
             history lock, so the cap is ingest backpressure, not a queue — cancel tiles whose \
             viewport you have left and retry the ones you still want"
        ),
    )
}

/// Which open pyramid a tile address resolves against (T-439).
///
/// `scheme` was already *the lattice the address is expressed in*; with a view-scheme pyramid open
/// it is also **which store answers**, and the two cannot come apart — asking for the view lattice
/// and reading scheme 1's ladder is precisely the gap T-438 left: the coarse nodes had no store
/// behind them and fell back on a ladder whose time cell is a day.
///
/// The choice is one function so there is exactly one mapping, and so a server with no view
/// pyramid behaves as it did before (everything resolves against [`ApiState::history`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileStore {
    /// The de-welded view lattice ([`ApiState::view_history`]), whose finest node is the live edge.
    View,
    /// The spectrum-history pyramid `/api/history` answers from (scheme 1).
    Main,
}

/// Picks the store for this address. `scheme=view` (and the default) uses the view pyramid when
/// one is open; a numeric `scheme` uses whichever open pyramid *is* that scheme, so `scheme=2`
/// and `scheme=view` name the same store and cannot disagree.
pub fn tile_store(state: &ApiState, q: &Params) -> TileStore {
    let Some(v) = state.view_history.as_ref() else {
        return TileStore::Main;
    };
    match param(q, "scheme") {
        None | Some("view") => TileStore::View,
        Some(other) => match other.parse::<u16>() {
            Ok(n) => {
                let is_view = v
                    .lock()
                    .map(|p| p.config().scheme == n)
                    .unwrap_or_else(|e| e.into_inner().config().scheme == n);
                if is_view {
                    TileStore::View
                } else {
                    TileStore::Main
                }
            }
            // Unparseable: `parse_key` refuses it, and the geometry it refuses against is
            // immaterial. Main keeps the pre-T-439 message.
            Err(_) => TileStore::Main,
        },
    }
}

/// Runs `f` on the pyramid `store` names.
pub(crate) fn with_tile_history<T>(
    state: &ApiState,
    store: TileStore,
    f: impl FnOnce(&hk_store::Pyramid) -> Result<T, ApiError>,
) -> Result<T, ApiError> {
    match store {
        TileStore::Main => crate::http::with_history(state, f),
        TileStore::View => {
            let p = state
                .view_history
                .as_ref()
                .ok_or_else(|| ApiError::new(404, "no view-scheme history on this server"))?;
            let p = p
                .lock()
                .map_err(|_| ApiError::new(500, "view history store poisoned"))?;
            f(&p)
        }
    }
}

/// [`with_tile_history`], with the store **mutable first**, so a read can build the coarse node it
/// is asking for (`docs/16` §5.2 "on demand at the live edge", T-453) inside the same lock hold
/// that then reads it.
///
/// The build and the read cannot be split into two holds: a live-edge summary is dropped the
/// instant the frames under it change, so materialising under one lock and querying under the next
/// would race ingest and serve a tile that reads *unobserved* over data the store holds.
///
/// A store reached only through the floor product is immutable here and is scheme 1, whose coarse
/// levels are a seal-time product; [`hk_store::Pyramid::materialize`] is a no-op for it either way,
/// so that path simply reads.
fn with_tile_history_built<T>(
    state: &ApiState,
    store: TileStore,
    level: u8,
    freq: FreqRange,
    time: TimeRange,
    read: impl FnOnce(&hk_store::Pyramid) -> Result<T, ApiError>,
) -> Result<T, ApiError> {
    let shared = match store {
        TileStore::View => state.view_history.as_ref(),
        TileStore::Main => state.history.as_ref(),
    };
    let Some(shared) = shared else {
        return with_tile_history(state, store, read);
    };
    let mut p = shared
        .lock()
        .map_err(|_| ApiError::new(500, "history store poisoned"))?;
    p.materialize(usize::from(level), freq, time).map_err(|e| {
        ApiError::new(
            400,
            format!("this tile's level cannot be built from the levels below it: {e}"),
        )
    })?;
    read(&p)
}

/// The lattice a tile address is expressed in.
///
/// Both forms give the same thing — a `(level_f, level_t)` grid of cell sizes — and differ only in
/// where the sizes come from, which is exactly the distinction that makes *"no such node"* a real
/// answer instead of a theoretical one.
#[derive(Clone, Debug, PartialEq)]
pub struct TileLattice {
    /// How the address was spelled: `"view"` or the store scheme's id.
    pub name: String,
    /// Frequency cell per `level_f`, finest first.
    pub f_cells_hz: Vec<f64>,
    /// Time cell per `level_t`, finest first, ns.
    pub t_cells_ns: Vec<i64>,
    /// `true` when the lattice is the store's own levels, so a missing node is a real refusal.
    pub from_store: bool,
}

impl TileLattice {
    /// The de-welded view lattice, anchored at the open pyramid's **own level-0 cell** (F1) and
    /// doubling each axis independently until §6.2's widest and tallest tile are reached.
    pub fn view(geom: &Geometry) -> Self {
        let f0 = geom.levels[0].f_cell_hz.max(f64::MIN_POSITIVE);
        let t0 = geom.levels[0].t_cell_ns.max(1);
        let cells = TILE_CELLS as f64;
        let mut f_cells_hz = vec![f0];
        while f_cells_hz.len() < MAX_VIEW_LEVELS
            && f_cells_hz.last().copied().unwrap_or(f0) * cells < VIEW_MAX_TILE_HZ
        {
            f_cells_hz.push(f_cells_hz.last().copied().unwrap_or(f0) * 2.0);
        }
        let mut t_cells_ns = vec![t0];
        while t_cells_ns.len() < MAX_VIEW_LEVELS
            && t_cells_ns
                .last()
                .copied()
                .unwrap_or(t0)
                .saturating_mul(TILE_CELLS as i64)
                < VIEW_MAX_TILE_NS
        {
            t_cells_ns.push(t_cells_ns.last().copied().unwrap_or(t0).saturating_mul(2));
        }
        Self {
            name: "view".into(),
            f_cells_hz,
            t_cells_ns,
            from_store: false,
        }
    }

    /// The store's own lattice, read off the geometry by T-434's [`Geometry::f_axis`]/
    /// [`Geometry::t_axis`]. A welded ladder comes out as the **diagonal**, so most of this grid
    /// has no node — which is the honest statement of what a ladder is.
    pub fn store(geom: &Geometry, scheme: u16) -> Self {
        Self {
            name: scheme.to_string(),
            f_cells_hz: geom.f_axis(),
            t_cells_ns: geom.t_axis(),
            from_store: true,
        }
    }

    fn cell_of(&self, level_f: usize, level_t: usize) -> Option<(f64, i64)> {
        Some((
            *self.f_cells_hz.get(level_f)?,
            *self.t_cells_ns.get(level_t)?,
        ))
    }
}

/// A validated tile address, and the extent it names.
#[derive(Clone, Debug, PartialEq)]
pub struct TileKey {
    /// Whose coverage decides this tile's grey; `"any"` is the union and is never named.
    pub device: String,
    /// The lattice the address is expressed in.
    pub lattice: TileLattice,
    /// Frequency level: the `level_f` axis, 0 finest.
    pub level_f: usize,
    /// Time level: the `level_t` axis, 0 finest. **Independent of `level_f`.**
    pub level_t: usize,
    /// Tile index along frequency.
    pub f_index: i64,
    /// Tile index along time, from the Unix epoch.
    pub t_index: i64,
    /// Cells on each edge.
    pub cells: usize,
    /// This tile's frequency cell, Hz.
    pub f_cell_hz: f64,
    /// This tile's time cell, ns.
    pub t_cell_ns: i64,
    /// The tile's extent.
    pub region: Region,
    /// The store level whose cells are exactly this tile's, when one exists ([`Geometry::level_at`]).
    pub store_node: Option<usize>,
}

impl TileKey {
    fn window(&self) -> TimeRange {
        TimeRange::new(
            Timestamp::from_unix_nanos(self.region.t0_ns),
            Timestamp::from_unix_nanos(self.region.t1_ns),
        )
    }

    fn named_device(&self) -> bool {
        self.device != "any"
    }
}

fn integer(q: &Params, key: &'static str) -> Result<i64, ApiError> {
    param(q, key)
        .ok_or_else(|| bad(&format!("{key} is required")))?
        .parse::<i64>()
        .ok()
        .ok_or_else(|| bad(&format!("{key} must be an integer")))
}

/// Parses and validates a tile address against the open pyramid's geometry.
pub fn parse_key(geom: &Geometry, q: &Params) -> Result<TileKey, ApiError> {
    let cells = count(q, "cells", TILE_CELLS, TILE_CELLS)?;
    if cells < MIN_TILE_CELLS {
        return Err(bad(&format!(
            "cells must be in {MIN_TILE_CELLS}..={TILE_CELLS}"
        )));
    }
    let device = param(q, "device").unwrap_or("any").to_owned();
    if device.is_empty() || device.len() > 128 {
        return Err(bad("device must be `any` or a device id"));
    }
    let lattice = match param(q, "scheme") {
        None | Some("view") => TileLattice::view(geom),
        Some(other) => {
            let n: u16 = other
                .parse()
                .map_err(|_| bad("scheme must be `view` or a store scheme id"))?;
            TileLattice::store(geom, n)
        }
    };
    let level_f = integer(q, "level_f")?;
    let level_t = integer(q, "level_t")?;
    let (level_f, level_t) = match (usize::try_from(level_f), usize::try_from(level_t)) {
        (Ok(a), Ok(b)) => (a, b),
        _ => return Err(bad("level_f and level_t must be >= 0")),
    };
    // "No such node", spelled out: which axis ran out, and how far each one goes. A route that
    // silently clamped here would be the snap T-434 exists to refuse.
    let Some((f_cell_hz, t_cell_ns)) = lattice.cell_of(level_f, level_t) else {
        return Err(ApiError::new(
            404,
            format!(
                "scheme {:?} has no node at (level_f {level_f}, level_t {level_t}): its axes are \
                 level_f 0..{} and level_t 0..{}",
                lattice.name,
                lattice.f_cells_hz.len(),
                lattice.t_cells_ns.len()
            ),
        ));
    };
    // A welded ladder is the DIAGONAL of its own lattice (T-434): addressing it off-diagonal is a
    // node that does not exist, and the honest answer is to say so rather than serve a level whose
    // time cell is a day when a second was asked for.
    let store_node = node_of(geom, f_cell_hz, t_cell_ns);
    if lattice.from_store && store_node.is_none() {
        return Err(ApiError::new(
            404,
            format!(
                "scheme {:?} has no node at (level_f {level_f}, level_t {level_t}): this scheme is \
                 a welded ladder, so only the diagonal exists — its cells coarsen on both axes at \
                 once. Address the `view` lattice for independent axes.",
                lattice.name
            ),
        ));
    }
    let f_index = integer(q, "f_index")?;
    let t_index = integer(q, "t_index")?;
    if f_index < 0 || t_index < 0 {
        return Err(bad("f_index and t_index must be >= 0"));
    }
    let span_hz = f_cell_hz * cells as f64;
    let f_lo = f_index as f64 * span_hz;
    if !(f_lo.is_finite() && f_lo + span_hz <= 1e12) {
        return Err(bad(
            "f_index is outside the addressable spectrum (0 .. 1 THz)",
        ));
    }
    let span_ns = i128::from(t_cell_ns) * i128::from(cells as i64);
    let t0 = i128::from(t_index) * span_ns;
    if t0 + span_ns > i128::from(i64::MAX) / 2 {
        return Err(bad("t_index is outside the addressable time range"));
    }
    Ok(TileKey {
        device,
        lattice,
        level_f,
        level_t,
        f_index,
        t_index,
        cells,
        f_cell_hz,
        t_cell_ns,
        region: Region {
            freq: FreqRange::new(f_lo, f_lo + span_hz),
            t0_ns: t0 as i64,
            t1_ns: (t0 + span_ns) as i64,
        },
        store_node,
    })
}

/// The store level whose cells are **exactly** `(f_cell_hz, t_cell_ns)`, when the scheme has one.
///
/// A welded ladder is the DIAGONAL of its own lattice (T-434): addressing it off-diagonal is a node
/// that does not exist, and `None` here is what says so.
fn node_of(geom: &Geometry, f_cell_hz: f64, t_cell_ns: i64) -> Option<usize> {
    let lf = geom.f_axis().iter().position(|w| *w == f_cell_hz)?;
    let lt = geom.t_axis().iter().position(|d| *d == t_cell_ns)?;
    geom.level_at(lf, lt)
}

/// `(nt, nf)` of store level `l`'s own grid over `r`.
fn dims(geom: &Geometry, l: usize, r: &Region) -> (f64, f64) {
    let g = &geom.levels[l];
    let nf = ((r.freq.hi_hz / g.f_cell_hz).ceil() - (r.freq.lo_hz / g.f_cell_hz).floor()).max(1.0);
    let t0 = r.t0_ns.div_euclid(g.t_cell_ns);
    let t1 = (r.t1_ns.saturating_add(g.t_cell_ns - 1)).div_euclid(g.t_cell_ns);
    ((t1 - t0).max(1) as f64, nf)
}

/// The levels whose **read** over this tile fits the work budget — **one half of the question**,
/// and never an answer on its own. [`affordable_levels`] is the whole answer; this is private for
/// exactly that reason.
///
/// Ordered by **cell area**, explicitly, because T-434 proved index order is a coarseness order
/// only for a ladder: in a lattice node (1, 0) outranks (0, 3) in index while being finer in time.
/// Area is a total order that agrees with the partial coarsening order — if a level is no coarser
/// than another on both axes its area is no larger — so ordering by it never puts a coarser level
/// before a finer one.
///
/// The bound is a **work** bound, never a resolution budget: a level qualifies when its whole grid
/// over the tile fits [`TILE_MAX_TOTAL_SOURCE_CELLS`] and one chunk of it fits
/// [`TILE_MAX_SOURCE_CELLS`]. Because the preference is *finest*, the bound can only ever push the
/// answer toward a level at least as coarse as the finest one that fits — and that is stated as
/// replication, never hidden as grey.
fn read_affordable_levels(geom: &Geometry, key: &TileKey) -> Vec<usize> {
    let mut out: Vec<usize> = (0..geom.n_levels())
        .filter(|&l| {
            let (nt, nf) = dims(geom, l, &key.region);
            // One output row's worth of source, which is the smallest chunk a read can take.
            let rows_per_out =
                (key.t_cell_ns as f64 / geom.levels[l].t_cell_ns as f64).ceil() + 1.0;
            nt * nf <= TILE_MAX_TOTAL_SOURCE_CELLS as f64
                && rows_per_out * nf <= TILE_MAX_SOURCE_CELLS as f64
        })
        .collect();
    out.sort_by(|&a, &b| {
        let area = |l: usize| geom.levels[l].f_cell_hz * geom.levels[l].t_cell_ns as f64;
        area(a).total_cmp(&area(b)).then(a.cmp(&b))
    });
    out
}

/// Whether the **fold** this read would provoke at `level` fits `hk-store`'s materialize budget,
/// charged on an **empty** store — the worst store a read can meet, since a tile that already
/// exists costs no budget ([`hk_store::Pyramid::materialize_cost_bound`]).
///
/// The window is [`chunk_rows`]'s, not the tile's: the read is chunked into whole output rows and
/// `materialize` is called **per chunk** ([`with_tile_history_built`]), so the chunk is what the
/// budget is actually asked for. The last chunk can only be shorter, and the bound is monotone in
/// the window, so charging a full chunk is conservative for every chunk of the read.
fn fold_affordable(p: &hk_store::Pyramid, key: &TileKey, level: usize) -> bool {
    let window = chunk_rows(p.geometry(), key, level) as i64 * key.t_cell_ns;
    // Two things bound the blocks one chunk can touch, and a chunk fits the budget if EITHER does
    // (the true count is at most the smaller one):
    //
    // - its own start grid: `read_level` starts chunk k at `t0 + k * window`, and `t0` is a whole
    //   number of tile spans from the epoch, so every start is a multiple of gcd(window, span);
    // - the tile it lies inside: a chunk never leaves its tile, so it cannot touch a block the
    //   whole tile does not. The tile starts on its own span, and at `(9, 1)` on the shipped
    //   store that span is one 512 s block. The window grid alone says a 30 s chunk may straddle
    //   two blocks. The tile says there is only one to straddle.
    //
    // The last chunk is shorter, starts on the same grid and lies in the same tile. The bound is
    // monotone in the window, so that chunk is covered too.
    let span = key.t_cell_ns.saturating_mul(key.cells as i64);
    let freq = key.region.freq;
    p.materialize_cost_bound(level, freq, window, gcd_ns(window, span))
        .or_else(|| p.materialize_cost_bound(level, freq, span, span))
        .is_some()
}

/// Greatest common divisor of two durations; `0` only when both are `0`.
fn gcd_ns(mut a: i64, mut b: i64) -> i64 {
    while b != 0 {
        (a, b) = (b, a.rem_euclid(b));
    }
    a.abs()
}

/// The store levels that may back this tile, **finest first** — affordable to **read** *and*
/// affordable to **build**.
///
/// # Why both halves are one predicate (T-494)
///
/// They were two, and they disagreed. This function used to answer only the read half, and
/// `tile_read` then walked its answer straight into `hk-store`'s `materialize`, which refuses a
/// fold past [`hk_store::history::MAX_MATERIALIZE_TILES`]. A level can be cheap to read and
/// impossible to build — a coarse node's grid over a tile is small *because* the node is coarse,
/// and the same coarseness is what makes folding it from nothing expensive — so the two predicates
/// contradicted each other on a large part of every lattice, measured: on the shipped 4 × 4 store
/// every address with `level_t >= 5` listed the frequency-coarsest column as a candidate and none
/// of that column could be folded.
///
/// [`servable`] papered over it the only way a per-address `bool` can: by demanding **every**
/// candidate be buildable, so one unbuildable level poisoned the address even though `tile_read`
/// would only ever reach it after everything finer held nothing. That is what capped the ladder —
/// on a 6 × 6 store the ceiling fell to `(7, 2)` and on 8 × 8 to `(0, 0)`, because a deeper store
/// has *more* coarse nodes to be poisoned by.
///
/// **The fix is not to loosen `materialize`.** Its refusal is real: the fold it declines genuinely
/// costs more than 1024 tiles. The fix is that a level nobody can build is not a candidate, so the
/// walk never offers it and `servable` reduces to *"is any level left?"*. Skipping it can only
/// serve more than refusing the whole address did, because the walk continues to the coarser levels
/// that follow it.
///
/// The answer does **not** depend on what the store holds — `materialize_cost_bound` is taken on an
/// empty store — so this stays a pure function of geometry and config, which is what lets
/// [`readable_ceiling`] quote it in a contract.
pub fn affordable_levels(p: &hk_store::Pyramid, key: &TileKey) -> Vec<usize> {
    let mut out = read_affordable_levels(p.geometry(), key);
    out.retain(|&l| fold_affordable(p, key, l));
    out
}

/// Output rows one chunk of a read at `level` covers — **one history lock hold each**.
///
/// Shared by the read and by [`servable`] on purpose: the readable ceiling is a claim about what
/// the read will do, so the two must not be able to disagree about how the read is cut up.
fn chunk_rows(geom: &Geometry, key: &TileKey, level: usize) -> usize {
    let g = &geom.levels[level];
    let (_, nf) = dims(geom, level, &key.region);
    let rows_per_out = (key.t_cell_ns as f64 / g.t_cell_ns as f64).ceil() + 1.0;
    let per_out_row = (rows_per_out * nf).max(1.0);
    let rows = (TILE_MAX_SOURCE_CELLS as f64 / per_out_row)
        .floor()
        .max(1.0);
    (rows as usize).min(key.cells)
}

/// Can [`tile_read`] answer this address **at all**, on the worst store it could meet?
///
/// Both of the route's two refusals, asked before either is provoked — and asked as **one**
/// question, because [`affordable_levels`] now carries both (T-494): a level survives it only if
/// its read fits the work budget *and* its fold fits `hk-store`'s materialize budget on an empty
/// store. So servability is exactly *"is any level left?"*.
///
/// **This is the whole candidate list, not merely the finest.** `tile_read` walks the candidates in
/// order and stops at the first that *holds something*, so on a store where the fine levels are
/// empty — which is every store, over any band it has not tuned — the walk reaches the coarse ones.
/// Every level it can reach has to be answerable, and it is, because every level it can reach is in
/// this list.
pub fn servable(p: &hk_store::Pyramid, key: &TileKey) -> bool {
    !affordable_levels(p, key).is_empty()
}

/// The address at `(level_f, level_t)` on this lattice, at index `(0, 0)`.
///
/// Index is immaterial to servability on this route's own lattices and it is worth saying why
/// rather than assuming it: a tile's extent is a power-of-two multiple of the store's own cell on
/// both axes, and so is a store block, so a tile either contains whole blocks or lies inside one
/// **at every index**. The one offset-dependent quantity — where a read's *time chunks* fall
/// against the blocks — is not indexed at all; `materialize_cost_bound` charges it the worst
/// alignment by construction.
fn probe_key(
    lattice: &TileLattice,
    cells: usize,
    level_f: usize,
    level_t: usize,
) -> Option<TileKey> {
    let (f_cell_hz, t_cell_ns) = lattice.cell_of(level_f, level_t)?;
    Some(TileKey {
        device: "any".into(),
        lattice: lattice.clone(),
        level_f,
        level_t,
        f_index: 0,
        t_index: 0,
        cells,
        f_cell_hz,
        t_cell_ns,
        region: Region {
            freq: FreqRange::new(0.0, f_cell_hz * cells as f64),
            t0_ns: 0,
            t1_ns: t_cell_ns.saturating_mul(cells as i64),
        },
        store_node: None,
    })
}

/// **How far up each axis this route can actually be READ** (T-482).
///
/// # The defect this exists for
///
/// The view lattice is 12 × 15 on the shipped geometry and the store behind it is 4 × 4, so the
/// coarse corner is **named but unbackable**: the route declared `levels` and then `400`d a large
/// part of the grid it had just declared. Measured against a real `hk serve`, an aggressive
/// zoom-out made 117 tile requests of which 107 were refused — and the client was not misbehaving,
/// it was obeying the only bound anyone stated. *Nothing said is never permissive*, inverted: the
/// route promised more than it could deliver, and every client that believed it was punished.
///
/// # What the number means, and what a per-axis pair cannot say
///
/// `(max_f, max_t)` is a **box**: every address with `level_f <= max_f` and `level_t <= max_t` is
/// servable. That is the strong reading, and it is the one a client can use, because the client
/// clamps each axis on its own.
///
/// **The servable set is not a box, though, and a box therefore loses some of it.** The binding
/// constraint at the coarse end is the *work* budget, and work goes as the tile's **area** — a
/// store level's grid over the tile is `tile_hz / f_cell` by `tile_s / t_cell`, so the budget reads
/// `level_f + level_t <= k`, an anti-diagonal. A box inscribed in an anti-diagonal cannot reach
/// both of its far corners: on the shipped geometry `(9, 1)` and `(5, 5)` are both servable and
/// both maximal, and no single box holds them both. What is lost is the off-corner pairs — a very
/// wide tile *and* a very tall one, which is exactly the combination whose work bound is the reason
/// for the ceiling in the first place.
///
/// The tie is settled toward **frequency**, and measured rather than argued: covering the canvas's
/// own widest view (1 MHz–6 GHz by a retention window of tens of minutes) costs 32 tiles at
/// `(9, 1)`, 30 at `(8, 2)` and `(7, 3)`, and 118 at `(5, 5)` — so the sum is what matters and the
/// tie-break barely does, and it goes to the axis the defect appeared on: the frequency surface is
/// 6 GHz wide and always at full extent, while the time surface is a retention window that usually
/// fits inside one tile whatever the level.
///
/// # Readability, not existence
///
/// A node a scheme does not *have* — a welded ladder's off-diagonal, which `parse_key` answers with
/// its own 404 and `axes.store_node` already reports — is skipped here rather than counted against
/// the ceiling. The two are different claims and conflating them would collapse a ladder's ceiling
/// to `(0, 0)` while saying nothing new.
///
/// # It is stated at [`TILE_CELLS`], NOT at this answer's `cells`
///
/// The bound is on tile **area**, so it does move with `cells` — and quoting it *per answer* would
/// be the adjacent question rather than the one a client asks. `ui/src/surface/tile.ts` bootstraps
/// its lattice from a deliberately cheap **`cells = 8`** probe and then renders the surface at 256
/// (`latticeOf(probe, RENDER_CELLS)`), so a ceiling quoted for the probe's own tile size would be
/// cached against tiles 32× wider on each axis and would be a lie for every one of them — measured
/// in the browser tier, where a per-answer ceiling read back as `(11, 9)` from the probe while the
/// page drew at 256 and 69 addresses inside that box were refused.
///
/// So the ceiling is a property of the **surface**, not of the probe that fetched it, and 256 is
/// the canvas's own tile unit (`docs/16` §6.2). It is also the **most conservative** statement over
/// the whole `cells` range this route accepts, since a bigger tile is strictly harder to back — so
/// it cannot over-claim for a caller using any smaller tile either. What it costs is reach for such
/// a caller: at `cells = 32` more levels are genuinely readable than this pair names.
pub fn readable_ceiling(p: &hk_store::Pyramid, lattice: &TileLattice) -> (usize, usize) {
    let cells = TILE_CELLS;
    let geom = p.geometry();
    let nf = lattice.f_cells_hz.len();
    let nt = lattice.t_cells_ns.len();
    // `reach[lt]`: the largest `level_f` with every `(0..=level_f, lt)` servable, or `None` when
    // even `(0, lt)` is not.
    let reach = |lt: usize| -> Option<usize> {
        let mut last = None;
        for lf in 0..nf {
            let Some(key) = probe_key(lattice, cells, lf, lt) else {
                break;
            };
            // A node this scheme does not have is not an unreadable node (see above).
            let exists =
                !lattice.from_store || node_of(geom, key.f_cell_hz, key.t_cell_ns).is_some();
            if exists && !servable(p, &key) {
                break;
            }
            last = Some(lf);
        }
        last
    };
    let (mut best, mut floor_f) = ((0usize, 0usize), usize::MAX);
    for lt in 0..nt {
        let Some(r) = reach(lt) else { break };
        floor_f = floor_f.min(r);
        // Maximise the coarsest tile's AREA (`2^(max_f + max_t)`), then reach in frequency.
        if floor_f + lt > best.0 + best.1 || (floor_f + lt == best.0 + best.1 && floor_f > best.0) {
            best = (floor_f, lt);
        }
    }
    best
}

/// One tile's measurement plane, and what produced it.
pub struct TileRead {
    /// The tile's grid, exactly `cells × cells` on the tile's own extent.
    pub grid: Overview,
    /// The store level that **answered** — not the one implied by the address (T-426's rule).
    pub level: u8,
    /// That level's cells.
    pub src_f_cell_hz: f64,
    /// That level's time cell, ns.
    pub src_t_cell_ns: i64,
    /// Every level consulted, in order. More than one means a finer level held nothing and the
    /// read walked coarser.
    pub tried: Vec<u8>,
    /// Affordable levels, finest first.
    pub candidates: Vec<u8>,
    /// Source cells actually read.
    pub source_cells: usize,
    /// Chunks the read was split into — one history lock hold each.
    pub chunks: usize,
}

/// Reads one tile, chunked by whole output rows so no single history lock hold exceeds
/// [`TILE_MAX_SOURCE_CELLS`].
fn read_level(
    state: &ApiState,
    store: TileStore,
    key: &TileKey,
    level: u8,
) -> Result<TileRead, ApiError> {
    let cells = key.cells;
    let (rows_per_chunk, nf_src, level_f_cell, level_t_cell) =
        with_tile_history(state, store, |p| {
            let g = &p.geometry().levels[level as usize];
            let (_, nf) = dims(p.geometry(), level as usize, &key.region);
            Ok((
                chunk_rows(p.geometry(), key, level as usize),
                nf as usize,
                g.f_cell_hz,
                g.t_cell_ns,
            ))
        })?;
    let mut out = vec![OverviewCell::UNOBSERVED; cells * cells];
    let mut unit = None;
    let (mut src_nt, mut source_cells, mut chunks) = (0usize, 0usize, 0usize);
    let t0 = key.region.t0_ns;
    let mut row = 0usize;
    while row < cells {
        let hi = (row + rows_per_chunk).min(cells);
        let chunk = TimeRange::new(
            Timestamp::from_unix_nanos(t0 + row as i64 * key.t_cell_ns),
            Timestamp::from_unix_nanos(t0 + hi as i64 * key.t_cell_ns),
        );
        // One lock hold per chunk: the history mutex is released between chunks so a tile fan-out
        // at the live edge never holds it for a whole tile (docs/16 §5.5 cap 3). T-453 builds the
        // coarse node inside that same hold, per chunk, so the chunking that bounds the read bounds
        // the build too — and so an ingest between chunks re-builds the live edge rather than
        // leaving half the tile unobserved.
        let part = with_tile_history_built(state, store, level, key.region.freq, chunk, |p| {
            let h = p
                .query(&RegionQuery {
                    freq: key.region.freq,
                    time: chunk,
                    resolution: Resolution::Level(level),
                })
                .map_err(|_| ApiError::new(400, "tile query refused"))?;
            Ok(h.overview(chunk, key.region.freq, hi - row, cells))
        })?;
        unit.get_or_insert(part.unit);
        src_nt += part.src_nt;
        source_cells += part.src_nt * part.src_nf;
        chunks += 1;
        out[row * cells..hi * cells].copy_from_slice(&part.cells);
        row = hi;
    }
    let (mut lo_db, mut hi_db) = (f32::INFINITY, f32::NEG_INFINITY);
    let mut observed_cells = 0;
    for c in &out {
        if c.sources == 0 {
            continue;
        }
        observed_cells += 1;
        lo_db = lo_db.min(c.max_db);
        hi_db = hi_db.max(c.max_db);
    }
    Ok(TileRead {
        grid: Overview {
            unit: unit.unwrap_or(hk_model::PowerUnit::Dbfs),
            nt: cells,
            nf: cells,
            t0_ns: key.region.t0_ns,
            t_cell_ns: key.t_cell_ns as f64,
            f_lo_hz: key.region.freq.lo_hz,
            f_cell_hz: key.f_cell_hz,
            cells: out,
            observed_cells,
            src_nt,
            src_nf: nf_src,
            range_db: (observed_cells > 0).then_some((lo_db, hi_db)),
        },
        level,
        src_f_cell_hz: level_f_cell,
        src_t_cell_ns: level_t_cell,
        tried: vec![level],
        candidates: Vec::new(),
        source_cells,
        chunks,
    })
}

/// Reads one tile: the finest affordable level, walking **coarser** only when a level holds nothing.
pub fn tile_read(state: &ApiState, store: TileStore, key: &TileKey) -> Result<TileRead, ApiError> {
    let (candidates, read_only) = with_tile_history(state, store, |p| {
        let read = read_affordable_levels(p.geometry(), key);
        let both: Vec<usize> = read
            .iter()
            .copied()
            .filter(|&l| fold_affordable(p, key, l))
            .collect();
        Ok((both, read))
    })?;
    if candidates.is_empty() {
        // Which half emptied it is the difference between "this tile is too wide for the history's
        // coarsest cell" and "the levels that would fit cannot be folded from nothing", and a
        // caller can act on one and not the other. Saying "no level fits" for both would be the
        // same conflation T-494 removed from the predicate.
        return Err(ApiError::new(
            400,
            if read_only.is_empty() {
                "no store level can back this tile inside the work budget: the spectrum history's \
                 coarsest cells are still too fine for a tile this wide. This is the view \
                 lattice's own scheme being absent, not an unobserved region."
                    .to_string()
            } else {
                format!(
                    "no store level can back this tile: {} level(s) are cheap enough to READ over \
                     this extent, and none of them can be FOLDED inside the store's materialize \
                     budget from nothing. Ask for a finer level, or a smaller region — \
                     `axes.{{frequency,time}}.max_level` states how far up this route can be read.",
                    read_only.len()
                )
            },
        ));
    }
    let listed: Vec<u8> = candidates.iter().map(|&l| l as u8).collect();
    let mut tried = Vec::new();
    let mut first: Option<TileRead> = None;
    for &level in &listed {
        let mut got = read_level(state, store, key, level)?;
        tried.push(level);
        if got.grid.observed_cells > 0 {
            got.tried = tried;
            got.candidates = listed;
            return Ok(got);
        }
        first.get_or_insert(got);
    }
    let mut r = first.expect("at least one candidate was read");
    r.tried = tried;
    r.candidates = listed;
    Ok(r)
}

/// The honesty tier for one tile read.
///
/// Two independent downgrades, and the weaker claim always wins: a tile wider than any capture
/// window is `survey-overview` by T-341's rule, and so is a tile whose source cells are **coarser**
/// than its own on either axis, because then a measured value is repeating across output cells
/// rather than each one carrying its own measurement.
fn tier(key: &TileKey, r: &TileRead, max_live_span_hz: Option<f64>) -> DetailSource {
    let replicated = r.src_f_cell_hz > key.f_cell_hz * 1.000_001
        || r.src_t_cell_ns as f64 > key.t_cell_ns as f64 * 1.000_001;
    if replicated {
        return DetailSource::SurveyOverview;
    }
    base_tier(key, max_live_span_hz)
}

/// The tier before any replication claim: what a tile of this **width** is, whatever answered it.
///
/// Never `live-iq`, and **T-439 does not change that**. T-439 makes the finest node of the view
/// lattice the growing edge — "live" is a viewport, not a mode (docs/16 §8.1) — but a tile is still
/// a *pyramid* read, served at a level's cell size, and `DetailSource::LiveIq` means exactly "live
/// IQ from the front end at the resolution shown". A 6.25 kHz × 1 s cell is not that, however
/// recently it was written. Claiming otherwise would trade §4's honesty rule for a word, and the
/// ring is where a client goes for live-IQ resolution.
fn base_tier(key: &TileKey, max_live_span_hz: Option<f64>) -> DetailSource {
    match crate::navigation::live_window_verdict(key.region.freq.width_hz(), max_live_span_hz) {
        DetailSource::LiveIq => DetailSource::SpectrumHistory,
        other => other,
    }
}

/// Whether the coverage map answered this tile on its own, and — when it did not — **why not**
/// (T-461).
///
/// Served on *both* paths, so "did the short-circuit fire?" is a question the wire answers rather
/// than one a test has to infer from a timing. `applied` is true **if and only if**
/// `selected_plane_uniform` is `"unobserved"`: a mixed plane, or a uniformly `"observed"` or
/// `"unknown"` one, takes the full read.
fn short_circuit_json(uniform: Option<&'static str>) -> Value {
    json!({
        "applied": uniform == Some("unobserved"),
        "selected_plane_uniform": uniform,
        "rule": "if and only if the SELECTED coverage plane — the one `coverage.selected.plane` \
            names, which is the same plane this answer serves and the same one the renderer greys \
            from — is \"unobserved\" for EVERY cell of this tile's extent, the tile is answered \
            from it: no pyramid query, no level walk, no grid enumeration, and `grid` states its \
            one cell instead of enumerating cells x cells of it. It FAILS CLOSED in every other \
            case: one observed cell, or one \"unknown\" cell (T-423 — we no longer know whether we \
            looked, which is not \"nothing looked\"), and the full read runs. The coverage plane is \
            read at this tile's own cells or coarser, and a coarser cell is unobserved only when no \
            tune span touches it at all, so uniform-unobserved coarser implies uniform-unobserved \
            finer — the predicate cannot be weakened by the grid it was evaluated on.",
    })
}

/// The answer for a tile whose selected coverage plane is `unobserved` end to end (T-461).
///
/// **Measured:** the read this replaces cost 92 ms of the route's own `cost.build_ms` and 2.56 MB
/// on the wire to say *nothing here*, with `source_cells: 65 536` — the full grid walked because
/// the cost is `O(cells²)` and essentially independent of whether any data exists. And the empty
/// case is the **common** case: T-437 measured the default full-device view at 99.4 % grey before
/// history accumulates, settling to 55.2 %.
///
/// It is not a second answer, it is a cheaper spelling of the same one. The client's own rule is
/// *"a cell the coverage plane calls unobserved stays unobserved even with a level beside it"*
/// (`ui/src/surface/tile.ts`), so the measurement the full read would have produced is discarded by
/// the renderer cell for cell — which is why this is observationally equivalent and not merely
/// faster.
fn unobserved_tile_json(
    key: &TileKey,
    store: TileStore,
    ceiling: (usize, usize),
    coverage: Value,
    max_live: Option<f64>,
    elapsed_ms: f64,
    slot: &TileSlot,
) -> Value {
    let source = base_tier(key, max_live);
    json!({
        "key": key_json(key),
        "extent": {
            "f_lo_hz": key.region.freq.lo_hz,
            "f_hi_hz": key.region.freq.hi_hz,
            "f_cell_hz": key.f_cell_hz,
            "t0_s": key.region.t0_ns as f64 / 1e9,
            "t1_s": key.region.t1_ns as f64 / 1e9,
            "t_cell_s": key.t_cell_ns as f64 / 1e9,
            "nt": key.cells,
            "nf": key.cells,
        },
        "axes": axes_json(key, ceiling),
        "grid": unobserved_grid_json(key),
        "coverage": coverage,
        "resolution": {
            "source": source.as_str(),
            "live": source.is_live(),
            "statement": source.statement(),
            // NOTHING answered, because nothing was asked. `null` rather than a level, because
            // naming a level here would claim a read that did not happen — the same direction as
            // `tried: []`, which says the walk did not occur rather than that it found nothing.
            "answered": Value::Null,
            "candidates": Value::Array(Vec::new()),
            "tried": Value::Array(Vec::new()),
            "store": match store {
                TileStore::View => "view-lattice",
                TileStore::Main => "spectrum-history",
            },
            "fold": {
                // No source cells were read, so there is no fold and in particular no REPLICATION:
                // a replicated axis is the only direction that makes a claim, and nothing here
                // claims anything.
                "frequency": axis_fold(key.f_cell_hz, key.f_cell_hz, 0, key.cells),
                "time": axis_fold(key.t_cell_ns as f64, key.t_cell_ns as f64, 0, key.cells),
                "rule": "no level was folded onto this tile because none was consulted: the \
                    coverage map answered it (see `short_circuit`). `source_cells: 0` is the \
                    honest count of what was read.",
            },
            "budget": {
                "max_source_cells_per_lock": TILE_MAX_SOURCE_CELLS,
                "max_source_cells_per_tile": TILE_MAX_TOTAL_SOURCE_CELLS,
                "statement": "these bound WORK, never resolution — and no work was done here: the \
                    coverage map decides grey, and it greyed the whole tile.",
            },
            "short_circuit": short_circuit_json(Some("unobserved")),
            "grey_rule": "grey is decided by `coverage`, never by this block: a level that holds \
                nothing here is a level, and a cell nothing ever sampled is grey. Not-loaded is a \
                third thing and is the client's to draw (docs/16 §5.5).",
        },
        "cost": {
            "build_ms": (elapsed_ms * 1000.0).round() / 1000.0,
            // Zero, and that is the whole ticket: the read this replaces walked 65 536 source cells
            // to produce 65 536 nulls over spectrum no record says was ever sampled.
            "source_cells": 0,
            "chunks": 0,
            "in_flight": slot.in_flight(),
            "in_flight_limit": TILE_MAX_IN_FLIGHT,
            "statement": "this tile's grid was answered from the coverage map alone (T-461): no \
                history lock was taken for it, so `chunks` is 0. The last-known search behind \
                `shadow` (T-519) is separate and states its own holds in `shadow.search.chunks`; \
                over spectrum no tile holds it answers from the pyramid's tile index without \
                reading a cell.",
        },
    })
}

fn axis_fold(source_cell: f64, tile_cell: f64, source_cells: usize, served: usize) -> Value {
    let direction = if source_cell > tile_cell * 1.000_001 {
        "replicated"
    } else if source_cell < tile_cell * 0.999_999 {
        "folded"
    } else {
        "exact"
    };
    json!({
        "source_cell": source_cell,
        "tile_cell": tile_cell,
        "source_cells": source_cells,
        "served": served,
        "direction": direction,
        "replicated": direction == "replicated",
    })
}

/// A measurement value on the wire: a finite number, or `null`. **`null` is *not observed*, never
/// quiet** (C26) — there is no zero here for anything to read as a level.
fn num(x: f32) -> Value {
    if x.is_finite() { json!(x) } else { Value::Null }
}

/// The one cell every cell of a uniform grid is, stated once instead of enumerated (T-461).
///
/// Written through the same [`num`] the per-cell arrays use, from a real [`OverviewCell`], so the
/// short form cannot say something the long form would not have said about the same cell.
fn uniform_cell_json(c: &hk_store::OverviewCell) -> Value {
    json!({
        "max_db": num(c.max_db),
        "occupancy_max": num(c.occupancy_max),
        "coverage": c.coverage,
        "frames": c.frames,
        "observed": c.observed(),
        "rule": "every cell of this grid holds exactly this, so the per-cell arrays are omitted \
            (T-461). `max_db: null` is the ABSENCE of a level, never a level of zero — and grey is \
            not decided here in any case: `coverage` decides it, cell by cell, and this grid says \
            only that the pyramid holds nothing for any of them.",
    })
}

/// The grid of a tile the **coverage map** answered: `cells × cells` of nothing (T-461).
///
/// Every field is what [`read_level`] would have produced for the same address had it run — an
/// [`Overview`] of [`OverviewCell::UNOBSERVED`], `observed_cells` zero, no `range_db`, and the unit
/// `read_level` falls back to when no level answered. `an_unobserved_tile_read_produces_exactly_the_constants_the_short_circuit_serves`
/// asserts that equality against a real store read, so this is a *cheaper spelling* of the full
/// path's answer and not a second answer.
fn unobserved_grid_json(key: &TileKey) -> Value {
    let o = Overview {
        unit: hk_model::PowerUnit::Dbfs,
        nt: key.cells,
        nf: key.cells,
        t0_ns: key.region.t0_ns,
        t_cell_ns: key.t_cell_ns as f64,
        f_lo_hz: key.region.freq.lo_hz,
        f_cell_hz: key.f_cell_hz,
        cells: Vec::new(),
        observed_cells: 0,
        src_nt: 0,
        src_nf: 0,
        range_db: None,
    };
    json!({
        "nt": o.nt,
        "nf": o.nf,
        "t0_s": o.t0_ns as f64 / 1e9,
        "t_cell_s": o.t_cell_ns / 1e9,
        "f_lo_hz": o.f_lo_hz,
        "f_cell_hz": o.f_cell_hz,
        // The four per-cell arrays are ABSENT, not empty: an empty array would read as a grid of
        // no cells, which is a different claim from a grid of cells that hold nothing.
        "uniform": uniform_cell_json(&hk_store::OverviewCell::UNOBSERVED),
        "cells": o.nt * o.nf,
        "observed_cells": 0,
        "range_db": Value::Null,
        "unit": o.unit,
        "percentiles": "unknown: a de-welded fold cannot split a tile's per-frequency histogram \
            across parent time cells, so no percentile is carried (T-434). The noise-floor \
            distribution stays a scheme-1 question, asked through /api/history.",
        "semantics": crate::query::overview_semantics_json(&o),
    })
}

fn grid_json(o: &Overview) -> Value {
    json!({
        "nt": o.nt,
        "nf": o.nf,
        "t0_s": o.t0_ns as f64 / 1e9,
        "t_cell_s": o.t_cell_ns / 1e9,
        "f_lo_hz": o.f_lo_hz,
        "f_cell_hz": o.f_cell_hz,
        // Row-major, time then frequency. `null` is **not observed**, never quiet (C26).
        "max_db": Value::Array(o.cells.iter().map(|c| num(c.max_db)).collect()),
        "occupancy_max": Value::Array(o.cells.iter().map(|c| num(c.occupancy_max)).collect()),
        "coverage": Value::Array(o.cells.iter().map(|c| json!(c.coverage)).collect()),
        "frames": Value::Array(o.cells.iter().map(|c| json!(c.frames)).collect()),
        "cells": o.cells.len(),
        "observed_cells": o.observed_cells,
        "range_db": o.range_db.map(|(lo, hi)| json!({"lo": lo, "hi": hi})),
        "unit": o.unit,
        // De-welding costs the percentiles (T-434): a tile keeps one histogram per frequency cell
        // over the whole tile, which is the parent cell's histogram only under the weld. So
        // `p_low_db`/`floor_db` are not offered here rather than approximated.
        "percentiles": "unknown: a de-welded fold cannot split a tile's per-frequency histogram \
            across parent time cells, so no percentile is carried (T-434). The noise-floor \
            distribution stays a scheme-1 question, asked through /api/history.",
        "semantics": crate::query::overview_semantics_json(o),
    })
}

/// The **shadow** plane (T-519): each column's most-recent-known value, carried down the tile's
/// rows wherever the tile itself holds none — the fog-of-war tier.
///
/// # What it is, and what it is not
///
/// A band swept and then departed is not grey: it was observed, and the newest thing known about it
/// is a real measurement. It is also not *live*: the value is a measurement of `last_t_s` and
/// earlier, carried forward, and the wire says when. So it is its own honesty tier — **last-known /
/// stale** (`docs/adr/0020`) — beside the three `/api/tiles` already serves, and it changes the
/// meaning of none of them. **Grey is still decided by `coverage` alone**: a column with no run here
/// is one no retained measurement reaches, and a cell the coverage plane calls unobserved with no
/// run over it stays grey.
///
/// # Where the value comes from
///
/// Down each column the carried value starts as the newest one **before `t0`**, read from the
/// spectrum-history pyramid by [`hk_store::Pyramid::last_known_search`] — a query over tiles it
/// already holds, newest-first and fine-to-coarse, so nothing is maintained for it and capture pays
/// nothing (T-453) — and is **replaced by this tile's own value** at every row where the grid holds
/// one. A row where the grid holds a value gets no run: the shadow never stands in for a
/// measurement. Rows at or after the store's newest frame get no run either.
///
/// # The search's lock holds
///
/// One step per history lock hold, each at most [`TILE_MAX_SOURCE_CELLS`] source cells, and the
/// whole search at most [`TILE_MAX_TOTAL_SOURCE_CELLS`] — the same two bounds as the tile read, so
/// the shadow can at most double a tile's work and never lengthens a hold.
struct Shadow {
    runs: Vec<hk_store::ShadowRun>,
    known: hk_store::LastKnown,
    store: TileStore,
    edge_ns: Option<i64>,
    chunks: usize,
    elapsed_ms: f64,
    /// Per level of the searched store: `(f_cell_hz, t_cell_ns)`.
    levels: Vec<(f64, i64)>,
}

/// The store the search before `t0` reads: the spectrum-history pyramid when this server has one,
/// because its ladder is time-deep (seconds → days, each coarse level sealed eagerly); a view
/// lattice's nodes stop at seconds and its coarse ones exist only once materialised. A server with
/// no spectrum history searches the tile's own store, at its level 0.
fn shadow_store(state: &ApiState, tile: TileStore) -> TileStore {
    if state.history.is_some() || state.floor.is_some() {
        TileStore::Main
    } else {
        tile
    }
}

fn shadow(
    state: &ApiState,
    tile_store: TileStore,
    key: &TileKey,
    grid: Option<&Overview>,
) -> Result<Shadow, ApiError> {
    let started = std::time::Instant::now();
    let store = shadow_store(state, tile_store);
    let (t0, t1, n) = (key.region.t0_ns, key.region.t1_ns, key.cells);
    let latest = |s: TileStore| {
        with_tile_history(state, s, |p| {
            Ok(p.latest_frame_end().map(Timestamp::as_unix_nanos))
        })
    };
    let seed_edge = latest(store)?;
    let edge_ns = match (seed_edge, latest(tile_store).unwrap_or(None)) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    // What the tile itself says about `[t0, t1)`: per column, the first row holding a value. That
    // is what lets a coarse cell straddling `t0` be used for a column — its frames after `t0` would
    // be in this tile, and there are none.
    let first_after: Vec<i64> = (0..n)
        .map(|f| {
            grid.filter(|g| g.nt == n && g.nf == n)
                .and_then(|g| (0..n).find(|&r| g.cells[r * n + f].observed()))
                .map_or(i64::MAX, |r| t0 + r as i64 * key.t_cell_ns)
        })
        .collect();
    let guard = hk_store::StraddleGuard {
        first_after,
        // Known to the end of the data when the newest frame is inside the tile: nothing exists
        // after it to have been missed.
        known_until_ns: if seed_edge.is_none_or(|e| e <= t1) {
            i64::MAX
        } else {
            t1
        },
    };
    let (mut search, levels) = with_tile_history(state, store, |p| {
        let levels = p
            .geometry()
            .levels
            .iter()
            .map(|g| (g.f_cell_hz, g.t_cell_ns))
            .collect::<Vec<_>>();
        Ok((
            p.last_known_search(
                key.region.freq,
                Timestamp::from_unix_nanos(t0),
                n,
                Some(guard),
                TILE_MAX_SOURCE_CELLS,
                TILE_MAX_TOTAL_SOURCE_CELLS,
            ),
            levels,
        ))
    })?;
    let mut chunks = 0;
    while !search.done() {
        // One lock hold per step, released in between, exactly as the tile read's chunks are.
        with_tile_history(state, store, |p| {
            p.last_known_step(&mut search)
                .map_err(|e| ApiError::new(500, format!("last-known search failed: {e}")))
        })?;
        chunks += 1;
    }
    let known = search.finish();
    let runs = known.carry_forward(grid, key.t_cell_ns as f64, n, edge_ns);
    Ok(Shadow {
        runs,
        known,
        store,
        edge_ns,
        chunks,
        elapsed_ms: started.elapsed().as_secs_f64() * 1e3,
        levels,
    })
}

fn store_name(s: TileStore) -> &'static str {
    match s {
        TileStore::View => "view-lattice",
        TileStore::Main => "spectrum-history",
    }
}

/// The `shadow` block: runs as parallel arrays, a source table, and the search that found them.
fn shadow_json(sh: &Shadow, tile_level: Option<u8>) -> Value {
    let s_of = |ns: i64| ns as f64 / 1e9;
    // `src` indexes this table: 0 is this tile's own grid, then one entry per store level a value
    // before the tile came from — so a run's frequency and time resolution are stated, never implied.
    let mut sources = vec![json!({
        "from": "this-tile",
        "level": tile_level,
        "statement": "a value this tile's own grid holds, carried down the rows below it after the \
            band was departed",
    })];
    let mut src_of_level: Vec<(u8, usize)> = Vec::new();
    let mut src = Vec::with_capacity(sh.runs.len());
    for r in &sh.runs {
        src.push(match r.level {
            None => 0,
            Some(l) => match src_of_level.iter().find(|(x, _)| *x == l) {
                Some(&(_, i)) => i,
                None => {
                    let (f_cell, t_cell) =
                        sh.levels.get(usize::from(l)).copied().unwrap_or_default();
                    sources.push(json!({
                        "from": "before-tile",
                        "store": store_name(sh.store),
                        "level": l,
                        "f_cell_hz": f_cell,
                        "t_cell_s": s_of(t_cell),
                    }));
                    src_of_level.push((l, sources.len() - 1));
                    sources.len() - 1
                }
            },
        });
    }
    let k = &sh.known;
    json!({
        "encoding": "column-runs",
        "runs": sh.runs.len(),
        "f": sh.runs.iter().map(|r| r.f).collect::<Vec<_>>(),
        "row": sh.runs.iter().map(|r| r.row).collect::<Vec<_>>(),
        "rows": sh.runs.iter().map(|r| r.rows).collect::<Vec<_>>(),
        "last_db": sh.runs.iter().map(|r| num(r.max_db)).collect::<Vec<_>>(),
        "last_t_s": sh.runs.iter().map(|r| s_of(r.t_ns)).collect::<Vec<_>>(),
        "src": src,
        "sources": sources,
        "edge_s": sh.edge_ns.map(s_of),
        "search": {
            "store": store_name(sh.store),
            "before_s": s_of(k.before_ns),
            "searched_from_s": s_of(k.searched_from_ns),
            "columns_found": k.found(),
            "unsearched": k
                .stages
                .iter()
                .filter(|s| s.skipped)
                .map(|s| json!([s_of(s.from_ns), s_of(s.to_ns)]))
                .collect::<Vec<_>>(),
            "stages": k
                .stages
                .iter()
                .map(|s| json!({
                    "level": s.level,
                    "from_s": s_of(s.from_ns),
                    "to_s": s_of(s.to_ns),
                    "source_cells": s.source_cells,
                    "found": s.found,
                    "skipped": s.skipped,
                }))
                .collect::<Vec<_>>(),
            "source_cells": k.source_cells,
            "chunks": sh.chunks,
            "build_ms": (sh.elapsed_ms * 1000.0).round() / 1000.0,
            "rule": "newest-first, fine-to-coarse: each stage reads one level over the part of the \
                past the finer stage above it did not, so the whole retained horizon costs a few \
                hundred rows per column; the search stops when every column has a value or the \
                store holds nothing older. Bounded by the tile read's own two budgets \
                (`resolution.budget`), one lock hold per step. A window in `unsearched` was NOT read \
                (over budget, or its coarse cell is not folded yet): a column with no run is \
                unobserved in [searched_from_s, before_s) OUTSIDE those windows, and nothing is \
                claimed about earlier.",
        },
        "rule": "the LAST-KNOWN tier (docs/adr/0020), NOT a measurement of the row it is drawn on. \
            Run i covers rows [row[i], row[i] + rows[i]) of column f[i], in `grid.order`'s axes; \
            last_db[i] is the newest max-hold known there at or before those rows, last seen at \
            last_t_s[i] (absolute capture time; age is the row's time minus it), resolved at \
            sources[src[i]]'s cells. A row where `grid` holds a value is never covered: a shadow \
            never replaces a measurement. Rows at or after `edge_s` (the newest frame) are never \
            covered. GREY IS UNCHANGED AND IS STILL DECIDED BY `coverage` ALONE: draw a shadow only \
            where the coverage plane says \"unobserved\", and a cell with no run over it stays grey \
            — no retained measurement reaches it. The plane is not device-scoped: the pyramid is \
            not, and this carries its values.",
    })
}

fn key_json(key: &TileKey) -> Value {
    json!({
        // Coverage is device-local (T-259/T-305, docs/16 §6.3), so the device is part of the KEY
        // and never part of a cell — and `any` keeps `named: false` so a union can never wear one
        // radio's identity.
        "device": key.device,
        "device_named": key.named_device(),
        "scheme": key.lattice.name,
        "level_f": key.level_f,
        "level_t": key.level_t,
        "f_index": key.f_index,
        "t_index": key.t_index,
        "cells": key.cells,
    })
}

fn axes_json(key: &TileKey, ceiling: (usize, usize)) -> Value {
    json!({
        "frequency": {
            "levels": key.lattice.f_cells_hz.len(),
            "max_level": ceiling.0,
            "cell_hz": key.f_cell_hz,
            "tile_hz": key.f_cell_hz * key.cells as f64,
        },
        "time": {
            "levels": key.lattice.t_cells_ns.len(),
            "max_level": ceiling.1,
            "cell_s": key.t_cell_ns as f64 / 1e9,
            "tile_s": key.t_cell_ns as f64 * key.cells as f64 / 1e9,
        },
        // T-482. `levels` and `max_level` answer DIFFERENT questions and a client that conflates
        // them addresses a node that is named and cannot be built: how many levels the address
        // lattice NAMES, against how far up it can actually be READ. They differ whenever the store
        // ladder is shallower than the address lattice, which on the shipped geometry is a 12 x 15
        // lattice over a 4 x 4 store.
        "readable": "`max_level` is a CEILING ON READING, per axis, and it is a BOX: every address \
            with level_f <= frequency.max_level AND level_t <= time.max_level can be served. \
            Beyond it this route answers 400, because the store behind the lattice cannot back the \
            tile — `levels` names the addresses, `max_level` is the reachable part of them. It is \
            computed from the open pyramid's own geometry against both of this route's work bounds \
            (the per-tile source-cell budget in `resolution.budget`, and the store's fold budget), \
            on the WORST store either could meet, so it does not move as the store fills. \
            The servable set is an AREA constraint and not a box — work goes as tile_hz x tile_s, \
            so the real bound is an anti-diagonal in (level_f, level_t) — and a box inscribed in it \
            cannot reach both far corners. What a box loses is the very-wide-AND-very-tall pairs; \
            the box stated is the one whose corner tile covers the most area, and the tie among \
            those goes to frequency. IT IS STATED FOR A 256-CELL TILE, this route's own unit, and \
            NOT for this answer's `cells`: the bound is on area, so a ceiling quoted per answer \
            would be a lie the moment a client probed cheaply and drew at full size — which is \
            exactly what a client does. 256 is also the widest tile this route accepts, so the \
            pair can never over-claim for a smaller one; a caller using `cells` below 256 can read \
            further than this says, and gives up that reach for a number it can cache.",
        // The store level whose cells are EXACTLY this tile's, when the scheme has one. `null` on
        // the view lattice is not an error: it says this pair is off a welded ladder's diagonal, so
        // the tile is folded rather than read whole (T-434).
        "store_node": key.store_node,
        "independent": "level_f and level_t are independent coordinates: changing one moves only \
            its own axis's cell. The de-welded lattice is the point — a welded ladder is its \
            DIAGONAL, and `store_node` is null wherever this pair is off it.",
    })
}

/// `GET /api/tiles`.
pub fn tiles_json(state: &ApiState, q: &Params) -> Result<Value, ApiError> {
    const ALLOWED: [&str; 8] = [
        "device", "scheme", "level_f", "level_t", "f_index", "t_index", "cells", "token",
    ];
    if let Some((k, _)) = q.iter().find(|(k, _)| !ALLOWED.contains(&k.as_str())) {
        return Err(bad(&format!(
            "unknown parameter {k:?} (allowed: device, scheme, level_f, level_t, f_index, \
             t_index, cells)"
        )));
    }
    let Some(slot) = TileSlot::acquire(&state.tiles_in_flight) else {
        return Err(too_many_in_flight());
    };
    let store = tile_store(state, q);
    let (key, ceiling, readable) = with_tile_history(state, store, |p| {
        let key = parse_key(p.geometry(), q)?;
        let ceiling = readable_ceiling(p, &key.lattice);
        let readable = servable(p, &key);
        Ok((key, ceiling, readable))
    })?;
    let started = std::time::Instant::now();
    let max_live = crate::http::max_live_span_hz(state);
    let window = key.window();
    // T-467: the compact plane-table form. The per-cell form this route used to serve was 99 % of a
    // live tile's 19.34 MB body, duplicated between `any` and `devices[0]`, for a `state` field the
    // renderer reads and nothing else. `TileOverlay` computes each distinct plane once and serves
    // it once.
    //
    // T-461: and it is computed **first**, because it is what decides grey. When the selected plane
    // is `unobserved` over the tile's whole extent, the answer is already complete and the pyramid
    // read below is 65 536 source cells spent to confirm it.
    let overlay =
        crate::coverage::TileOverlay::collect(state, key.region.freq, window, key.cells, key.cells);
    let uniform = overlay.uniform_state(&key.device);
    let coverage = overlay.to_json(&key.device, key.named_device());
    // T-515 (folded into T-507): **the shortcut is a cheaper spelling of the full path, including
    // its refusals.** An address no store level can back is refused whatever its coverage — the
    // `tile_read` below answers it with the full path's own 400. Otherwise whether an address is
    // servable would depend on what the radio happened to sample there: the same address would
    // answer 200 over an unobserved band and 400 once the band became observed, and the declared
    // ceiling (`axes.*.max_level`, a pure function of geometry) would stop being the bound a client
    // can cache. Until T-507 this was hidden by accident: a young server's past was `"unknown"`,
    // which never short-circuits, so the walk above the ceiling reached the refusal anyway.
    if uniform == Some("unobserved") && readable {
        // T-519: a tile the coverage map greys end to end is exactly where a departed band's
        // shadow lives, so the last-known search runs here too — against no grid, because the
        // coverage map just said no tune touched this tile.
        let sh = shadow(state, store, &key, None)?;
        let elapsed_ms = started.elapsed().as_secs_f64() * 1e3;
        let mut v =
            unobserved_tile_json(&key, store, ceiling, coverage, max_live, elapsed_ms, &slot);
        v["shadow"] = shadow_json(&sh, None);
        return Ok(v);
    }
    let r = tile_read(state, store, &key)?;
    let sh = shadow(state, store, &key, Some(&r.grid))?;
    let levels = with_tile_history(state, store, |p| Ok(p.geometry().n_levels()))?;
    let source = tier(&key, &r, max_live);
    let elapsed_ms = started.elapsed().as_secs_f64() * 1e3;
    Ok(json!({
        "key": key_json(&key),
        "extent": {
            "f_lo_hz": key.region.freq.lo_hz,
            "f_hi_hz": key.region.freq.hi_hz,
            "f_cell_hz": key.f_cell_hz,
            "t0_s": key.region.t0_ns as f64 / 1e9,
            "t1_s": key.region.t1_ns as f64 / 1e9,
            "t_cell_s": key.t_cell_ns as f64 / 1e9,
            "nt": key.cells,
            "nf": key.cells,
        },
        "axes": axes_json(&key, ceiling),
        "grid": grid_json(&r.grid),
        "coverage": coverage,
        "shadow": shadow_json(&sh, Some(r.level)),
        "resolution": {
            "source": source.as_str(),
            "live": source.is_live(),
            "statement": source.statement(),
            // Which tier ANSWERED, not the one the address implies (T-426). A silent fallback
            // would trade one lie for another.
            "answered": {
                "level": r.level,
                "levels": levels,
                "f_cell_hz": r.src_f_cell_hz,
                "t_cell_s": r.src_t_cell_ns as f64 / 1e9,
                "exact_node": key.store_node == Some(r.level as usize),
                // T-439: WHICH pyramid answered. `view-lattice` is the de-welded scheme the live
                // chain writes its finest node of, so a fine-frequency/coarse-time address is a
                // real node rather than a fold out of a ladder's diagonal. There is no separate
                // live path — this says which surface, never whether it was "live".
                "store": match store {
                    TileStore::View => "view-lattice",
                    TileStore::Main => "spectrum-history",
                },
            },
            "candidates": r.candidates,
            "tried": r.tried,
            "fold": {
                "frequency": axis_fold(r.src_f_cell_hz, key.f_cell_hz, r.grid.src_nf, key.cells),
                "time": axis_fold(
                    r.src_t_cell_ns as f64,
                    key.t_cell_ns as f64,
                    r.grid.src_nt,
                    key.cells,
                ),
                "rule": "the served grid is ALWAYS the tile's own cells x cells; the level only \
                    decides what was folded onto it. `folded` reduces a finer measurement (nothing \
                    is invented and nothing is greyed); `replicated` repeats one measured value \
                    across output cells and is the only direction that makes a claim, so it \
                    downgrades the tier to survey-overview.",
            },
            "budget": {
                "max_source_cells_per_lock": TILE_MAX_SOURCE_CELLS,
                "max_source_cells_per_tile": TILE_MAX_TOTAL_SOURCE_CELLS,
                "statement": "these bound WORK, never resolution: the level is chosen \
                    finest-affordable-first, so a budget can only ever move the answer toward a \
                    COARSER source, which replicates and says so — it can never grey a cell a \
                    finer level holds. There is no caller-supplied per-axis cell budget on this \
                    route at all, which is why /api/history's max_f defect (T-437 F2: a 1.5x \
                    tighter frequency budget cost 34x of time resolution and greyed a third of the \
                    window) cannot be expressed here.",
            },
            // T-461: served on the full path too, so "did the coverage map answer this?" is a
            // question the wire answers and `applied: false` names the reason it did not.
            "short_circuit": short_circuit_json(uniform),
            "grey_rule": "grey is decided by `coverage`, never by this block: a level that holds \
                nothing here is a level, and a cell nothing ever sampled is grey. Not-loaded is a \
                third thing and is the client's to draw (docs/16 §5.5).",
        },
        "cost": {
            "build_ms": (elapsed_ms * 1000.0).round() / 1000.0,
            "source_cells": r.source_cells,
            "chunks": r.chunks,
            "in_flight": slot.in_flight(),
            "in_flight_limit": TILE_MAX_IN_FLIGHT,
            "statement": "tile PRODUCTION is the cost this surface is designed against, not \
                rendering: T-437 measured 48 panes at p95 2.2 ms against ~500 ms per tile. \
                `chunks` is the number of history lock holds this tile took.",
        },
    }))
}

/// `GET /api/tiles/events` — the coarse-zoom aggregate form (`docs/16` §5.3).
///
/// A tile carries **no emitters**: identity gating is per-caller and a tile is not, and a sealed
/// tile is immutable while an emitter set changes on every ledger append. So the highlight layer is
/// a separate query on the same address, and at a coarse zoom it is a **count per cell** — never a
/// box inflated to be visible, which would fabricate a timespan.
///
/// **One count per event, at the cell holding its start.** An event is a presence interval with its
/// own extent (CLAUDE.md invariant 1); counting it once per row it crosses would inflate a long
/// emission into a busy band. The rule is on the wire rather than left to be inferred.
pub fn tile_events_json(state: &ApiState, q: &Params) -> Result<Value, ApiError> {
    const ALLOWED: [&str; 9] = [
        "device", "scheme", "level_f", "level_t", "f_index", "t_index", "cells", "state", "token",
    ];
    if let Some((k, _)) = q.iter().find(|(k, _)| !ALLOWED.contains(&k.as_str())) {
        return Err(bad(&format!("unknown parameter {k:?}")));
    }
    let key = with_tile_history(state, tile_store(state, q), |p| parse_key(p.geometry(), q))?;
    let repo = state
        .inventory
        .as_ref()
        .ok_or_else(|| ApiError::new(404, "no inventory on this server"))?;
    let repo = repo
        .lock()
        .map_err(|_| ApiError::new(500, "inventory poisoned"))?;
    let window = key.window();
    let cells = key.cells;
    let mut counts = vec![0u32; cells * cells];
    let mut query = crate::query::parse_inventory_query(&[
        ("f_lo".into(), key.region.freq.lo_hz.to_string()),
        ("f_hi".into(), key.region.freq.hi_hz.to_string()),
        ("t0".into(), (key.region.t0_ns as f64 / 1e9).to_string()),
        ("t1".into(), (key.region.t1_ns as f64 / 1e9).to_string()),
    ])?;
    query.limit = crate::events::MAX_EVENT_EMITTERS;
    query.offset = 0;
    let page = repo
        .query_inventory(&query)
        .map_err(|_| ApiError::new(500, "event query failed"))?;
    let mut total = 0u64;
    let mut placed = 0u64;
    for entry in &page.entries {
        let intervals = repo
            .presence_intervals(entry.emitter.id, IdleGap::conservative(), window.end)
            .map_err(|_| ApiError::new(500, "event query failed"))?;
        for i in intervals.iter().filter(|i| i.time.overlaps(&window)) {
            total += 1;
            let t = (i.time.start.as_unix_nanos() - key.region.t0_ns) / key.t_cell_ns;
            // An interval with no measured centre has no column: it is counted in `total` and not
            // placed, exactly as /api/events declines to invent a timespan it did not measure.
            let Some(centre) = i.f_center_hz else {
                continue;
            };
            let f = ((centre - key.region.freq.lo_hz) / key.f_cell_hz).floor();
            // An event that began before this tile, or off its band, is counted in `total` and not
            // placed: clamping it to the edge would put activity in a cell it never occupied.
            if t < 0 || t >= cells as i64 || !(f >= 0.0 && f < cells as f64) {
                continue;
            }
            counts[t as usize * cells + f as usize] += 1;
            placed += 1;
        }
    }
    Ok(json!({
        "key": key_json(&key),
        "extent": {
            "f_lo_hz": key.region.freq.lo_hz,
            "f_hi_hz": key.region.freq.hi_hz,
            "f_cell_hz": key.f_cell_hz,
            "t0_s": key.region.t0_ns as f64 / 1e9,
            "t1_s": key.region.t1_ns as f64 / 1e9,
            "t_cell_s": key.t_cell_ns as f64 / 1e9,
            "nt": cells,
            "nf": cells,
        },
        // Row-major on exactly the tile's axes, so a client indexes this with the tile's index.
        "counts": counts,
        "total": total,
        "placed": placed,
        "emitters_scanned": page.entries.len(),
        "emitters_truncated": page.next_offset.is_some(),
        "rule": "ONE COUNT PER EVENT, placed at the cell holding its START. An event is a presence \
            interval with its own extent; counting it once per row it crosses would inflate a long \
            emission into a busy band, and scaling a sub-cell burst up to be visible would \
            fabricate a timespan (docs/16 §5.3). An event beginning before this tile, or centred \
            off its band, is counted in `total` and not placed — `placed` is what the grid holds.",
        "not_a_tile_channel": "counts change with every append to the observation ledger, so they \
            are computed on demand and never sealed into a tile: a tile stays immutable and \
            cacheable, which is the property its storage was bought for.",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_store::history::{PyramidConfig, ViewLattice};

    fn geom() -> Geometry {
        PyramidConfig::default().geometry().unwrap()
    }

    fn params(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect()
    }

    /// The view lattice's floor is the STORE's level-0 cell, not `docs/16` §6.2's 100 kHz × 128 s
    /// (T-437 finding F1): a 128 s finest time cell puts the whole 120 s IQ retention inside one
    /// cell, so `level_t` pins at 0 and the de-welding buys nothing on the axis it exists for.
    #[test]
    fn the_view_lattices_floor_is_the_stores_own_level_zero_cell() {
        let g = geom();
        let l = TileLattice::view(&g);
        assert_eq!(l.f_cells_hz[0], g.levels[0].f_cell_hz);
        assert_eq!(l.t_cells_ns[0], g.levels[0].t_cell_ns);
        assert_eq!(l.t_cells_ns[0], 1_000_000_000, "one second, not 128");
        // Each axis doubles independently, and reaches docs/16 §6.2's widest and tallest tile.
        for w in l.f_cells_hz.windows(2) {
            assert_eq!(w[1], w[0] * 2.0);
        }
        for w in l.t_cells_ns.windows(2) {
            assert_eq!(w[1], w[0] * 2);
        }
        let widest = l.f_cells_hz.last().unwrap() * TILE_CELLS as f64;
        assert!(widest >= VIEW_MAX_TILE_HZ, "{widest}");
        let tallest = l.t_cells_ns.last().unwrap() * TILE_CELLS as i64;
        assert!(tallest >= VIEW_MAX_TILE_NS, "{tallest}");
    }

    /// A welded ladder is the **diagonal** of its own lattice, and off it there is no node
    /// (T-434). The route says so rather than snapping to a level whose time cell is a day.
    #[test]
    fn a_welded_ladders_off_diagonal_node_is_refused_and_never_snapped() {
        let g = geom();
        // Scheme 1's diagonal: (3, 3) is level 3 — 50 kHz × 3600 s.
        let on = parse_key(
            &g,
            &params(&[
                ("scheme", "1"),
                ("level_f", "3"),
                ("level_t", "3"),
                ("f_index", "0"),
                ("t_index", "0"),
            ]),
        )
        .unwrap();
        assert_eq!(on.store_node, Some(3));
        assert_eq!(on.f_cell_hz, 50_000.0);
        assert_eq!(on.t_cell_ns, 3_600_000_000_000);
        // Off it: fine frequency, coarse time. A ladder cannot express it.
        let err = parse_key(
            &g,
            &params(&[
                ("scheme", "1"),
                ("level_f", "0"),
                ("level_t", "3"),
                ("f_index", "0"),
                ("t_index", "0"),
            ]),
        )
        .unwrap_err();
        assert_eq!(err.status, 404, "{}", err.message);
        assert!(err.message.contains("diagonal"), "{}", err.message);
        // Past the end of an axis is the other kind of "no such node", and names both extents.
        let err = parse_key(
            &g,
            &params(&[
                ("scheme", "1"),
                ("level_f", "99"),
                ("level_t", "0"),
                ("f_index", "0"),
                ("t_index", "0"),
            ]),
        )
        .unwrap_err();
        assert_eq!(err.status, 404, "{}", err.message);
        assert!(err.message.contains("level_f 0..5"), "{}", err.message);
    }

    /// The de-welding, at the addressing layer: **changing one axis's level moves only that axis's
    /// cell.** This is the structural reason T-437's F2 cannot happen here — there, tightening the
    /// *frequency* budget 1.5× cost 34× of *time* resolution.
    #[test]
    fn changing_one_axis_level_never_moves_the_other_axis_cell() {
        let g = geom();
        let key = |lf: &str, lt: &str| {
            parse_key(
                &g,
                &params(&[
                    ("level_f", lf),
                    ("level_t", lt),
                    ("f_index", "0"),
                    ("t_index", "0"),
                ]),
            )
            .unwrap()
        };
        let base = key("0", "0");
        for lf in ["1", "2", "5", "9"] {
            let k = key(lf, "0");
            assert_eq!(
                k.t_cell_ns, base.t_cell_ns,
                "level_f {lf} moved the TIME cell"
            );
            assert!(k.f_cell_hz > base.f_cell_hz);
        }
        for lt in ["1", "2", "5", "9"] {
            let k = key("0", lt);
            assert_eq!(
                k.f_cell_hz, base.f_cell_hz,
                "level_t {lt} moved the FREQUENCY cell"
            );
            assert!(k.t_cell_ns > base.t_cell_ns);
        }
    }

    /// Candidates are ordered by **cell area**, not by level index — T-434's warning made a test.
    /// On a real de-welded lattice index order is not a coarseness order, and ordering by it would
    /// answer a coarse time question with a fine time cell.
    #[test]
    fn candidate_levels_are_ordered_by_cell_area_and_index_order_would_be_wrong() {
        let lattice_geom = PyramidConfig::view_lattice(ViewLattice {
            scheme: 7,
            f_cell_hz: 6250.0,
            t_cell: std::time::Duration::from_secs(1),
            cells_per_block: 256,
            f_levels: 4,
            t_levels: 4,
        })
        .geometry()
        .unwrap();
        // Node (1, 0) has a LOWER index than (0, 3) in nothing — index is f*t_levels + t, so
        // (1, 0) is index 4 and (0, 3) is index 3: (0, 3) comes first by index while being
        // coarser in time by 8x. Index order is not coarseness order.
        let index = |lf: usize, lt: usize| lf * 4 + lt;
        let (i_10, i_03) = (index(1, 0), index(0, 3));
        assert!(i_03 < i_10, "index order puts the time-coarse node first");
        assert!(
            lattice_geom.levels[i_03].t_cell_ns > lattice_geom.levels[i_10].t_cell_ns,
            "…and it is genuinely coarser in time"
        );
        let key = parse_key(
            &lattice_geom,
            &params(&[
                ("level_f", "1"),
                ("level_t", "1"),
                ("f_index", "0"),
                ("t_index", "0"),
            ]),
        )
        .unwrap();
        let got = read_affordable_levels(&lattice_geom, &key);
        let area =
            |l: usize| lattice_geom.levels[l].f_cell_hz * lattice_geom.levels[l].t_cell_ns as f64;
        for w in got.windows(2) {
            assert!(
                area(w[0]) <= area(w[1]),
                "candidates must be finest-first by area: {got:?}"
            );
        }
        // Finest first, so level 0 leads whenever it is affordable.
        assert_eq!(got.first(), Some(&0), "{got:?}");
    }

    /// The work budget is a **work** budget: it excludes levels that cost too much to read and
    /// never reaches for a coarser one to save cells.
    #[test]
    fn the_work_budget_excludes_unaffordable_levels_and_keeps_the_finest_that_fits() {
        let g = geom();
        // A wide, tall tile: 6.4 MHz x 65 536 s at (level_f 7, level_t 8).
        let key = parse_key(
            &g,
            &params(&[
                ("level_f", "7"),
                ("level_t", "8"),
                ("f_index", "0"),
                ("t_index", "0"),
            ]),
        )
        .unwrap();
        let got = read_affordable_levels(&g, &key);
        assert!(!got.is_empty());
        // Level 0 (6.25 kHz x 1 s) over that extent is 128 x 65 536 source cells and must be out.
        let (nt, nf) = dims(&g, 0, &key.region);
        assert!(nt * nf > TILE_MAX_TOTAL_SOURCE_CELLS as f64);
        assert!(!got.contains(&0), "{got:?}");
        // Whatever leads is the finest of those that fit.
        let lead = got[0];
        for &l in &got {
            let area = |l: usize| g.levels[l].f_cell_hz * g.levels[l].t_cell_ns as f64;
            assert!(area(lead) <= area(l));
        }
    }

    /// The in-flight cap is a real cap: the N+1th reader is refused, and a released slot is reusable.
    #[test]
    fn the_in_flight_cap_refuses_over_the_limit_and_releases_on_drop() {
        let c = Arc::new(AtomicUsize::new(0));
        let held: Vec<TileSlot> = (0..TILE_MAX_IN_FLIGHT)
            .map(|_| TileSlot::acquire(&c).expect("under the cap"))
            .collect();
        assert_eq!(held.last().unwrap().in_flight(), TILE_MAX_IN_FLIGHT);
        assert!(TileSlot::acquire(&c).is_none(), "the cap must bind");
        let err = too_many_in_flight();
        assert_eq!(err.status, 503);
        assert!(
            err.message.contains(&TILE_MAX_IN_FLIGHT.to_string()),
            "the body must NAME the cap: {}",
            err.message
        );
        drop(held);
        assert_eq!(c.load(Ordering::Acquire), 0);
        assert!(TileSlot::acquire(&c).is_some());
    }

    // ---- store-backed reads ----------------------------------------------------------------

    /// Tile edge used by the store-backed tests: small enough to keep them quick, large enough
    /// that a fold is a real fold.
    const N: usize = 64;
    /// Tile index on the time axis, chosen so the tile's extent is epoch-aligned by construction.
    const T_INDEX: i64 = 27_958_762;
    /// Tile index on the frequency axis at `level_f = 0`: **even**, so the `level_f + 1` tile
    /// covering it starts at the same edge.
    const F_INDEX: i64 = 1_116;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "hk-api-tiles-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// A pyramid holding `secs` seconds of frames over 400 kHz from the tile's own low edge, with
    /// one carrier, so an observed cell is observed because a frame landed there.
    fn state_with_history(dir: &std::path::Path, secs: i64) -> (ApiState, i64, f64) {
        let mut p = hk_store::Pyramid::open(dir, PyramidConfig::default()).unwrap();
        let g = p.geometry().clone();
        let t_cell = g.levels[0].t_cell_ns;
        let f_cell = g.levels[0].f_cell_hz;
        let t0 = T_INDEX * t_cell * N as i64;
        let f_lo = F_INDEX as f64 * f_cell * N as f64;
        const NB: usize = 128;
        let bin_hz = f_cell * N as f64 / NB as f64;
        for k in 0..secs {
            let mut psd = [1e-12f32; NB];
            psd[40] = 1e-6;
            p.ingest(&hk_store::history::FrameInput::new(
                Timestamp::from_unix_nanos(t0 + k * t_cell),
                t_cell,
                f_lo,
                bin_hz,
                hk_model::PowerUnit::Dbfs,
                &psd,
            ))
            .unwrap();
        }
        let state = ApiState {
            history: Some(Arc::new(std::sync::Mutex::new(p))),
            ..ApiState::default()
        };
        (state, t0, f_lo)
    }

    fn read_at(
        state: &ApiState,
        level_f: &str,
        level_t: &str,
        f_index: i64,
        t_index: i64,
    ) -> (TileKey, TileRead) {
        let g = crate::http::with_history(state, |p| Ok(p.geometry().clone())).unwrap();
        let key = parse_key(
            &g,
            &params(&[
                ("level_f", level_f),
                ("level_t", level_t),
                ("f_index", &f_index.to_string()),
                ("t_index", &t_index.to_string()),
                ("cells", &N.to_string()),
            ]),
        )
        .unwrap();
        let r = tile_read(state, TileStore::Main, &key).unwrap();
        (key, r)
    }

    fn read(state: &ApiState, level_f: &str, level_t: &str, f_index: i64) -> (TileKey, TileRead) {
        read_at(state, level_f, level_t, f_index, T_INDEX)
    }

    /// The tile's grid is **always** its own `cells × cells` on its own extent, whatever level
    /// answered — and the level that answered is reported, not the one the address implies (T-426).
    #[test]
    fn a_tile_is_folded_onto_its_own_cells_and_names_the_level_that_answered() {
        let dir = temp_dir("fold");
        let (state, t0, f_lo) = state_with_history(&dir, N as i64);
        let (key, r) = read(&state, "0", "0", F_INDEX);
        assert_eq!((r.grid.nt, r.grid.nf), (N, N));
        assert_eq!(r.grid.t0_ns, t0);
        assert_eq!(r.grid.f_lo_hz, f_lo);
        assert_eq!(
            key.region.t1_ns - key.region.t0_ns,
            N as i64 * 1_000_000_000
        );
        // Level 0 is the finest affordable level and it holds the data, so it answers and the
        // address's node is exactly it.
        assert_eq!(r.level, 0, "tried {:?}", r.tried);
        assert_eq!(key.store_node, Some(0));
        assert_eq!(r.tried, vec![0], "no walk was needed: {:?}", r.tried);
        assert!(r.grid.observed_cells > 0, "the fixture's frames must show");
        // Exactly one chunk: N x N source cells is far inside one lock hold's budget.
        assert_eq!(r.chunks, 1);
        eprintln!(
            "T-438 tile (0,0): observed {}/{} cells, {} source cells, {} chunk(s)",
            r.grid.observed_cells,
            N * N,
            r.source_cells,
            r.chunks
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The F2 guard.** T-437 measured `/api/history` losing 34× of *time* resolution and greying
    /// a third of the window when only the *frequency* budget was tightened 1.5×. The same
    /// experiment here — same window, same data, only the frequency address coarsened one step —
    /// must cost neither: the time cell is identical, and every time row the fine address shows as
    /// observed is observed at the coarse one too.
    #[test]
    fn coarsening_the_frequency_address_costs_no_time_resolution_and_greys_nothing() {
        let dir = temp_dir("f2");
        let (state, _, _) = state_with_history(&dir, N as i64);
        let (fine_key, fine) = read(&state, "0", "0", F_INDEX);
        let (coarse_key, coarse) = read(&state, "1", "0", F_INDEX / 2);
        // The two tiles start at the same frequency edge and span the same window.
        assert_eq!(fine_key.region.freq.lo_hz, coarse_key.region.freq.lo_hz);
        assert_eq!(fine_key.region.t0_ns, coarse_key.region.t0_ns);
        assert_eq!(fine_key.region.t1_ns, coarse_key.region.t1_ns);
        // F2's exact failure: the time axis moved when only the frequency axis was asked to.
        assert_eq!(
            fine_key.t_cell_ns, coarse_key.t_cell_ns,
            "coarsening level_f must not touch the time cell"
        );
        assert_eq!(
            fine.grid.t_cell_ns, coarse.grid.t_cell_ns,
            "…nor the served grid's"
        );
        // F2's other half: a third of the window went grey. Every row the fine tile observed must
        // be observed in the coarse tile's lower half, which covers exactly the fine tile's band.
        let observed_rows = |g: &Overview, lo: usize, hi: usize| -> Vec<bool> {
            (0..N)
                .map(|t| (lo..hi).any(|f| g.cells[t * N + f].observed()))
                .collect()
        };
        let fine_rows = observed_rows(&fine.grid, 0, N);
        let coarse_rows = observed_rows(&coarse.grid, 0, N / 2);
        assert!(
            fine_rows.iter().any(|&b| b),
            "the fixture must observe rows"
        );
        for (t, (&a, &b)) in fine_rows.iter().zip(&coarse_rows).enumerate() {
            assert!(
                !a || b,
                "row {t}: the fine address observed it and the coarse one greyed it"
            );
        }
        eprintln!(
            "T-438 F2 guard: fine observed {} rows, coarse {} rows, t_cell {} s both",
            fine_rows.iter().filter(|b| **b).count(),
            coarse_rows.iter().filter(|b| **b).count(),
            fine.grid.t_cell_ns / 1e9
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A tile over a window nothing observed is grey **and says which levels it consulted** — the
    /// walk happened and found nothing, which is a different statement from not having walked.
    #[test]
    fn an_unobserved_tile_is_grey_and_reports_every_level_it_consulted() {
        let dir = temp_dir("grey");
        let (state, _, _) = state_with_history(&dir, N as i64);
        // The next tile along the time axis: the fixture wrote nothing there.
        let g = crate::http::with_history(&state, |p| Ok(p.geometry().clone())).unwrap();
        let key = parse_key(
            &g,
            &params(&[
                ("level_f", "0"),
                ("level_t", "0"),
                ("f_index", &F_INDEX.to_string()),
                ("t_index", &(T_INDEX + 4).to_string()),
                ("cells", &N.to_string()),
            ]),
        )
        .unwrap();
        let r = tile_read(&state, TileStore::Main, &key).unwrap();
        assert_eq!(r.grid.observed_cells, 0);
        assert!(r.grid.range_db.is_none(), "no scale from nothing");
        assert_eq!(
            r.tried, r.candidates,
            "an empty answer must have consulted every affordable level: {:?}",
            r.tried
        );
        assert!(r.candidates.len() > 1, "{:?}", r.candidates);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The cap is enforced **on the route**, not only in the slot type: with the cap's worth of
    /// readers out, the next request is a `503` that names the cap rather than a queued read that
    /// lengthens the stretch during which ingest competes for the history mutex.
    #[test]
    fn the_route_refuses_over_the_in_flight_cap_and_serves_again_once_a_slot_frees() {
        let dir = temp_dir("cap");
        let (state, _, _) = state_with_history(&dir, N as i64);
        let q = params(&[
            ("level_f", "0"),
            ("level_t", "0"),
            ("f_index", &F_INDEX.to_string()),
            ("t_index", &T_INDEX.to_string()),
            ("cells", &N.to_string()),
        ]);
        assert_eq!(
            tiles_json(&state, &q).unwrap()["cost"]["in_flight_limit"],
            json!(TILE_MAX_IN_FLIGHT)
        );
        let held: Vec<TileSlot> = (0..TILE_MAX_IN_FLIGHT)
            .map(|_| TileSlot::acquire(&state.tiles_in_flight).unwrap())
            .collect();
        let err = tiles_json(&state, &q).unwrap_err();
        assert_eq!(err.status, 503, "{}", err.message);
        assert!(
            err.message.contains(&TILE_MAX_IN_FLIGHT.to_string()),
            "{}",
            err.message
        );
        drop(held);
        let v = tiles_json(&state, &q).unwrap();
        assert_eq!(v["cost"]["in_flight"], json!(1), "{v}");
        assert!(v["grid"]["observed_cells"].as_u64().unwrap() > 0, "{v}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The read is chunked into whole output rows, so no single history lock hold exceeds
    /// [`TILE_MAX_SOURCE_CELLS`] — the ingest-backpressure discipline of `docs/16` §5.5 cap 3.
    #[test]
    fn a_wide_tile_is_read_in_chunks_so_one_lock_hold_stays_inside_the_budget() {
        let dir = temp_dir("chunk");
        let (state, _, _) = state_with_history(&dir, 16);
        // A tile 2^10 coarser in frequency and 2^4 longer in time: 409.6 MHz x 1024 s. `t_index`
        // is per level, so the same wall-clock window is `T_INDEX >> 4` here — which is itself the
        // de-welding: the two axes index independently.
        let (_, r) = read_at(&state, "10", "4", 1, T_INDEX >> 4);
        assert!(
            r.chunks > 1,
            "a wide tile must take more than one lock hold"
        );
        assert!(
            r.source_cells <= TILE_MAX_TOTAL_SOURCE_CELLS,
            "{} source cells",
            r.source_cells
        );
        assert!(
            r.source_cells.div_ceil(r.chunks) <= TILE_MAX_SOURCE_CELLS,
            "one lock hold read {} cells",
            r.source_cells / r.chunks
        );
        eprintln!(
            "T-438 chunking: level {}, {} source cells in {} lock hold(s)",
            r.level, r.source_cells, r.chunks
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- T-461: the coverage short-circuit ---------------------------------------------------

    /// One dwell over `lo..hi` for `[t0, t1)`, by a named front end — the record that makes a band
    /// *observed* and, just as importantly, fixes the **record horizon**: a tile wholly before it
    /// is `"unknown"`, not `"unobserved"`, and must not short-circuit.
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
        let w = TimeRange::new(
            Timestamp::from_unix_nanos(t0_ns),
            Timestamp::from_unix_nanos(t1_ns),
        );
        ObservationRecord::Dwell(DwellRecord {
            schema: hk_model::attention::ATTENTION_SCHEMA_VERSION,
            survey_id: None,
            seq: 1,
            plan_version: 1,
            site: SiteKey::Unassigned,
            device_id: Some("mock:0".into()),
            reason: Reason::RegionDwell { hop: 0 },
            tier: Tier::ScheduledPlan,
            window: ObservedWindow {
                center_hz: (lo + hi) / 2.0,
                sample_rate_hz: hi - lo,
                usable: FreqRange::new(lo, hi),
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

    /// [`state_with_history`] plus an observation log holding one dwell over the fixture's own band
    /// and window, offset in time by `record_offset_s`.
    ///
    /// The offset is the whole point of the fixture: at `0` the record covers the tile, so the
    /// fixture's band is `observed` and every other band is `unobserved`. Pushed an hour into the
    /// future, the record horizon moves past the tile and *every* cell becomes `"unknown"` — which
    /// is the fail-closed case, and it is a case this fixture can actually produce rather than one
    /// argued for.
    fn state_with_records(
        dir: &std::path::Path,
        secs: i64,
        record_offset_s: i64,
    ) -> (ApiState, i64, f64) {
        let (mut state, t0, f_lo) = state_with_history(dir, secs);
        let f_hi = f_lo + 6250.0 * N as f64;
        let store = hk_store::observation::ObservationStore::open(
            hk_store::observation::ObservationLogConfig::new(dir.join("observations")),
        )
        .unwrap();
        let off = record_offset_s * 1_000_000_000;
        store.append(&dwell(
            f_lo,
            f_hi,
            t0 + off,
            t0 + off + secs * 1_000_000_000,
        ));
        store.flush();
        state.observations = Some(store);
        (state, t0, f_lo)
    }

    fn tile_params(f_index: i64, t_index: i64) -> Vec<(String, String)> {
        params(&[
            ("level_f", "0"),
            ("level_t", "0"),
            ("f_index", &f_index.to_string()),
            ("t_index", &t_index.to_string()),
            ("cells", &N.to_string()),
        ])
    }

    /// **The ticket.** A tile whose selected coverage plane is `unobserved` end to end is answered
    /// from that plane: no pyramid query, no level walk, no grid enumeration, and a body that
    /// states its one cell instead of enumerating `cells × cells` of it.
    ///
    /// Measured before this change on the same fixture shape at 256 × 256: **92 ms** of the route's
    /// own `cost.build_ms` and **2 561 726 B** on the wire, with `source_cells: 65 536` — the whole
    /// grid walked to say *nothing here*. (`crates/hk-api/tests/tile_cost.rs` is that measurement,
    /// committed so it can be re-run rather than quoted.)
    #[test]
    fn an_unobserved_tile_is_answered_from_the_coverage_map_and_no_level_is_consulted() {
        let dir = temp_dir("short-circuit");
        let (state, _, _) = state_with_records(&dir, N as i64, 0);
        // 100 tiles up the frequency axis: inside the record horizon, and no record covers it.
        let v = tiles_json(&state, &tile_params(F_INDEX + 100, T_INDEX)).unwrap();

        assert_eq!(
            v["resolution"]["short_circuit"]["applied"],
            json!(true),
            "{v}"
        );
        assert_eq!(
            v["resolution"]["short_circuit"]["selected_plane_uniform"],
            json!("unobserved"),
            "{v}"
        );
        // Nothing was read, and the answer says so rather than reporting a level that did not run.
        assert_eq!(v["cost"]["source_cells"], json!(0), "{v}");
        assert_eq!(
            v["cost"]["chunks"],
            json!(0),
            "no history lock was taken: {v}"
        );
        assert_eq!(v["resolution"]["answered"], Value::Null, "{v}");
        assert_eq!(v["resolution"]["tried"], json!([]), "{v}");
        assert_eq!(v["resolution"]["candidates"], json!([]), "{v}");

        // **The answer is "unobserved", which is not "quiet" and not "zero".** The grid states one
        // cell, and that cell has NO level — `null`, never a number, and `observed: false`.
        let g = &v["grid"];
        assert_eq!(g["uniform"]["max_db"], Value::Null, "{g}");
        assert_eq!(g["uniform"]["occupancy_max"], Value::Null, "{g}");
        assert_eq!(g["uniform"]["frames"], json!(0), "{g}");
        assert_eq!(g["uniform"]["observed"], json!(false), "{g}");
        assert_eq!(g["observed_cells"], json!(0), "{g}");
        assert_eq!(g["range_db"], Value::Null, "no scale from nothing: {g}");
        assert_eq!(
            g["cells"],
            json!(N * N),
            "the grid is still the tile's own: {g}"
        );
        // The per-cell arrays are ABSENT, not empty and not full of zeroes — an empty array would
        // read as a grid of no cells, and a zero would read as a level.
        for k in ["max_db", "occupancy_max", "coverage", "frames"] {
            assert!(g.get(k).is_none(), "grid still enumerates {k}: {g}");
        }

        // And the plane the client greys from says the same thing, in its own vocabulary.
        assert_eq!(
            v["coverage"]["planes"][v["coverage"]["selected"]["plane"].as_u64().unwrap() as usize]
                ["uniform"],
            json!("unobserved"),
            "{v}"
        );

        // The body is a constant, not a function of `cells`: the same address at 256 × 256 is the
        // same size to within the axis numbers.
        let small = serde_json::to_string(&v).unwrap().len();
        let big = serde_json::to_string(
            &tiles_json(&state, &{
                let mut p = tile_params(
                    (F_INDEX + 100) * N as i64 / TILE_CELLS as i64,
                    T_INDEX * N as i64 / TILE_CELLS as i64,
                );
                p.retain(|(k, _)| k != "cells");
                p.push(("cells".into(), TILE_CELLS.to_string()));
                p
            })
            .unwrap(),
        )
        .unwrap()
        .len();
        eprintln!(
            "T-461 short-circuit body: {small} B at {N}x{N}, {big} B at {TILE_CELLS}x{TILE_CELLS}"
        );
        assert!(
            big < small * 2,
            "a 16x larger tile must not cost 16x the body: {small} B vs {big} B"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **It fails closed, and the closing is load-bearing.**
    ///
    /// Two cases the short-circuit must refuse, and for the second one the refusal is what keeps a
    /// measurement the store holds from being served as nothing:
    ///
    /// 1. an **observed** plane — the ordinary tile, which takes the full read;
    /// 2. an **`"unknown"`** plane (T-423: a row wholly before the record horizon, where no
    ///    surviving record can say either way). `unknown` is not `unobserved`, and here the pyramid
    ///    demonstrably **holds data** for exactly those cells. Had the short-circuit fired on
    ///    `unknown`, this tile would have been served as an empty grid over a measured band — which
    ///    is precisely the defect the ticket named as worse than being slow.
    #[test]
    fn the_short_circuit_refuses_an_observed_plane_and_refuses_unknown_over_data_the_store_holds() {
        // 1. Observed: the full read runs and the tile carries its measurements.
        let dir = temp_dir("closed-observed");
        let (state, _, _) = state_with_records(&dir, N as i64, 0);
        let v = tiles_json(&state, &tile_params(F_INDEX, T_INDEX)).unwrap();
        assert_eq!(
            v["resolution"]["short_circuit"]["applied"],
            json!(false),
            "{v}"
        );
        assert_eq!(
            v["resolution"]["short_circuit"]["selected_plane_uniform"],
            json!("observed"),
            "{v}"
        );
        assert!(v["cost"]["source_cells"].as_u64().unwrap() > 0, "{v}");
        assert!(v["grid"]["observed_cells"].as_u64().unwrap() > 0, "{v}");
        assert!(
            v["grid"]["max_db"].is_array(),
            "the full path enumerates: {v}"
        );
        assert!(v["grid"].get("uniform").is_none(), "{v}");
        let _ = std::fs::remove_dir_all(&dir);

        // 2. Unknown, over a band the pyramid holds frames for. The record horizon is an hour after
        //    the tile, so no surviving record can say whether we looked — and the measurement is
        //    still there to be served.
        let dir = temp_dir("closed-unknown");
        let (state, _, _) = state_with_records(&dir, N as i64, 3600);
        let v = tiles_json(&state, &tile_params(F_INDEX, T_INDEX)).unwrap();
        assert_eq!(
            v["resolution"]["short_circuit"]["selected_plane_uniform"],
            json!("unknown"),
            "the fixture must actually produce a uniformly UNKNOWN plane: {}",
            v["coverage"]
        );
        assert_eq!(
            v["resolution"]["short_circuit"]["applied"],
            json!(false),
            "`unknown` is not `unobserved`, and short-circuiting on it would grey measured \
             spectrum: {v}"
        );
        // The load-bearing half: there really is data here, so the refusal saved something.
        assert!(
            v["grid"]["observed_cells"].as_u64().unwrap() > 0,
            "the mutation control is vacuous unless the store holds data here: {}",
            v["grid"]["observed_cells"]
        );
        assert!(v["grid"]["max_db"].is_array(), "{v}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The equivalence.** The constants [`unobserved_grid_json`] serves are exactly what a real
    /// store read produces for the same address — so the short-circuit is a cheaper *spelling* of
    /// the full path's answer, not a second answer about the same tile.
    #[test]
    fn an_unobserved_tile_read_produces_exactly_the_constants_the_short_circuit_serves() {
        let dir = temp_dir("equivalent");
        let (state, _, _) = state_with_history(&dir, N as i64);
        let g = crate::http::with_history(&state, |p| Ok(p.geometry().clone())).unwrap();
        let key = parse_key(&g, &tile_params(F_INDEX + 100, T_INDEX)).unwrap();
        let r = tile_read(&state, TileStore::Main, &key).unwrap();
        // What the full read actually produced over never-sampled spectrum.
        assert_eq!(r.grid.observed_cells, 0);
        assert_eq!(r.grid.range_db, None);
        assert_eq!(r.grid.unit, hk_model::PowerUnit::Dbfs);
        assert!(
            r.grid.cells.iter().all(|c| {
                c.sources == 0 && c.frames == 0 && c.coverage == 0.0 && !c.max_db.is_finite()
            }),
            "the full read must be uniformly UNOBSERVED for this comparison to mean anything"
        );
        // …and the short form says the same, field for field.
        let short = unobserved_grid_json(&key);
        assert_eq!(short["unit"], grid_json(&r.grid)["unit"]);
        assert_eq!(short["cells"], grid_json(&r.grid)["cells"]);
        assert_eq!(
            short["observed_cells"],
            grid_json(&r.grid)["observed_cells"]
        );
        assert_eq!(short["range_db"], grid_json(&r.grid)["range_db"]);
        assert_eq!(short["semantics"], grid_json(&r.grid)["semantics"]);
        assert_eq!(short["uniform"], uniform_cell_json(&r.grid.cells[0]));
        // The mutation: had the uniform cell been written as zeroes rather than as absences, it
        // would differ — which is what makes the equality above an assertion and not a tautology.
        let zeroed = hk_store::OverviewCell {
            max_db: 0.0,
            occupancy_max: 0.0,
            ..hk_store::OverviewCell::UNOBSERVED
        };
        assert_ne!(
            short["uniform"],
            uniform_cell_json(&zeroed),
            "a level of zero and the absence of a level must not serialise the same"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The cost, measured through the route, before and after, in one run.**
    ///
    /// The short-circuit's floor is the coverage rasterisation itself — which is the point: it
    /// reads the same plane the renderer greys from, so that work is the answer rather than
    /// overhead. What disappears is the `O(cells²)` pyramid walk on top of it.
    #[test]
    fn the_short_circuit_is_measured_against_the_read_it_replaces() {
        let dir = temp_dir("sc-cost");
        let (state, _, _) = state_with_records(&dir, TILE_CELLS as i64, 0);
        let g = crate::http::with_history(&state, |p| Ok(p.geometry().clone())).unwrap();
        let n = 8;
        let time = |f_index: i64| -> f64 {
            let q = tile_params(f_index, T_INDEX);
            let _ = tiles_json(&state, &q).unwrap();
            let started = std::time::Instant::now();
            for _ in 0..n {
                std::hint::black_box(tiles_json(&state, &q).unwrap());
            }
            started.elapsed().as_secs_f64() * 1e3 / n as f64
        };
        // The same address, read the long way, so the comparison is against this machine and this
        // fixture rather than against a number from another run.
        let key = parse_key(&g, &tile_params(F_INDEX + 100, T_INDEX)).unwrap();
        let started = std::time::Instant::now();
        for _ in 0..n {
            std::hint::black_box(
                tile_read(&state, TileStore::Main, &key)
                    .unwrap()
                    .source_cells,
            );
        }
        let full_read_ms = started.elapsed().as_secs_f64() * 1e3 / n as f64;
        let short_ms = time(F_INDEX + 100);
        let observed_ms = time(F_INDEX);
        eprintln!(
            "T-461 at {N}x{N}: pyramid read alone over never-sampled spectrum {full_read_ms:.2} ms; \
             whole short-circuited answer {short_ms:.2} ms; whole observed answer {observed_ms:.2} ms"
        );
        assert!(
            short_ms < observed_ms,
            "the short-circuited answer must be cheaper than the full one: {short_ms:.2} vs {observed_ms:.2} ms"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The number this route is designed against.** T-437 measured rendering at p95 2.2 ms for
    /// 48 panes and tile *production* at ~500 ms per tile over HTTP — three orders of magnitude
    /// apart — so production is where the budget goes. Measured here through the real store and
    /// the real fold; the assertion is a generous ceiling, the print is the measurement.
    #[test]
    fn tile_production_cost_is_measured_and_stays_inside_the_screen_budget() {
        let dir = temp_dir("cost");
        let (state, _, _) = state_with_history(&dir, TILE_CELLS as i64);
        let g = crate::http::with_history(&state, |p| Ok(p.geometry().clone())).unwrap();
        let n = 16;
        let mut worst: f64 = 0.0;
        for &cells in &[N, TILE_CELLS] {
            // The canvas's own unit is 256 x 256 (docs/16 §6.2); N is the same read an eighth of
            // the area, so the pair says whether the cost is in the fold or in the fixed overhead.
            let t_index = T_INDEX * N as i64 / cells as i64;
            let f_index = F_INDEX * N as i64 / cells as i64;
            let started = std::time::Instant::now();
            for i in 0..n {
                let key = parse_key(
                    &g,
                    &params(&[
                        ("level_f", "0"),
                        ("level_t", "0"),
                        ("f_index", &(f_index + i % 3).to_string()),
                        ("t_index", &t_index.to_string()),
                        ("cells", &cells.to_string()),
                    ]),
                )
                .unwrap();
                std::hint::black_box(
                    tile_read(&state, TileStore::Main, &key)
                        .unwrap()
                        .grid
                        .observed_cells,
                );
            }
            let mean_ms = started.elapsed().as_secs_f64() * 1e3 / n as f64;
            eprintln!(
                "T-438 tile production: {mean_ms:.2} ms mean over {n} tiles of {cells}x{cells} cells"
            );
            worst = worst.max(mean_ms);
        }
        assert!(
            worst < 250.0,
            "tile production regressed to {worst:.1} ms/tile"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T-519's fixture: band X (tile `F_INDEX`) holds a -60 dB carrier over noise for the first
    /// `x_secs` of tile `T_INDEX`, then is departed; band Y (tile `F_INDEX + 2`) stays observed for
    /// two whole tiles, so the store's data edge is the end of tile `T_INDEX + 1`. Tile
    /// `F_INDEX + 1` is never observed at all.
    fn state_swept_then_departed(dir: &std::path::Path, x_secs: i64) -> (ApiState, i64) {
        let mut p = hk_store::Pyramid::open(dir, PyramidConfig::default()).unwrap();
        let g = p.geometry().clone();
        let (t_cell, f_cell) = (g.levels[0].t_cell_ns, g.levels[0].f_cell_hz);
        let t0 = T_INDEX * t_cell * N as i64;
        const NB: usize = 128;
        let bin_hz = f_cell * N as f64 / NB as f64;
        for k in 0..2 * N as i64 {
            let mut psd = [1e-12f32; NB];
            psd[40] = 1e-6;
            let bands = if k < x_secs {
                &[0i64, 2][..]
            } else {
                &[2i64][..]
            };
            for &b in bands {
                p.ingest(&hk_store::history::FrameInput::new(
                    Timestamp::from_unix_nanos(t0 + k * t_cell),
                    t_cell,
                    (F_INDEX + b) as f64 * f_cell * N as f64,
                    bin_hz,
                    hk_model::PowerUnit::Dbfs,
                    &psd,
                ))
                .unwrap();
            }
        }
        let state = ApiState {
            history: Some(Arc::new(std::sync::Mutex::new(p))),
            ..ApiState::default()
        };
        (state, t0)
    }

    /// Expands the `shadow` block's runs into a per-cell `(last_db, last_t_s, source f cell)` plane;
    /// the source's frequency cell is the tile's own for a value this tile holds.
    fn shadow_plane(v: &Value) -> Vec<Option<(f64, f64, f64)>> {
        let sh = &v["shadow"];
        assert_eq!(sh["encoding"], json!("column-runs"), "{sh}");
        let n = v["extent"]["nf"].as_u64().unwrap() as usize;
        let tile_f_cell = v["extent"]["f_cell_hz"].as_f64().unwrap();
        let mut out = vec![None; n * n];
        let arr = |k: &str| sh[k].as_array().unwrap().clone();
        let (f, row, rows, db, t, src) = (
            arr("f"),
            arr("row"),
            arr("rows"),
            arr("last_db"),
            arr("last_t_s"),
            arr("src"),
        );
        assert_eq!(sh["runs"].as_u64().unwrap() as usize, f.len());
        for i in 0..f.len() {
            let source = &sh["sources"][src[i].as_u64().unwrap() as usize];
            assert!(source.is_object(), "src indexes `sources`: {sh}");
            let f_cell = source["f_cell_hz"].as_f64().unwrap_or(tile_f_cell);
            let c = f[i].as_u64().unwrap() as usize;
            let r0 = row[i].as_u64().unwrap() as usize;
            for r in r0..r0 + rows[i].as_u64().unwrap() as usize {
                assert!(out[r * n + c].is_none(), "runs overlap at ({r}, {c})");
                out[r * n + c] = Some((db[i].as_f64().unwrap(), t[i].as_f64().unwrap(), f_cell));
            }
        }
        out
    }

    /// **T-519, the user's three states.** Swept then departed = a shadow carrying the value last
    /// seen, with when; never swept = NO shadow; observed now = its own value, never a shadow.
    #[test]
    fn a_departed_band_carries_its_last_known_value_and_a_never_swept_one_carries_none() {
        let dir = temp_dir("shadow");
        let half = N as i64 / 2;
        let h = half as usize;
        let (state, t0) = state_swept_then_departed(&dir, half);
        let (n, t0_s) = (N, t0 as f64 / 1e9);

        // The tile band X was seen in, for its first half.
        let here = tiles_json(&state, &tile_params(F_INDEX, T_INDEX)).unwrap();
        let grid = here["grid"]["max_db"].as_array().unwrap().clone();
        let plane = shadow_plane(&here);
        for f in 0..n {
            let seen = grid[(h - 1) * n + f].as_f64().expect("row half-1 measured");
            for r in 0..n {
                if r < h {
                    // Observed now: the grid's value stands and NO shadow is laid over it.
                    assert!(grid[r * n + f].is_number(), "({r}, {f})");
                    assert_eq!(
                        plane[r * n + f],
                        None,
                        "a shadow over a measurement ({r}, {f})"
                    );
                } else {
                    // Departed part-way down this tile: the value it was last seen with, and when.
                    assert!(grid[r * n + f].is_null());
                    let (db, t, _) = plane[r * n + f].expect("shadow below the departure");
                    assert_eq!((db, t), (seen, t0_s + half as f64), "({r}, {f})");
                }
            }
        }

        // The next tile up: band X was never seen in it, and every cell carries what it was last
        // seen with — the SAME value the tile below measured (T-315: assert the value), at the
        // resolution of the level that held it, which `sources` states: a source cell k tile
        // columns wide carries the max-hold of those k columns, replicated (T-334's direction).
        let next = tiles_json(&state, &tile_params(F_INDEX, T_INDEX + 1)).unwrap();
        assert_eq!(next["grid"]["observed_cells"], json!(0), "{}", next["grid"]);
        let plane = shadow_plane(&next);
        let carrier = grid[(h - 1) * n + 20].as_f64().unwrap();
        let noise = grid[(h - 1) * n].as_f64().unwrap();
        assert!(
            carrier > noise + 30.0,
            "the fixture's carrier: {carrier} vs {noise}"
        );
        let tile_f_cell = next["extent"]["f_cell_hz"].as_f64().unwrap();
        for r in 0..n {
            for f in 0..n {
                let (db, t, f_cell) = plane[r * n + f].expect("every cell of the departed band");
                let k = (f_cell / tile_f_cell).round().max(1.0) as usize;
                let g = f / k * k;
                let expect = (g..g + k)
                    .map(|c| grid[(h - 1) * n + c].as_f64().unwrap())
                    .fold(f64::NEG_INFINITY, f64::max);
                assert_eq!(db, expect, "({r}, {f}) from a {f_cell} Hz source");
                let (lo, hi) = (t0_s + half as f64, t0_s + n as f64);
                assert!(t >= lo && t <= hi, "last seen {t} outside [{lo}, {hi}]");
            }
        }
        assert!(
            plane
                .iter()
                .any(|c| c.is_some_and(|(db, _, _)| db == carrier))
        );
        assert_eq!(next["shadow"]["search"]["columns_found"], json!(n));

        // Never swept: NO shadow anywhere, and the search says it found nothing — grey stays grey.
        let never = tiles_json(&state, &tile_params(F_INDEX + 1, T_INDEX + 1)).unwrap();
        assert_eq!(never["shadow"]["runs"], json!(0), "{}", never["shadow"]);
        assert_eq!(never["shadow"]["search"]["columns_found"], json!(0));
        assert!(shadow_plane(&never).iter().all(Option::is_none));

        // Past the data edge nothing is carried: every row of the tile after the newest frame is
        // the future, so it carries nothing even over band X.
        let future = tiles_json(&state, &tile_params(F_INDEX, T_INDEX + 2)).unwrap();
        assert_eq!(future["shadow"]["runs"], json!(0), "{}", future["shadow"]);

        eprintln!(
            "T-519 shadow: departed tile {} runs, {} B shadow block, search {} ms / {} cells",
            next["shadow"]["runs"],
            next["shadow"].to_string().len(),
            next["shadow"]["search"]["build_ms"],
            next["shadow"]["search"]["source_cells"],
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
