//! `GET /api/lastknown` (T-1058): the fog-of-war's source, over a pane's whole frequency window,
//! as one array.
//!
//! Every column of the window gets the **last-known** value the store's ledger holds for it
//! (`hk_store::Pyramid::last_known_ledger`): the newest sample's max-hold and when it was taken,
//! kept as the rows arrived — no tile is read and nothing is searched, so the answer is exact at any
//! zoom and any distance in time, and a pane can paint fog for its whole window from one response
//! and re-lay it per frame through its own capture-time mapping. The tile route's `shadow` plane
//! reads the same ledger (`docs/adr/0020` §T-1058).
//!
//! It is the **last-known tier only**. Grey is still decided by the coverage map alone (T-368): a
//! column with a value here is drawn as fog only where `coverage` says the radio was not looking.

use hk_model::Timestamp;
use serde_json::{Value, json};

use crate::http::ApiState;
use crate::query::{ApiError, Params, bad, count, param, parse_freq_only};
use crate::tiles::{store_name, tile_store, with_tile_history};

/// Default columns.
pub const DEFAULT_COLS: usize = 256;
/// Most columns one answer may carry (a 4K pane, with room).
pub const MAX_COLS: usize = 8192;

fn opt_secs(q: &Params, key: &'static str) -> Result<Option<f64>, ApiError> {
    match param(q, key) {
        None => Ok(None),
        Some(v) => v
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .map(Some)
            .ok_or_else(|| bad(&format!("{key} must be a finite number of seconds"))),
    }
}

/// `GET /api/lastknown?f_lo&f_hi[&cols][&device][&before][&t_cell][&scheme]` — see `docs/api.md`.
pub fn lastknown_json(state: &ApiState, q: &Params) -> Result<Value, ApiError> {
    const ALLOWED: [&str; 8] = [
        "f_lo", "f_hi", "cols", "device", "before", "t_cell", "scheme", "token",
    ];
    if let Some((k, _)) = q.iter().find(|(k, _)| !ALLOWED.contains(&k.as_str())) {
        return Err(ApiError::new(
            400,
            format!(
                "unknown parameter {k:?} (allowed: f_lo, f_hi, cols, device, before, t_cell, \
                 scheme)"
            ),
        ));
    }
    let freq = parse_freq_only(q)?.ok_or_else(|| bad("f_lo and f_hi are required, in Hz"))?;
    let cols = count(q, "cols", DEFAULT_COLS, MAX_COLS)?;
    let device = param(q, "device").filter(|d| !d.is_empty() && *d != "any");
    let source = device.map(hk_store::history::source_key);
    let before = opt_secs(q, "before")?;
    let t_cell = opt_secs(q, "t_cell")?;
    if t_cell.is_some_and(|t| t <= 0.0) {
        return Err(bad("t_cell must be > 0 seconds"));
    }
    let store = tile_store(state, q);
    let (ans, stats) = with_tile_history(state, store, |p| {
        let t_cell_ns = t_cell.map_or(p.geometry().levels[0].t_cell_ns, |t| (t * 1e9) as i64);
        Ok((
            p.last_known_ledger(
                source,
                freq,
                cols,
                t_cell_ns,
                before.map(|b| Timestamp::from_unix_nanos((b * 1e9) as i64)),
            ),
            p.ledger_stats(),
        ))
    })?;
    let s_of = |ns: i64| ns as f64 / 1e9;
    let n = ans.columns.len();
    let mut state_code = Vec::with_capacity(n);
    let (mut last_db, mut last_t, mut first_t, mut epoch, mut src) = (
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
    );
    for c in &ans.columns {
        let (code, db, lt, ft, ep, sk) = match *c {
            hk_store::LedgerColumn::Never => (0u8, None, None, None, None, None),
            hk_store::LedgerColumn::Known(v) => (
                1,
                Some(v.max_db),
                Some(s_of(v.last_ns)),
                Some(s_of(v.first_ns)),
                Some(v.epoch),
                Some(format!("{:016x}", v.source)),
            ),
            hk_store::LedgerColumn::NothingBefore { first_ns } => {
                (2, None, None, Some(s_of(first_ns)), None, None)
            }
            hk_store::LedgerColumn::Later { newest_ns } => {
                (3, None, Some(s_of(newest_ns)), None, None, None)
            }
        };
        state_code.push(code);
        last_db.push(db.map_or(Value::Null, crate::tiles::num));
        last_t.push(lt);
        first_t.push(ft);
        epoch.push(ep);
        src.push(sk);
    }
    Ok(json!({
        "store": store_name(store),
        "device": device.unwrap_or("any"),
        "region": { "lo_hz": freq.lo_hz, "hi_hz": freq.hi_hz },
        "cols": n,
        "f_cell_hz": ans.f_cell_hz,
        "source_f_cell_hz": ans.source_f_cell_hz,
        "t_cell_s": s_of(ans.t_cell_ns),
        "before_s": before,
        "complete": ans.complete,
        "known": ans.known(),
        "state": state_code,
        "states": ["never", "known", "nothing_before", "later"],
        "last_db": last_db,
        "last_t_s": last_t,
        "first_t_s": first_t,
        "epoch": epoch,
        "source": src,
        "cells_read": ans.cells_read,
        "ledger": {
            "sources": stats.sources,
            "cells": stats.cells,
            "resident_bytes": stats.resident_bytes,
            "file_bytes": stats.file_bytes,
            "loaded": stats.loaded,
        },
        "rule": "the LAST-KNOWN tier (docs/adr/0020 §T-1058), read from the store's ledger — never \
            searched, never re-rendered. Per column: `known` = last_db[i] is the max-hold of the \
            store's cell of `t_cell_s` holding the column's newest sample, last seen at \
            last_t_s[i] (absolute capture time; the end of that sample's finest cell), first seen \
            at first_t_s[i], in tune epoch epoch[i] of front end source[i]; `never` = nothing \
            ever reached the column; `nothing_before` = its first sample (first_t_s[i]) is at or \
            after `before_s`; `later` = it was also seen at or after `before_s` (newest at \
            last_t_s[i]) and the ledger holds only the newest value, so the value AT `before_s` \
            is not here (the tile route searches for it). `never` and `nothing_before` are \
            proof only when `complete` (the ledger has seen every frame its store folded). \
            GREY IS NOT DECIDED HERE: draw this only where `/api/coverage` says unobserved.",
    }))
}
