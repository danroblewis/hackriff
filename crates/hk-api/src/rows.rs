//! `GET /ws/tiles/rows` — rows **pushed** as they are recorded, to a subscription over an
//! **address range** of the tile lattice (T-468, the durable half of T-460).
//!
//! # The subscription is a range, never "the live stream"
//!
//! A subscriber names a column of the same `(scheme, device, level_f, f_index, level_t, cells)`
//! lattice [`crate::tiles`] serves, and a **row range** on its time axis: `t_from` (required) and
//! `t_to` (optional). Row `r` at `level_t` is the time cell `[r·T, (r+1)·T)` from the Unix epoch,
//! `T` that level's cell — so row `r` is row `r mod cells` of tile `t_index = r div cells`, and a
//! client can key everything it receives by the **tile address** it already uses.
//!
//! There is no parameter that means *now*, and no default that stands in for one: a subscription
//! without `t_from` is refused. That is the whole design, not a detail of it. Historical playback
//! (T-463) is a **reader walking forward** through sealed history at a rate of its choosing, not a
//! producer appending; a route that could only mean "the live stream" could not serve it, and
//! playback would have to grow a second mechanism beside this one — the two-implementations drift
//! that T-420, T-388, T-397 and T-412 each were. "Live" is only what happens when a range's end is
//! past the data edge: the route delivers what is complete and then waits for the rest to be
//! recorded. The store already works this way — tile addressing is absolute and edge-free, and a
//! live edge is the derived predicate `block_end > watermark`, never a mode — and so does this.
//!
//! # What is pushed, and when
//!
//! A row is pushed **once, when it is complete**: when the store's data edge (the end of the newest
//! folded frame, or the watermark if later) has passed the row's end. At the finest level that is
//! one display row, so rows append as they are recorded — the classic-waterfall invariant, never
//! gated on a tile being buildable. A coarser `level_t` row is a fold of `2^level_t` finest rows and
//! is pushed when its whole cell has passed: the "commits every N" rule, stated by the address.
//! Each block says whether it is **`final`** — its end at or before the watermark, so no late frame
//! can still land in it — or provisional; a provisional row may still be amended by a late frame
//! and the sealed tile from `GET /api/tiles` is then the authority. Nothing is pushed twice.
//!
//! Blocks never cross a tile boundary, so each `rows` message patches exactly one tile. A stretch
//! the **coverage map** says is unobserved for the selected device is answered as one `unobserved`
//! message over the whole stretch without reading the store (T-461's short-circuit, applied to a
//! range), so a sealed range over months of an un-tuned band costs a handful of messages rather
//! than millions of empty rows.
//!
//! # Cost
//!
//! Every block is read under the same per-chunk lock discipline as a tile read, through the same
//! level choice (finest affordable, walking coarser only when a level holds nothing — T-426), so a
//! subscription cannot hold the history mutex longer than one tile chunk does. The server never
//! paces a sealed range: it writes as fast as the socket takes it, and a reader that wants to walk
//! slower asks for a shorter range. Subscriptions are capped per server
//! ([`MAX_ROW_FEEDS`]); a subscription holds no tile in-flight slot, because it is long-lived and a
//! slot is the unit of a *request*.

