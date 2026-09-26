//! `GET /ws/spectrum/rows` — **one pane's** spectrum rows, folded onto the pane's own grid,
//! quantised, pushed as binary blocks over a time range that starts in the store and runs into the
//! live edge (LSR-2, T-1043).
//!
//! # Why a second row route, beside `/ws/tiles/rows`
//!
//! [`crate::rows`] pushes rows at **tile-lattice addresses**: a column of `(level_f, f_index,
//! cells)`, one JSON message per block, `max_db` as a JSON array. That is the right shape for the
//! tile cache — a client files every row under the tile key it already holds — and it is the wrong
//! shape for the **live edge of a pane**, for three measured reasons:
//!
//! 1. **A pane is not a tile column.** A following pane 1600 px wide over an arbitrary window
//!    covers some number of lattice columns that is neither 1 nor constant, so painting its live
//!    edge from tile columns means one subscription per column (the shipped client opens up to 12,
//!    `ui/src/surface/rowfeed.ts`), each carrying cells outside the pane and none carrying the
//!    pane's own pixel grid. Here the *pane* is the subscription: `f_lo_hz`, `f_hi_hz`, `nf`, and
//!    the fold onto those `nf` columns happens once, on the server, where the cells already are.
//! 2. **JSON numbers are most of the bytes.** A row of 1600 cells is ~12 kB of JSON text and
//!    3.2 kB of binary16; at 25 rows/s that is 300 kB/s against 80 kB/s, and the client pays a
//!    `JSON.parse` per block on the frame thread. The quantisation is the tile route's own
//!    (`?planes=f16`, T-533): little-endian IEEE binary16, **NaN = not measured**, which is exactly
//!    what the R16F texture the renderer uploads to keeps. Nothing is rounded that the GPU would
//!    not have rounded anyway.
//! 3. **The live ring needs a coverage plane and an epoch per block, not per tile.** LSR-1's ring
//!    texture is per pane; the grey it draws and the "the fog moved" signal it re-lays on both
//!    arrive here, on the block's own axes, in the same message as the values.
//!
//! What is **not** different is the cursor's contract, and that is deliberate: this route is the
//! same walk over the same store with the same three rules, which is why it can serve a pane's
//! history and its live edge with one mechanism (T-420's drift is what two mechanisms cost).
//!
//! - **The subscription is a time range, never "now".** `t_from` is required (absolute capture
//!   time, Unix ns, snapped down to a row boundary) and nothing defaults it; `t_to` is optional and
//!   an open range runs into the live edge. A pane scrubbed into the past and a pane following live
//!   are the same request with a different range, so nothing here knows which it is serving.
//! - **A row is pushed once, when it is complete**, and never before the tune record has reached it
//!   ([`crate::rows::record_reach`]): a row goes out once, so its coverage must be final when it
//!   does.
//! - **A stretch the coverage map calls uniformly unobserved is answered from the map alone**
//!   (T-461), as one payload-less block over the whole stretch, with the probe span doubling while
//!   the grey continues — so a pane opened over a band nothing ever tuned costs a few dozen small
//!   messages rather than millions of empty rows.
//!
//! # The epoch
//!
//! Every block carries a `u32` **epoch**: it increments when the set of *tuning configurations*
//! over this pane's frequency window changes ([`crate::coverage::TileOverlay::config_fingerprint`]
//! states the rule). A retune under the pane changes it; a dwell that goes on covering the same
//! band does not. A client re-lays its coverage fog and re-reads the tiles under the pane when the
//! epoch it sees changes, which is the signal T-1042 needed and did not want to invent twice.
//!
//! # The wire
//!
//! `docs/stream-contract.md` §17 is the record; `docs/api.md` is the route. One **text** header
//! first (the subscription, stated back), then one **binary** message per block — a 48-byte
//! little-endian header, the binary16 values, and the coverage trailer — and a text `end` only when
//! a closed range completes. Refusals complete the upgrade and close `4000 + status`, the
//! `/ws/open/{name}` convention every other WebSocket route here follows.