use std::io::{self, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use hk_model::{FreqRange, TimeRange, Timestamp};
use hk_store::{Overview, OverviewCell, RegionQuery, Resolution};
use serde_json::{Value, json};
use tungstenite::Message;
use tungstenite::protocol::frame::CloseFrame;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::{Role, WebSocket};

use crate::http::ApiState;
use crate::query::{ApiError, Params, Region, bad, param};
use crate::tiles::{
    TileKey, TileStore, affordable_levels, axis_fold, chunk_rows, num, parse_key, read_order_until,
    servable, store_name, tier_of, tile_store, with_tile_history, with_tile_history_built,
};

/// Rows in one `rows` message at most. Small enough that a block's coverage plane is always laid
/// on its own axes (64 × 256 is well inside `MAX_COVERAGE_GRID_CELLS`), large enough that a sealed
/// range streams in a few hundred messages per tile-column-hour at the finest level.
pub const ROW_BLOCK: usize = 64;

/// Rows the first coverage probe spans before the store is read. A stretch the coverage map calls
/// uniformly unobserved is one `unobserved` message, and while consecutive probes keep finding grey
/// the span **doubles**, so a range starting decades before any capture (`t_from=0`) reaches the
/// first recorded row in a few dozen messages rather than billions of empty ones. The first probe
/// that finds anything resets it.
pub const GAP_PROBE_ROWS: i64 = 4096;

/// Row subscriptions open at once, per server. Past it the handshake is refused `503`.
pub const MAX_ROW_FEEDS: usize = 16;

/// Bounds on how often a waiting subscription looks at the data edge. The look is one short lock
/// hold to read two timestamps.
const MIN_TICK: Duration = Duration::from_millis(5);
const MAX_TICK: Duration = Duration::from_millis(250);

/// Query parameters this route accepts.
const ALLOWED: [&str; 9] = [
    "device", "scheme", "level_f", "level_t", "f_index", "cells", "t_from", "t_to", "token",
];

/// A subscription: one tile column of the lattice and a row range on its time axis.
#[derive(Clone, Debug)]
pub struct RowSubscription {
    /// Which store answers (the same rule `/api/tiles` uses).
    pub store: TileStore,
    /// The address of the tile holding `from`; every other tile of the range is this key with
    /// another `t_index`.
    pub key: TileKey,
    /// First row, inclusive, on `level_t`'s axis from the epoch.
    pub from: i64,
    /// Row after the last, or `None` for a range that runs on until the subscriber leaves.
    pub to: Option<i64>,
    /// Affordable store levels, finest first (the tile read's own candidate list).
    pub candidates: Vec<u8>,
}

fn row_arg(q: &Params, k: &'static str) -> Result<Option<i64>, ApiError> {
    match param(q, k) {
        None => Ok(None),
        Some(raw) => raw
            .parse::<i64>()
            .ok()
            .filter(|v| *v >= 0)
            .map(Some)
            .ok_or_else(|| bad(&format!("{k} must be a row address: an integer >= 0"))),
    }
}

/// Parses a subscription. **`t_from` is required and nothing defaults it** — see the module docs.
pub fn parse_subscription(state: &ApiState, q: &Params) -> Result<RowSubscription, ApiError> {
    if param(q, "t_index").is_some() {
        return Err(bad(
            "t_index is a single tile; a row subscription is a RANGE — give t_from (and \
             optionally t_to) as row addresses on level_t's axis",
        ));
    }
    if let Some((k, _)) = q.iter().find(|(k, _)| !ALLOWED.contains(&k.as_str())) {
        return Err(bad(&format!(
            "unknown parameter {k:?} (allowed: device, scheme, level_f, level_t, f_index, cells, \
             t_from, t_to)"
        )));
    }
    let from = row_arg(q, "t_from")?.ok_or_else(|| {
        bad(
            "t_from is required: a row subscription is an ADDRESS RANGE [t_from, t_to) on the \
             lattice's time axis, never \"the live stream\" — there is no implicit now. To follow \
             the growing edge, start the range at the row you have and leave t_to open.",
        )
    })?;
    let to = row_arg(q, "t_to")?;
    if to.is_some_and(|t| t <= from) {
        return Err(bad("t_to must be greater than t_from"));
    }
    let store = tile_store(state, q);
    // The tile holding `from`, parsed by the tile route's own parser so the two routes cannot
    // disagree about what an address is.
    let cells = param(q, "cells")
        .and_then(|c| c.parse::<i64>().ok())
        .filter(|c| *c > 0)
        .unwrap_or(crate::tiles::TILE_CELLS as i64);
    let mut tq: Vec<(String, String)> = q
        .iter()
        .filter(|(k, _)| k != "t_from" && k != "t_to" && k != "token")
        .cloned()
        .collect();
    tq.push(("t_index".into(), from.div_euclid(cells).to_string()));
    let (key, candidates) = with_tile_history(state, store, |p| {
        let key = parse_key(p.geometry(), &tq)?;
        if !servable(p, &key) {
            return Err(ApiError::new(
                400,
                "no store level can back this address inside the work budget — \
                 `axes.*.max_level` on /api/tiles states how far up the lattice can be read",
            ));
        }
        // The affordable set, finest first. The ORDER a block walks it in is decided per block,
        // when the block is read (`read_order_until`, T-1018): whether the coarse nodes have
        // folded a block's rows yet changes as the run goes on.
        let c: Vec<u8> = affordable_levels(p, &key)
            .into_iter()
            .map(|l| l as u8)
            .collect();
        Ok((key, c))
    })?;
    if let Some(t) = to
        && i128::from(t) * i128::from(key.t_cell_ns) > i128::from(i64::MAX) / 2
    {
        return Err(bad("t_to is outside the addressable time range"));
    }
    Ok(RowSubscription {
        store,
        key,
        from,
        to,
        candidates,
    })
}

impl RowSubscription {
    fn t_cell(&self) -> i64 {
        self.key.t_cell_ns
    }

    fn row_ns(&self, row: i64) -> i64 {
        row.saturating_mul(self.t_cell())
    }

    fn cells(&self) -> i64 {
        self.key.cells as i64
    }

    /// The key of the tile holding `row`.
    fn key_at(&self, row: i64) -> TileKey {
        let t_index = row.div_euclid(self.cells());
        let span = self.t_cell() * self.cells();
        let mut k = self.key.clone();
        k.t_index = t_index;
        k.region = Region {
            freq: k.region.freq,
            t0_ns: t_index * span,
            t1_ns: (t_index + 1) * span,
        };
        k
    }

    /// The first message: what was subscribed, stated back, so a client never has to remember
    /// what it asked for to interpret what it gets.
    pub fn subscribed_json(&self, state: &ApiState) -> Value {
        let (edge, watermark) = edges(state, self.store).unwrap_or((None, None));
        let k = &self.key;
        json!({
            "type": "subscribed",
            "address": {
                "scheme": k.lattice.name,
                "device": k.device,
                "level_f": k.level_f,
                "level_t": k.level_t,
                "f_index": k.f_index,
                "cells": k.cells,
            },
            "range": {
                "t_from": self.from,
                "t_to": self.to,
                "t0_s": self.row_ns(self.from) as f64 / 1e9,
                "t1_s": self.to.map(|t| self.row_ns(t) as f64 / 1e9),
                "open": self.to.is_none(),
            },
            "extent": {
                "f_lo_hz": k.region.freq.lo_hz,
                "f_hi_hz": k.region.freq.hi_hz,
                "f_cell_hz": k.f_cell_hz,
                "t_cell_s": k.t_cell_ns as f64 / 1e9,
                "nf": k.cells,
            },
            "store": store_name(self.store),
            "candidates": self.candidates,
            "data_edge_s": edge.map(|e| e as f64 / 1e9),
            "watermark_s": watermark.map(|w| w as f64 / 1e9),
            "rule": "row r is the time cell [r*t_cell, (r+1)*t_cell) from the epoch: row r mod \
                cells of tile t_index = r div cells. A row is pushed ONCE, when the data edge has \
                passed its end; `final` says no late frame can still land in it. The range is the \
                subscription: rows already recorded arrive at once, rows not yet recorded arrive \
                as they are, and the range ends at t_to (`end`) or never. Grey is decided by each \
                block's `coverage`, exactly as on /api/tiles.",
        })
    }
}

/// `(data edge, watermark)` of the store, ns. The data edge is the end of the newest folded frame.
fn edges(state: &ApiState, store: TileStore) -> Result<(Option<i64>, Option<i64>), ApiError> {
    with_tile_history(state, store, |p| {
        let w = p.watermark().as_unix_nanos();
        Ok((
            p.latest_frame_end().map(|t| t.as_unix_nanos()),
            (w > i64::MIN / 2).then_some(w),
        ))
    })
}

/// What one step of a cursor produced.
#[derive(Debug)]
pub enum Step {
    /// A message to send; more may be ready at once.
    Send(Value),
    /// Nothing complete yet: the range runs past the data edge.
    Wait,
    /// The range is exhausted; send this and close.
    End(Value),
}

/// A reader walking forward through one subscription's range. **Nothing in it knows or cares
/// whether the rows it is reading are "live"** — the same `step` serves a range sealed a month ago
/// and one that is still being recorded, which is the property this route exists to keep.
#[derive(Debug)]
pub struct RowCursor {
    sub: RowSubscription,
    next: i64,
    /// Rows the next coverage probe spans ([`GAP_PROBE_ROWS`], doubling across grey).
    gap_span: i64,
    /// How far forward the tune record is known to reach, ns — cached because it only grows, so
    /// a sealed walk far behind it never re-reads the record ([`Self::record_reach`]).
    reach_ns: Option<i64>,
}

impl RowCursor {
    /// A cursor at the start of `sub`'s range.
    pub fn new(sub: RowSubscription) -> Self {
        let next = sub.from;
        Self {
            sub,
            next,
            gap_span: GAP_PROBE_ROWS,
            reach_ns: None,
        }
    }

    /// **How far forward the tune record reaches, over any band** — the evidence grey is decided
    /// from, and the route's own `as_of_s` rule (T-532) applied to a stream (T-468 review).
    ///
    /// A row is pushed once and never again, so its coverage must be *final* when it goes: a row
    /// past the newest tune record would be rasterised `unobserved` because the record has not
    /// reached it yet, not because nothing looked — and with the IQ ring refused (T-588/T-596)
    /// the record's live edge is the open dwell, which trails the spectrum by up to a control
    /// tick. So the cursor delivers nothing past this instant. Over **any** band, because a record
    /// that reaches past a row elsewhere proves the absence here is real: the radio was somewhere
    /// else. `Ok(None)` means wait (a record source exists and has reached nothing yet);
    /// `Ok(Some(i64::MAX))` means this server keeps no tune record at all, so every plane is
    /// uniformly `unobserved` and nothing will ever change that.
    fn record_reach(&mut self, state: &ApiState, want_ns: i64) -> Option<i64> {
        if let Some(r) = self.reach_ns
            && r >= want_ns
        {
            return Some(r);
        }
        let from = self.reach_ns.unwrap_or_else(|| self.sub.row_ns(self.next));
        let ev = crate::coverage::Evidence::collect(
            state,
            FreqRange::new(0.0, 1e12),
            TimeRange::new(
                Timestamp::from_unix_nanos(from),
                Timestamp::from_unix_nanos(i64::MAX / 4),
            ),
        );
        if !ev.has_source() {
            self.reach_ns = Some(i64::MAX);
            return self.reach_ns;
        }
        if let Some(t) = ev.newest_record {
            let t = t.as_unix_nanos();
            self.reach_ns = Some(self.reach_ns.map_or(t, |r| r.max(t)));
        }
        self.reach_ns
    }

    /// The subscription.
    pub fn subscription(&self) -> &RowSubscription {
        &self.sub
    }

    /// The next row this cursor will deliver — **this subscription's own edge**, which is why a
    /// client with two panes has two of them.
    pub fn next_row(&self) -> i64 {
        self.next
    }

    /// Advances by at most one message.
    pub fn step(&mut self, state: &ApiState) -> Result<Step, ApiError> {
        let s = &self.sub;
        if let Some(to) = s.to
            && self.next >= to
        {
            return Ok(Step::End(json!({
                "type": "end",
                "row": to,
                "reason": "range-complete",
            })));
        }
        let (edge, watermark) = edges(state, s.store)?;
        let edge = match (edge, watermark) {
            (Some(a), Some(b)) => a.max(b),
            (a, b) => match a.or(b) {
                Some(x) => x,
                None => return Ok(Step::Wait),
            },
        };
        // Row r is complete iff (r + 1)·T <= edge.
        let t_cell = s.t_cell();
        let complete = edge.div_euclid(t_cell);
        let limit = s.to.map_or(complete, |t| t.min(complete));
        if self.next >= limit {
            return Ok(Step::Wait);
        }
        // ...and no row the tune record has not reached yet (see `record_reach`).
        let want = limit.saturating_mul(t_cell);
        let Some(reach) = self.record_reach(state, want) else {
            return Ok(Step::Wait);
        };
        let limit = limit.min(reach.div_euclid(t_cell));
        if self.next >= limit {
            return Ok(Step::Wait);
        }
        let s = &self.sub;
        let freq = s.key.region.freq;
        // T-461 applied to a range: a stretch the coverage map calls unobserved for the selected
        // device is answered from the map alone, as one message.
        let probe_end = limit.min(self.next.saturating_add(self.gap_span));
        let probe_rows = (probe_end - self.next) as usize;
        let probe = crate::coverage::TileOverlay::collect_once(
            state,
            freq,
            self.window(self.next, probe_end),
            probe_rows.min(crate::tiles::TILE_CELLS),
            s.key.cells,
        );
        if probe.uniform_state(&s.key.device) == Some("unobserved") {
            let v = json!({
                "type": "unobserved",
                "row0": self.next,
                "rows": probe_end - self.next,
                "t0_s": s.row_ns(self.next) as f64 / 1e9,
                "t1_s": s.row_ns(probe_end) as f64 / 1e9,
                "final": watermark.is_some_and(|w| s.row_ns(probe_end) <= w),
                "rule": "the coverage map says nothing sampled this stretch for the selected \
                    device: grey, and no measurement. Answered from the map alone (T-461); it may \
                    span several tiles.",
            });
            self.next = probe_end;
            self.gap_span = self.gap_span.saturating_mul(2);
            return Ok(Step::Send(v));
        }
        self.gap_span = GAP_PROBE_ROWS;
        let tile_end = (self.next.div_euclid(s.cells()) + 1) * s.cells();
        let block_end = limit.min(self.next + ROW_BLOCK as i64).min(tile_end);
        let v = self.block(state, self.next, block_end, watermark)?;
        self.next = block_end;
        Ok(Step::Send(v))
    }

    fn window(&self, a: i64, b: i64) -> TimeRange {
        TimeRange::new(
            Timestamp::from_unix_nanos(self.sub.row_ns(a)),
            Timestamp::from_unix_nanos(self.sub.row_ns(b)),
        )
    }

    /// One block of rows `[a, b)`, inside one tile.
    fn block(
        &self,
        state: &ApiState,
        a: i64,
        b: i64,
        watermark: Option<i64>,
    ) -> Result<Value, ApiError> {
        let s = &self.sub;
        let nrows = (b - a) as usize;
        let key = s.key_at(a);
        let overlay = crate::coverage::TileOverlay::collect_once(
            state,
            key.region.freq,
            self.window(a, b),
            nrows,
            key.cells,
        );
        // The tile read's rule: `read_order` (the exact node, else the cheapest level that folds),
        // walk on only when a level holds nothing, and say which answered.
        let mut tried = Vec::new();
        let mut first: Option<(u8, Overview)> = None;
        let mut answered: Option<(u8, Overview)> = None;
        let order: Vec<u8> = with_tile_history(state, s.store, |p| {
            Ok(read_order_until(p, &key, s.row_ns(b))
                .into_iter()
                .map(|l| l as u8)
                .collect())
        })?;
        for &level in &order {
            let o = read_rows(state, s.store, &key, level, self.window(a, b), nrows)?;
            tried.push(level);
            if o.observed_cells > 0 {
                answered = Some((level, o));
                break;
            }
            if first.is_none() {
                first = Some((level, o));
            }
        }
        let (level, o) = answered
            .or(first)
            .ok_or_else(|| ApiError::new(400, "no store level can back this address"))?;
        let (lf, lt) = with_tile_history(state, s.store, |p| {
            let g = &p.geometry().levels[usize::from(level)];
            Ok((g.f_cell_hz, g.t_cell_ns))
        })?;
        // The honesty tier of THIS block, by the tile route's own rule over the level that answered
        // it (T-902). Without it a client building a tile from pushed rows had to guess one.
        let source = tier_of(&key, lf, lt, crate::http::max_live_span_hz(state));
        let max_db: Vec<Value> = o
            .cells
            .iter()
            .map(|c: &OverviewCell| {
                if c.sources == 0 {
                    Value::Null
                } else {
                    num(c.max_db)
                }
            })
            .collect();
        Ok(json!({
            "type": "rows",
            "row0": a,
            "rows": nrows,
            "tile": { "t_index": key.t_index, "row": a - key.t_index * s.cells() },
            "t0_s": s.row_ns(a) as f64 / 1e9,
            "t_cell_s": s.t_cell() as f64 / 1e9,
            "nf": key.cells,
            // Row-major, rows then frequency, exactly `/api/tiles`' `grid.max_db` layout. `null` is
            // not measured, never quiet; grey is `coverage`'s to decide.
            "max_db": max_db,
            "observed_cells": o.observed_cells,
            "coverage": overlay.selected_plane_json(&key.device),
            "answered": {
                "level": level,
                "store": store_name(s.store),
                "f_cell_hz": lf,
                "t_cell_s": lt as f64 / 1e9,
                "tried": tried,
            },
            // Exactly `/api/tiles`' `resolution.{source,live,statement,fold}`, stated for this
            // block's rows: the level they were actually measured at, never inferred by the reader
            // from a neighbouring tile (T-902). `fold.time.served` is this block's row count.
            "resolution": {
                "source": source.as_str(),
                "live": source.is_live(),
                "statement": source.statement(),
                "fold": {
                    "frequency": axis_fold(lf, key.f_cell_hz, o.src_nf, key.cells),
                    "time": axis_fold(lt as f64, key.t_cell_ns as f64, o.src_nt, nrows),
                },
            },
            "final": watermark.is_some_and(|w| s.row_ns(b) <= w),
        }))
    }
}

/// Reads `nrows` output rows over `window` at `level`, chunked so no single history lock hold
/// exceeds one tile chunk's ([`chunk_rows`]).
fn read_rows(
    state: &ApiState,
    store: TileStore,
    key: &TileKey,
    level: u8,
    window: TimeRange,
    nrows: usize,
) -> Result<Overview, ApiError> {
    let per = with_tile_history(state, store, |p| {
        Ok(chunk_rows(p.geometry(), key, usize::from(level)))
    })?
    .max(1);
    let t0 = window.start.as_unix_nanos();
    let cells = key.cells;
    let freq: FreqRange = key.region.freq;
    let mut out = vec![OverviewCell::UNOBSERVED; nrows * cells];
    let mut unit = None;
    let (mut src_nt, mut src_nf) = (0, 0);
    let mut row = 0usize;
    while row < nrows {
        let hi = (row + per).min(nrows);
        let chunk = TimeRange::new(
            Timestamp::from_unix_nanos(t0 + row as i64 * key.t_cell_ns),
            Timestamp::from_unix_nanos(t0 + hi as i64 * key.t_cell_ns),
        );
        let part = with_tile_history_built(state, store, level, freq, chunk, |p| {
            let h = p
                .query(&RegionQuery {
                    freq,
                    time: chunk,
                    resolution: Resolution::Level(level),
                })
                .map_err(|_| ApiError::new(400, "row query refused"))?;
            Ok(h.overview(chunk, freq, hi - row, cells))
        })?;
        unit.get_or_insert(part.unit);
        src_nt += part.src_nt;
        src_nf = part.src_nf;
        out[row * cells..hi * cells].copy_from_slice(&part.cells);
        row = hi;
    }
    let observed_cells = out.iter().filter(|c| c.sources > 0).count();
    Ok(Overview {
        unit: unit.unwrap_or(hk_model::PowerUnit::Dbfs),
        nt: nrows,
        nf: cells,
        t0_ns: t0,
        t_cell_ns: key.t_cell_ns as f64,
        f_lo_hz: freq.lo_hz,
        f_cell_hz: key.f_cell_hz,
        cells: out,
        observed_cells,
        src_nt,
        src_nf,
        range_db: None,
    })
}

// ---------------------------------------------------------------------------------------------
// The WebSocket front end.
// ---------------------------------------------------------------------------------------------

/// Counts open subscriptions against [`MAX_ROW_FEEDS`]; released on drop.
struct FeedSlot<'a>(&'a AtomicUsize);