use std::io::{self, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use hk_model::{FreqRange, TimeRange, Timestamp};
use hk_store::Overview;
use serde_json::{Value, json};
use tungstenite::Message;
use tungstenite::protocol::frame::CloseFrame;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::{Role, WebSocket};

use crate::http::ApiState;
use crate::query::{ApiError, Params, Region, bad, param};
use crate::rows::{GAP_PROBE_ROWS, edges, read_rows, record_reach};
use crate::tiles::{
    TileKey, TileLattice, TileStore, affordable_levels, axis_fold, f16_bits, read_order_until,
    store_name, tier_of, tile_store, with_tile_history,
};

/// Pane columns a subscription may ask for. The floor is the tile route's own (`MIN_TILE_CELLS`);
/// the ceiling is a pane wider than any display this device drives, and is what bounds a block.
pub const MIN_NF: usize = 8;
/// See [`MIN_NF`].
pub const MAX_NF: usize = 4096;

/// Cells in one block, at most — the coverage rasteriser's own grid bound
/// (`hk_store::coverage::MAX_COVERAGE_GRID_CELLS`), so a block's plane is **always** laid
/// cell-for-cell on the block's own axes and a client never reads one grid through another's
/// addressing.
pub const MAX_BLOCK_CELLS: usize = 65_536;

/// Rows in one block, at most, whatever `nf` is: the live edge must not wait for a 64-row burst,
/// and a block is a patch of a ring texture rather than a tile.
pub const MAX_BLOCK_ROWS: usize = 64;

/// Pane subscriptions open at once, per server. Past it the handshake is refused `503`.
///
/// The same bound as [`crate::rows::MAX_ROW_FEEDS`] and for the same reason, counted separately
/// because a pane feed and a tile-column feed are different subscriptions with different costs:
/// a browser holds **one of these per pane**, where the tile route's client held up to 12 per pane.
pub const MAX_PANE_FEEDS: usize = 16;

/// The block header's bytes. Fixed, little-endian, and documented field by field in
/// `docs/stream-contract.md` §17 — never inferred from a payload length.
pub const BLOCK_HEADER_BYTES: usize = 48;

/// `kind`: measured rows follow.
pub const KIND_ROWS: u8 = 1;
/// `kind`: a stretch the coverage map calls uniformly unobserved. No payload, no trailer, no level.
pub const KIND_UNOBSERVED: u8 = 2;

/// `flags` bit 0: no late frame can still land in this block (the watermark has passed its end).
pub const FLAG_FINAL: u8 = 1 << 0;
/// `flags` bit 1: this block does not continue the previous one sent on this subscription.
pub const FLAG_DISCONTINUITY: u8 = 1 << 1;

/// `values`: little-endian IEEE binary16, NaN = not measured (never a zero, never a floor).
pub const VALUES_F16_LE: u8 = 1;

/// `level` / `tier` when the block carries no measurement at all.
pub const NO_LEVEL: u8 = 0xff;

/// The coverage trailer's encoding: `runs` runs of `(cells, state)` over the block's own cells,
/// row-major, earliest row first, low frequency first.
pub const TRAILER_RUN8: u8 = 1;

/// The coverage alphabet, by code — the same four states and the same order the JSON routes serve
/// (`hk_api::coverage::COVERAGE_STATES`), because a code read against another alphabet is exactly
/// the honesty failure the four states exist to prevent.
pub const COVERAGE_STATES: [&str; 4] = ["unobserved", "observed", "unknown", "excluded"];

/// Bounds on how often a waiting subscription looks at the data edge (the tile route's, unchanged:
/// one short lock hold to read two timestamps).
const MIN_TICK: Duration = Duration::from_millis(5);
const MAX_TICK: Duration = Duration::from_millis(250);

/// Query parameters this route accepts.
const ALLOWED: [&str; 9] = [
    "device", "scheme", "f_lo_hz", "f_hi_hz", "nf", "level_t", "t_from", "t_to", "token",
];

/// One pane's subscription: its frequency window, its columns, its row period and a time range.
#[derive(Clone, Debug)]
pub struct PaneSubscription {
    /// Which store answers (the same rule `/api/tiles` uses).
    pub store: TileStore,
    /// Whose coverage decides this pane's grey; `"any"` is the union.
    pub device: String,
    /// The lattice the row period was named on, kept so the subscription can state it back.
    pub lattice: TileLattice,
    /// The pane's frequency window.
    pub freq: FreqRange,
    /// The pane's columns. The fold onto them happens on the server.
    pub nf: usize,
    /// The lattice time level the row period came from.
    pub level_t: usize,
    /// The row period, ns.
    pub t_cell_ns: i64,
    /// First row, inclusive, on that level's axis from the Unix epoch.
    pub from: i64,
    /// Row after the last, or `None` for a range that runs on until the subscriber leaves.
    pub to: Option<i64>,
    /// Rows in one block: [`MAX_BLOCK_ROWS`], reduced so a block's cells fit
    /// [`MAX_BLOCK_CELLS`].
    pub rows_per_block: usize,
    /// Affordable store levels for a block of this shape, finest first.
    pub candidates: Vec<u8>,
}

fn float(q: &Params, k: &'static str) -> Result<f64, ApiError> {
    let raw = param(q, k).ok_or_else(|| bad(&format!("{k} is required")))?;
    raw.parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .ok_or_else(|| bad(&format!("{k} must be a frequency in Hz")))
}

fn nanos(q: &Params, k: &'static str) -> Result<Option<i64>, ApiError> {
    match param(q, k) {
        None => Ok(None),
        Some(raw) => raw
            .parse::<i64>()
            .ok()
            .filter(|v| *v >= 0)
            .map(Some)
            .ok_or_else(|| {
                bad(&format!(
                    "{k} must be absolute capture time in Unix nanoseconds, an integer >= 0"
                ))
            }),
    }
}

/// Parses a subscription. **`t_from` is required and nothing defaults it** — see the module docs.
pub fn parse_subscription(state: &ApiState, q: &Params) -> Result<PaneSubscription, ApiError> {
    if let Some((k, _)) = q.iter().find(|(k, _)| !ALLOWED.contains(&k.as_str())) {
        return Err(bad(&format!(
            "unknown parameter {k:?} (allowed: device, scheme, f_lo_hz, f_hi_hz, nf, level_t, \
             t_from, t_to). A pane is a WINDOW, not a lattice address: there is no level_f, \
             f_index, t_index or cells here — `nf` is the pane's own columns and the fold onto \
             them is this route's job."
        )));
    }
    let (lo, hi) = (float(q, "f_lo_hz")?, float(q, "f_hi_hz")?);
    if !(lo >= 0.0 && hi > lo) {
        return Err(bad(
            "f_lo_hz and f_hi_hz are the pane's frequency window: 0 <= f_lo_hz < f_hi_hz",
        ));
    }
    let nf = param(q, "nf")
        .ok_or_else(|| bad("nf is required: the pane's columns, which this route folds onto"))?
        .parse::<usize>()
        .ok()
        .filter(|n| (MIN_NF..=MAX_NF).contains(n))
        .ok_or_else(|| bad(&format!("nf must be in {MIN_NF}..={MAX_NF}")))?;
    let level_t = match param(q, "level_t") {
        None => 0usize,
        Some(raw) => raw
            .parse::<usize>()
            .ok()
            .ok_or_else(|| bad("level_t must be an integer >= 0"))?,
    };
    let device = param(q, "device").unwrap_or("any").to_owned();
    if device.is_empty() || device.len() > 128 {
        return Err(bad("device must be `any` or a device id"));
    }
    let t_from = nanos(q, "t_from")?.ok_or_else(|| {
        bad(
            "t_from is required: a pane subscription is a TIME RANGE [t_from, t_to) in absolute \
             capture time (Unix ns), never \"the live stream\" — there is no implicit now. To \
             follow the growing edge, start at the row you have and leave t_to open.",
        )
    })?;
    let t_to = nanos(q, "t_to")?;
    if t_to.is_some_and(|t| t <= t_from) {
        return Err(bad("t_to must be greater than t_from"));
    }
    let store = tile_store(state, q);
    let rows_per_block = (MAX_BLOCK_CELLS / nf).clamp(1, MAX_BLOCK_ROWS);
    let freq = FreqRange::new(lo, hi);
    let (lattice, t_cell_ns) = with_tile_history(state, store, |p| {
        let lattice = match param(q, "scheme") {
            None | Some("view") => TileLattice::view(p.geometry()),
            Some("overview") => TileLattice::overview(p.geometry()),
            Some(other) => {
                let n: u16 = other
                    .parse()
                    .map_err(|_| bad("scheme must be `view`, `overview` or a store scheme id"))?;
                TileLattice::store(p.geometry(), n)
            }
        };
        let t_cell_ns = *lattice.t_cells_ns.get(level_t).ok_or_else(|| {
            ApiError::new(
                404,
                format!(
                    "scheme {:?} has no time level {level_t}: its axis is level_t 0..{}",
                    lattice.name,
                    lattice.t_cells_ns.len()
                ),
            )
        })?;
        Ok((lattice, t_cell_ns))
    })?;
    let from = t_from.div_euclid(t_cell_ns);
    let to = t_to.map(|t| t.div_euclid(t_cell_ns) + i64::from(t.rem_euclid(t_cell_ns) != 0));
    if to.is_some_and(|t| i128::from(t) * i128::from(t_cell_ns) > i128::from(i64::MAX) / 2) {
        return Err(bad("t_to is outside the addressable time range"));
    }
    let mut sub = PaneSubscription {
        store,
        device,
        lattice,
        freq,
        nf,
        level_t,
        t_cell_ns,
        from,
        to,
        rows_per_block,
        candidates: Vec::new(),
    };
    sub.candidates = with_tile_history(state, store, |p| {
        let key = sub.block_key(sub.from, sub.rows_per_block as i64);
        Ok(affordable_levels(p, &key)
            .into_iter()
            .map(|l| l as u8)
            .collect::<Vec<u8>>())
    })?;
    if sub.candidates.is_empty() {
        return Err(bad(
            "no store level can back a pane this wide at this row period inside the work budget — \
             ask for a coarser level_t, fewer columns or a narrower window; `axes` on /api/tiles \
             states how far the lattice can be read",
        ));
    }
    Ok(sub)
}

impl PaneSubscription {
    fn row_ns(&self, row: i64) -> i64 {
        row.saturating_mul(self.t_cell_ns)
    }

    fn window(&self, a: i64, b: i64) -> TimeRange {
        TimeRange::new(
            Timestamp::from_unix_nanos(self.row_ns(a)),
            Timestamp::from_unix_nanos(self.row_ns(b)),
        )
    }

    /// The pane's frequency cell: its window folded onto its columns.
    fn f_cell_hz(&self) -> f64 {
        self.freq.width_hz() / self.nf as f64
    }

    /// The synthetic key one block of `rows` rows is read through.
    ///
    /// A pane is not a lattice address, but every question the read asks — which levels are
    /// affordable, how the read is chunked, which level answers, what tier that is, how the fold
    /// reads on each axis — is a question about a **region and a grid**, and those the pane has.
    /// So the block borrows the tile route's own key type and every one of its rules applies
    /// unchanged; `store_node` is `None` because a pane's frequency cell is its window over its
    /// pixels and is not a node of anything (the honest answer, and it only costs the exact-node
    /// shortcut in [`read_order_until`]).
    fn block_key(&self, a: i64, rows: i64) -> TileKey {
        TileKey {
            device: self.device.clone(),
            lattice: self.lattice.clone(),
            level_f: 0,
            level_t: self.level_t,
            f_index: 0,
            t_index: 0,
            // The block's rows: what the chunking and the fold budget are charged for.
            cells: rows.max(1) as usize,
            f_cell_hz: self.f_cell_hz(),
            t_cell_ns: self.t_cell_ns,
            region: Region {
                freq: self.freq,
                t0_ns: self.row_ns(a),
                t1_ns: self.row_ns(a + rows.max(1)),
            },
            store_node: None,
        }
    }

    /// The first message: what was subscribed, stated back, and how to read the binary blocks that
    /// follow — so a client never has to remember what it asked for, or infer a layout.
    pub fn subscribed_json(&self, state: &ApiState, epoch: u32) -> Value {
        let (edge, watermark) = edges(state, self.store).unwrap_or((None, None));
        json!({
            "type": "subscribed",
            "pane": {
                "f_lo_hz": self.freq.lo_hz,
                "f_hi_hz": self.freq.hi_hz,
                "nf": self.nf,
                "f_cell_hz": self.f_cell_hz(),
                "device": self.device,
                "scheme": self.lattice.name,
                "level_t": self.level_t,
                "t_cell_s": self.t_cell_ns as f64 / 1e9,
            },
            "range": {
                "t_from": self.row_ns(self.from),
                "t_to": self.to.map(|t| self.row_ns(t)),
                "t0_s": self.row_ns(self.from) as f64 / 1e9,
                "t1_s": self.to.map(|t| self.row_ns(t) as f64 / 1e9),
                "row0": self.from,
                "open": self.to.is_none(),
            },
            "record": {
                "contract": "docs/stream-contract.md#17",
                "framing": "one WebSocket binary message per block",
                "byte_order": "little-endian",
                "header_bytes": BLOCK_HEADER_BYTES,
                "kinds": { "rows": KIND_ROWS, "unobserved": KIND_UNOBSERVED },
                "values": {
                    "code": VALUES_F16_LE,
                    "type": "f16",
                    "cells": "rows x nf, row-major, earliest row first, low frequency first",
                    // The same claim `null` makes in the JSON spelling, in the only encoding
                    // binary16 has for it. Never a zero and never a floor.
                    "absent": "nan",
                },
                "coverage": {
                    "encoding": TRAILER_RUN8,
                    "name": "run8",
                    "states": COVERAGE_STATES,
                    "grid": "the block's own cells, the same order as the values",
                },
            },
            "epoch": epoch,
            "epoch_rule": "the tuning configurations the coverage record holds over this pane's \
                window: it increments on a retune under the pane, NOT when a dwell goes on covering \
                the same band. A change means re-lay the fog and re-read what is under the pane.",
            "store": store_name(self.store),
            "candidates": self.candidates,
            "rows_per_block": self.rows_per_block,
            "data_edge_s": edge.map(|e| e as f64 / 1e9),
            "watermark_s": watermark.map(|w| w as f64 / 1e9),
            "rule": "the pane is the subscription: its window and its nf columns, folded here. A \
                row is pushed ONCE, when the data edge has passed its end and the tune record has \
                reached it; FINAL says no late frame can still land in it. The range is the \
                subscription — rows already recorded arrive at once, rows not yet recorded arrive \
                as they are — and it ends at t_to (`end`) or never. Grey is each block's own \
                coverage trailer, in the same four states /api/tiles serves.",
        })
    }
}

/// What one step of a cursor produced.
#[derive(Debug)]
pub enum Step {
    /// A binary block to send; more may be ready at once.
    Send(Vec<u8>),
    /// Nothing complete yet: the range runs past the data edge.
    Wait,
    /// The range is exhausted; send this text message and close.
    End(Value),
}

/// A reader walking forward through one pane's range. **Nothing in it knows or cares whether the
/// rows it is reading are "live"** — the same `step` serves a pane scrubbed into last week and a
/// pane following the growing edge.
#[derive(Debug)]
pub struct PaneCursor {
    sub: PaneSubscription,
    next: i64,
    /// Rows the next coverage probe spans ([`GAP_PROBE_ROWS`], doubling across grey).
    gap_span: i64,
    /// How far forward the tune record is known to reach, ns ([`record_reach`]).
    reach_ns: Option<i64>,
    /// The epoch in force, and the configuration fingerprint it was computed from.
    epoch: u32,
    fingerprint: Option<u64>,
    /// The row after the last one sent, so a block that does not continue it is marked
    /// `DISCONTINUITY` rather than leaving the client to compare addresses.
    sent_through: Option<i64>,
}

impl PaneCursor {
    /// A cursor at the start of `sub`'s range.
    pub fn new(sub: PaneSubscription) -> Self {
        let next = sub.from;
        Self {
            sub,
            next,
            gap_span: GAP_PROBE_ROWS,
            reach_ns: None,
            epoch: 0,
            fingerprint: None,
            sent_through: None,
        }
    }

    /// The subscription.
    pub fn subscription(&self) -> &PaneSubscription {
        &self.sub
    }

    /// The next row this cursor will deliver — **this pane's own edge**, which is why two panes on
    /// one client have two of them.
    pub fn next_row(&self) -> i64 {
        self.next
    }

    /// The epoch in force.
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    /// The epoch after folding in `fp`: it increments when the configurations change, so a client
    /// reads a *change*, never a value it has to interpret.
    fn fold_epoch(&mut self, fp: u64) -> u32 {
        match self.fingerprint {
            Some(prev) if prev == fp => {}
            Some(_) => self.epoch = self.epoch.wrapping_add(1),
            None => self.fingerprint = Some(fp),
        }
        self.fingerprint = Some(fp);
        self.epoch
    }

    /// Advances by at most one block.
    pub fn step(&mut self, state: &ApiState) -> Result<Step, ApiError> {
        let s = &self.sub;
        if let Some(to) = s.to
            && self.next >= to
        {
            return Ok(Step::End(json!({
                "type": "end",
                "row": to,
                "t_ns": s.row_ns(to),
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
        let complete = edge.div_euclid(s.t_cell_ns);
        let limit = s.to.map_or(complete, |t| t.min(complete));
        if self.next >= limit {
            return Ok(Step::Wait);
        }
        // ...and no row the tune record has not reached yet.
        let want = limit.saturating_mul(s.t_cell_ns);
        let Some(reach) = record_reach(state, s.row_ns(self.next), want, &mut self.reach_ns) else {
            return Ok(Step::Wait);
        };
        let limit = limit.min(reach.div_euclid(s.t_cell_ns));
        if self.next >= limit {
            return Ok(Step::Wait);
        }
        // T-461 over a range: a stretch the coverage map calls unobserved for the selected device
        // is answered from the map alone, without reading the store.
        let probe_end = limit.min(self.next.saturating_add(self.gap_span));
        let probe_rows = (probe_end - self.next) as usize;
        let probe = crate::coverage::TileOverlay::collect_once(
            state,
            s.freq,
            s.window(self.next, probe_end),
            probe_rows.min(s.rows_per_block),
            s.nf,
        );
        if probe.uniform_state(&s.device) == Some("unobserved") {
            let rows = probe_end - self.next;
            let final_ = watermark.is_some_and(|w| s.row_ns(probe_end) <= w);
            let epoch = self.fold_epoch(probe.config_fingerprint());
            let block = self.header(
                KIND_UNOBSERVED,
                self.next,
                rows,
                epoch,
                final_,
                NO_LEVEL,
                NO_LEVEL,
                0,
                0,
                0,
            );
            self.next = probe_end;
            self.sent_through = Some(probe_end);
            self.gap_span = self.gap_span.saturating_mul(2);
            return Ok(Step::Send(block));
        }
        self.gap_span = GAP_PROBE_ROWS;
        let block_end = limit.min(self.next + self.sub.rows_per_block as i64);
        let bytes = self.block(state, self.next, block_end, watermark)?;
        self.next = block_end;
        self.sent_through = Some(block_end);
        Ok(Step::Send(bytes))
    }

    /// One block's header: the fixed 48 little-endian bytes of `docs/stream-contract.md` §17.
    #[allow(clippy::too_many_arguments)]
    fn header(
        &self,
        kind: u8,
        row0: i64,
        rows: i64,
        epoch: u32,
        final_: bool,
        level: u8,
        tier: u8,
        fold: u8,
        trailer_bytes: u32,
        observed_cells: u32,
    ) -> Vec<u8> {
        let s = &self.sub;
        let mut flags = 0u8;
        if final_ {
            flags |= FLAG_FINAL;
        }
        if self.sent_through != Some(row0) {
            flags |= FLAG_DISCONTINUITY;
        }
        let nf = if kind == KIND_ROWS { s.nf as u16 } else { 0 };
        let mut b = Vec::with_capacity(BLOCK_HEADER_BYTES);
        b.push(kind);
        b.push(flags);
        b.push(if kind == KIND_ROWS { VALUES_F16_LE } else { 0 });
        b.push(level);
        b.push(tier);
        b.push(fold);
        b.extend_from_slice(&nf.to_le_bytes());
        b.extend_from_slice(&u32::try_from(rows).unwrap_or(u32::MAX).to_le_bytes());
        b.extend_from_slice(&epoch.to_le_bytes());
        b.extend_from_slice(&s.row_ns(row0).to_le_bytes());
        b.extend_from_slice(&s.t_cell_ns.to_le_bytes());
        b.extend_from_slice(&row0.to_le_bytes());
        b.extend_from_slice(&trailer_bytes.to_le_bytes());
        b.extend_from_slice(&observed_cells.to_le_bytes());
        debug_assert_eq!(b.len(), BLOCK_HEADER_BYTES);
        b
    }

    /// One block of rows `[a, b)`: header, binary16 values, coverage trailer.
    fn block(
        &mut self,
        state: &ApiState,
        a: i64,
        b: i64,
        watermark: Option<i64>,
    ) -> Result<Vec<u8>, ApiError> {
        // Read through `self.sub` field by field rather than borrowing it: the epoch fold below
        // needs `&mut self`, and a block on the live path must not pay a clone of the
        // subscription to get one.
        let (nf, store) = (self.sub.nf, self.sub.store);
        let nrows = (b - a) as usize;
        let key = self.sub.block_key(a, (b - a).max(1));
        let window = self.sub.window(a, b);
        let overlay =
            crate::coverage::TileOverlay::collect_once(state, self.sub.freq, window, nrows, nf);
        // The tile read's rule, over the pane's grid: the cheapest level that only folds, walking
        // on only when a level holds nothing, and say which answered.
        let end_ns = self.sub.row_ns(b);
        let order: Vec<u8> = with_tile_history(state, store, |p| {
            Ok(read_order_until(p, &key, end_ns)
                .into_iter()
                .map(|l| l as u8)
                .collect())
        })?;
        let mut first: Option<(u8, Overview)> = None;
        let mut answered: Option<(u8, Overview)> = None;
        for &level in &order {
            let o = read_rows(state, store, &key, nf, level, window, nrows)?;
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
            .ok_or_else(|| ApiError::new(400, "no store level can back this pane"))?;
        let (lf, lt) = with_tile_history(state, store, |p| {
            let g = &p.geometry().levels[usize::from(level)];
            Ok((g.f_cell_hz, g.t_cell_ns))
        })?;
        // The honesty tier of THIS block, by the tile route's own rule over the level that
        // answered it (T-902): a claim the client states per block and never borrows.
        let tier = tier_of(&key, lf, lt, crate::http::max_live_span_hz(state));
        let fold = fold_code(
            &axis_fold(lf, key.f_cell_hz, o.src_nf, nf),
            &axis_fold(lt as f64, key.t_cell_ns as f64, o.src_nt, nrows),
        );
        let (codes, present, aligned) = overlay.selected_plane_codes(&self.sub.device);
        let trailer = trailer(&codes, present, aligned);
        let final_ = watermark.is_some_and(|w| end_ns <= w);
        let epoch = self.fold_epoch(overlay.config_fingerprint());
        let mut out = self.header(
            KIND_ROWS,
            a,
            b - a,
            epoch,
            final_,
            level,
            tier_code(tier),
            fold,
            u32::try_from(trailer.len()).unwrap_or(u32::MAX),
            u32::try_from(o.observed_cells).unwrap_or(u32::MAX),
        );
        out.reserve(nrows * nf * 2 + trailer.len());
        for c in &o.cells {
            let v: f32 = if c.sources == 0 { f32::NAN } else { c.max_db };
            out.extend_from_slice(&f16_bits(v).to_le_bytes());
        }
        out.extend_from_slice(&trailer);
        Ok(out)
    }
}

/// `DetailSource` as the block's one byte. Named codes, not an index into a list the client has to
/// hold in the right order.
fn tier_code(tier: crate::navigation::DetailSource) -> u8 {
    match tier {
        crate::navigation::DetailSource::LiveIq => 0,
        crate::navigation::DetailSource::SpectrumHistory => 1,
        crate::navigation::DetailSource::SurveyOverview => 2,
    }
}

/// The two axes' fold directions in one byte: bits 0–1 frequency, bits 2–3 time, `0` exact,
/// `1` folded, `2` replicated — the same three words [`axis_fold`] states in JSON.
fn fold_code(f: &Value, t: &Value) -> u8 {
    let code = |v: &Value| match v["direction"].as_str() {
        Some("exact") => 0u8,
        Some("folded") => 1,
        _ => 2,
    };
    code(f) | (code(t) << 2)
}

/// The coverage trailer: the selected plane, run-length encoded over the block's own cells.
fn trailer(codes: &[u8], present: bool, aligned: bool) -> Vec<u8> {
    let mut runs: Vec<(u32, u8)> = Vec::new();
    for &c in codes {
        match runs.last_mut() {
            Some((n, s)) if *s == c => *n += 1,
            _ => runs.push((1, c)),
        }
    }
    let mut out = Vec::with_capacity(8 + runs.len() * 8);
    out.push(TRAILER_RUN8);
    out.push(COVERAGE_STATES.len() as u8);
    out.push(u8::from(present) | (u8::from(aligned) << 1));
    out.push(0);
    out.extend_from_slice(&u32::try_from(runs.len()).unwrap_or(u32::MAX).to_le_bytes());
    for (n, state) in runs {
        out.extend_from_slice(&n.to_le_bytes());
        out.push(state);
        out.extend_from_slice(&[0u8; 3]);
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The WebSocket front end. The same convention as `/ws/tiles/rows`: refusals complete the upgrade.
// ---------------------------------------------------------------------------------------------

/// Counts open subscriptions against [`MAX_PANE_FEEDS`]; released on drop.
struct FeedSlot<'a>(&'a AtomicUsize);

impl<'a> FeedSlot<'a> {
    fn take(n: &'a AtomicUsize) -> Option<Self> {
        n.fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
            (v < MAX_PANE_FEEDS).then_some(v + 1)
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

/// Completes the upgrade, sends `{"type":"refused",…}` and closes with `4000 + status` — a browser
/// cannot read an HTTP error body on a failed upgrade.
fn refuse(mut ws: WebSocket<TcpStream>, e: &ApiError) {
    let _ = ws.send(Message::Text(
        json!({ "type": "refused", "status": e.status, "reason": e.message })
            .to_string()
            .into(),
    ));
    close(&mut ws, 4000 + e.status, &e.message);
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

/// Serves one `/ws/spectrum/rows` request (token already verified).
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
    let Some(_slot) = FeedSlot::take(&state.pane_feeds) else {
        return refuse(
            ws,
            &ApiError::new(
                503,
                format!("{MAX_PANE_FEEDS} pane subscriptions are already open on this server"),
            ),
        );
    };
    let sub = match parse_subscription(state, query) {
        Ok(s) => s,
        Err(e) => return refuse(ws, &e),
    };
    let tick = Duration::from_nanos(u64::try_from(sub.t_cell_ns / 4).unwrap_or(u64::MAX))
        .clamp(MIN_TICK, MAX_TICK);
    let mut cursor = PaneCursor::new(sub);
    let header = cursor.subscription().subscribed_json(state, cursor.epoch());
    if ws.send(Message::Text(header.to_string().into())).is_err() {
        return;
    }
    let mut sent = 0u32;
    loop {
        match cursor.step(state) {
            Ok(Step::Send(b)) => {
                if ws.send(Message::Binary(b.into())).is_err() {
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
                let _ = ws.send(Message::Text(v.to_string().into()));
                return close(&mut ws, 1000, "range complete");
            }
            Err(e) => return refuse(ws, &e),
        }
    }
    let _ = ws.get_mut().shutdown(Shutdown::Both);
}