impl<'a> FeedSlot<'a> {
    fn take(n: &'a AtomicUsize) -> Option<Self> {
        n.fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
            (v < MAX_ROW_FEEDS).then_some(v + 1)
        })
        .ok()
        .map(|_| Self(n))
    }
}

impl Drop for FeedSlot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn http_error(stream: &mut TcpStream, status: u16, message: &str) {
    let body = json!({ "error": message }).to_string();
    let head = format!(
        "HTTP/1.1 {status} Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\nCache-Control: no-store\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

fn close(ws: &mut WebSocket<TcpStream>, code: u16, reason: &str) {
    let mut end = reason.len().min(120);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    let _ = ws.close(Some(CloseFrame {
        code: CloseCode::from(code),
        reason: reason[..end].to_owned().into(),
    }));
    let _ = ws.get_mut().set_read_timeout(Some(Duration::from_secs(2)));
    while ws.read().is_ok() {}
    let _ = ws.get_mut().shutdown(Shutdown::Both);
}

/// Completes the upgrade, sends `{"type":"refused",…}` and closes with `4000 + status` — the
/// `/ws/open/{name}` convention, because a browser cannot read an HTTP error body on a failed
/// upgrade.
fn refuse(mut ws: WebSocket<TcpStream>, e: &ApiError) {
    let _ = ws.send(Message::Text(
        json!({ "type": "refused", "status": e.status, "reason": e.message })
            .to_string()
            .into(),
    ));
    close(&mut ws, 4000 + e.status, &e.message);
}

fn send(ws: &mut WebSocket<TcpStream>, v: &Value) -> bool {
    ws.send(Message::Text(v.to_string().into())).is_ok()
}

/// `true` while the peer is still there. Reads (and so answers pings) for at most `wait`.
fn peer_alive(ws: &mut WebSocket<TcpStream>, wait: Duration) -> bool {
    let _ = ws.get_mut().set_read_timeout(Some(wait));
    match ws.read() {
        Ok(Message::Close(_)) => false,
        // A subscriber never sends data; anything else (a ping, a pong) is housekeeping.
        Ok(_) => true,
        Err(tungstenite::Error::Io(e))
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
            ) =>
        {
            true
        }
        Err(_) => false,
    }
}

/// Serves one `/ws/tiles/rows` request (token already verified).
pub(crate) fn serve(
    mut stream: TcpStream,
    state: &ApiState,
    query: &Params,
    headers: &[(String, String)],
) {
    let header = |n: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(n))
            .map(|(_, v)| v.trim())
    };
    let has = |n: &str, want: &str| {
        header(n).is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(want)))
    };
    if !has("upgrade", "websocket") || !has("connection", "upgrade") {
        return http_error(&mut stream, 426, "WebSocket upgrade required");
    }
    if header("sec-websocket-version") != Some("13") {
        return http_error(&mut stream, 426, "WebSocket version 13 required");
    }
    let Some(key) = header("sec-websocket-key") else {
        return http_error(&mut stream, 400, "missing Sec-WebSocket-Key");
    };
    let handshake = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\r\n",
        tungstenite::handshake::derive_accept_key(key.as_bytes())
    );
    if stream.write_all(handshake.as_bytes()).is_err() {
        return;
    }
    let _ = stream.set_write_timeout(Some(Duration::from_secs(20)));
    let mut ws = WebSocket::from_raw_socket(stream, Role::Server, None);
    let Some(_slot) = FeedSlot::take(&state.row_feeds) else {
        return refuse(
            ws,
            &ApiError::new(
                503,
                format!("{MAX_ROW_FEEDS} row subscriptions are already open on this server"),
            ),
        );
    };
    let sub = match parse_subscription(state, query) {
        Ok(s) => s,
        Err(e) => return refuse(ws, &e),
    };
    let tick = Duration::from_nanos(u64::try_from(sub.t_cell() / 4).unwrap_or(u64::MAX))
        .clamp(MIN_TICK, MAX_TICK);
    if !send(&mut ws, &sub.subscribed_json(state)) {
        return;
    }
    let mut cursor = RowCursor::new(sub);
    let mut sent = 0u32;
    loop {
        match cursor.step(state) {
            Ok(Step::Send(v)) => {
                if !send(&mut ws, &v) {
                    return;
                }
                sent = sent.wrapping_add(1);
                // A long sealed range is written back to back; look for a close now and then.
                if sent % 16 == 0 && !peer_alive(&mut ws, Duration::from_millis(1)) {
                    break;
                }
            }
            Ok(Step::Wait) => {
                if !peer_alive(&mut ws, tick) {
                    break;
                }
            }
            Ok(Step::End(v)) => {
                let _ = send(&mut ws, &v);
                return close(&mut ws, 1000, "range complete");
            }
            Err(e) => return refuse(ws, &e),
        }
    }
    let _ = ws.get_mut().shutdown(Shutdown::Both);
}
