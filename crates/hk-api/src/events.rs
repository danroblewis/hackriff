//! The durable catalogue of events (T-264, ADR-0017 stage TM-8): `GET /api/events`.
//!
//! **What this is for.** The inventory answers *"what has ever been seen here"*; Explore presents
//! *"what is here now"* and is window-scoped for it (TM-3/T-260). The all-time record did not stop
//! being valuable when it left Explore — it moved here, to the surface workflow #3 asks for:
//! *choose a region and see what activity was seen there over time* (CLAUDE.md invariants,
//! ADR-0017 §3).
//!
//! **An event is a presence interval**, and the unit is the interval rather than the emitter, so a
//! one-off burst is a row with its own timespan rather than a blip filtered out of a live list
//! (invariant 1: ephemeral emissions are first-class). Each event carries the emitter it belongs
//! to; the emitters themselves are listed once each in `emitters`, with their ranked explanations —
//! suggestions beside the measurement, never truth (the exploration-first rule).
//!
//! **Nothing here can be erased by decay** (ADR-0017 conflict (b)). Every row is read from
//! `emitter_observation`, the append-only observation ledger, through
//! [`Repository::presence_intervals`] — the same derivation TM-5 built. What decays (T-251/TM-6) is
//! a *candidate's confidence*, a ranking over hypotheses; it writes nothing and deletes nothing,
//! and this route never reads it. A signal that stopped hours ago left Explore because it is not in
//! the window, and its events are still here.
//!
//! **Not observed is never quiet.** A region with no observation coverage must read as *no data for
//! this period*, not as *nothing was on air* — the distinction T-263 established for the scrubber
//! and C26 states generally. `coverage` answers it for the queried box from the spectrum-history
//! grid, and its backend-rendered `statement` is what a thin client shows beside an empty
//! catalogue; with no history store the answer is **unknown**, never "quiet".

use hk_model::{
    IdleGap, InventoryEntry, InventoryIdentity, PresenceInterval, Repository, TimeRange, Timestamp,
};
use hk_store::Pyramid;
use serde_json::{Value, json};

use crate::query::{
    ApiError, Params, Region, bad, count, explanations_json, parse_inventory_query, parse_region,
    region_coverage, ts_s,
};

/// Events served per page by default.
pub const DEFAULT_EVENTS_LIMIT: usize = 200;
/// Largest page `/api/events` serves (bounds the response).
pub const MAX_EVENTS_LIMIT: usize = 2_000;
/// Largest event offset accepted as a cursor.
pub const MAX_EVENTS_CURSOR: u64 = 1_000_000;
/// Emitters scanned for one `/api/events` answer. A cap on work, never a claim about what
/// happened: `emitters_truncated` says when more matched the box than were expanded.
pub const MAX_EVENT_EMITTERS: u32 = 500;

/// One presence interval as an event row.
fn event_json(entry: &InventoryEntry, i: &PresenceInterval, window: TimeRange) -> Value {
    let (lo, hi) = (
        i.time
            .start
            .as_unix_nanos()
            .max(window.start.as_unix_nanos()),
        i.time.end.as_unix_nanos().min(window.end.as_unix_nanos()),
    );
    // Silence a revoked end rejoined is inside the extent and is not air (T-413), so it comes off
    // the in-window figure exactly as it comes off `duration_s`.
    let revoked_in_window: i64 = i
        .revoked
        .iter()
        .map(|g| (g.end.as_unix_nanos().min(hi) - g.start.as_unix_nanos().max(lo)).max(0))
        .sum();
    json!({
        "emitter_id": entry.emitter.id.to_string(),
        "t_start_s": ts_s(i.time.start),
        "t_end_s": ts_s(i.time.end),
        // Backend-computed: a client never derives a timespan from two fields it was handed.
        "duration_s": i.duration_s(),
        "in_window_s": ((hi - lo).max(0) - revoked_in_window).max(0) as f64 / 1e9,
        "open": i.open,
        "revoked_s": i.revoked_s(),
        "count": i.count,
        "sources": i.sources,
        "f_center_hz": i.f_center_hz,
    })
}

/// The emitter an event belongs to: enough to explain the row, gated exactly as
/// `/api/inventory` gates it (the identity is served in clear only when the query returned it so).
fn emitter_json(
    repo: &Repository,
    entry: &InventoryEntry,
    events: u64,
    on_air_s: f64,
) -> Result<Value, ApiError> {
    let e = &entry.emitter;
    let (scheme, value, class, withheld) = match &entry.identity {
        InventoryIdentity::None => (None, None, None, false),
        InventoryIdentity::Clear { identity, class } => (
            Some(identity.scheme.as_string()),
            Some(identity.value.as_str()),
            Some(*class),
            false,
        ),
        InventoryIdentity::Withheld { scheme, class } => {
            (Some(scheme.as_string()), None, *class, true)
        }
    };
    let freq = e.freq();
    let mut row = json!({
        "id": e.id.to_string(),
        "state": entry.lifecycle,
        "f_center_hz": e.f_center_hz,
        "bandwidth_hz": e.bandwidth_hz,
        "f_lo_hz": freq.lo_hz,
        "f_hi_hz": freq.hi_hz,
        "known_status": e.known_status,
        "family": entry.family,
        // Ranked suggestions for what this was, never an assertion (vision step 4).
        "explanations": explanations_json(repo, e.id).map_err(|_| failed())?,
        "identity_scheme": scheme,
        "identity_class": class,
        "withheld": withheld,
        // This emitter's share of the catalogue over the requested window — not a lifetime total,
        // and never `count` (ADR-0017 §5).
        "events": events,
        "on_air_s": on_air_s,
        // The lifetime History total, which is exactly where a monotonic counter belongs.
        "count": e.count,
    });
    if let Some(v) = value {
        row["identity_value"] = json!(v);
    }
    Ok(row)
}

fn failed() -> ApiError {
    ApiError::new(500, "event query failed")
}

/// The coverage block: what the receiver actually observed over the queried box, and the sentence
/// a client renders beside an empty catalogue.
///
/// Three answers that must never collapse into one another:
///
/// - **unknown** — no spectrum history on this server, so an empty catalogue is evidence of
///   nothing at all;
/// - **no data for this period** — the box was observed by nothing, or holds unobserved stretches;
/// - **nothing was on air** — the box was observed throughout and the catalogue is genuinely empty.
fn coverage_json(history: Option<&Pyramid>, r: &Region) -> Value {
    let Some(p) = history else {
        return json!({
            "source": null,
            "observed_fraction": null,
            "cells": null,
            "observed_cells": null,
            "gaps": null,
            "gaps_truncated": null,
            "statement": "Coverage is unknown: this server keeps no spectrum history, so an empty \
                          catalogue here is not evidence of a quiet band.",
        });
    };
    let Ok(cov) = region_coverage(p, r) else {
        return json!({
            "source": "spectrum-history",
            "observed_fraction": null,
            "cells": null,
            "observed_cells": null,
            "gaps": null,
            "gaps_truncated": null,
            "statement": "Coverage could not be read for this region and period, so an empty \
                          catalogue here is not evidence of a quiet band.",
        });
    };
    let pct = (cov.observed_fraction * 100.0).round() as i64;
    let statement = if cov.observed_cells == 0 {
        "Nothing here was observed in this period: no data for this period, not a quiet band."
            .to_owned()
    } else if cov.gaps.is_empty() {
        format!(
            "This region was observed throughout this period ({pct} % cell coverage), so an empty \
             catalogue means nothing was on the air here."
        )
    } else {
        let n = cov.gaps.len();
        let s = if n == 1 { "" } else { "es" };
        format!(
            "{pct} % of this region was observed; {n} unobserved stretch{s} in this period are no \
             data, never a quiet band."
        )
    };
    json!({
        "source": "spectrum-history",
        "observed_fraction": cov.observed_fraction,
        "cells": cov.cells,
        "observed_cells": cov.observed_cells,
        "gaps": cov.gaps.iter().map(|g| json!({"t0_s": ts_s(g.start), "t1_s": ts_s(g.end)})).collect::<Vec<_>>(),
        "gaps_truncated": cov.gaps_truncated,
        "statement": statement,
    })
}

/// `/api/events`: every event recorded in a region over a time range, newest first.
///
/// `f_lo`, `f_hi`, `t0` and `t1` are required — History is a question about a box, and a catalogue
/// with no box is a question nobody asked. Every other `/api/inventory` filter (`state`, `status`,
/// `tag`, `scheme`, `family`, `relations`) is accepted with the same meaning and selects which
/// *emitters* are expanded; `limit` and `cursor` page the **events**.
///
/// The window's own `t1` is its live edge, exactly as it is for an `/api/inventory` row's
/// `presence` (TM-2): an interval reads `open` when it was still running *then*, so a past window
/// re-derives the truth of its own moment instead of being marked ended by the wall clock.
pub fn events_json(
    repo: &Repository,
    history: Option<&Pyramid>,
    q: &Params,
) -> Result<Value, ApiError> {
    let r = parse_region(q)?;
    let window = TimeRange::new(
        Timestamp::from_unix_nanos(r.t0_ns),
        Timestamp::from_unix_nanos(r.t1_ns),
    );
    let limit = count(q, "limit", DEFAULT_EVENTS_LIMIT, MAX_EVENTS_LIMIT)?;
    let offset = match crate::query::nonempty(q, "cursor") {
        None => 0usize,
        Some(c) => c
            .parse::<u64>()
            .ok()
            .filter(|&o| o <= MAX_EVENTS_CURSOR)
            .map(|o| o as usize)
            .ok_or_else(|| bad("invalid cursor"))?,
    };
    // The same filters as `/api/inventory`, over the same predicate — the surfaces must never
    // disagree about which rows a box holds. `f_lo`/`f_hi`/`t0`/`t1` were required above, so the
    // freq and time filters are always set here.
    let mut query = parse_inventory_query(q)?;
    query.limit = MAX_EVENT_EMITTERS;
    query.offset = 0;
    let page = repo.query_inventory(&query).map_err(|_| failed())?;
    let emitters_truncated = page.next_offset.is_some();

    let mut events: Vec<(i64, i64, Value)> = Vec::new();
    let mut emitters = Vec::new();
    // Rows the box selected that carry no presence interval at all (a legacy writer's row, matched
    // on its hull — see `inventory_where`). They contribute no event, because a hull is not a
    // timespan and inventing one would be a fabricated measurement. Disclosed rather than dropped
    // silently, so an empty catalogue is never quietly caused by them.
    let mut no_interval = 0u64;
    for entry in &page.entries {
        let intervals = repo
            .presence_intervals(entry.emitter.id, IdleGap::conservative(), window.end)
            .map_err(|_| failed())?;
        if intervals.is_empty() {
            no_interval += 1;
            continue;
        }
        let mut n = 0u64;
        let mut on_air_ns = 0i128;
        for i in intervals.iter().filter(|i| i.time.overlaps(&window)) {
            let lo = i
                .time
                .start
                .as_unix_nanos()
                .max(window.start.as_unix_nanos());
            let hi = i.time.end.as_unix_nanos().min(window.end.as_unix_nanos());
            on_air_ns += i128::from(hi - lo).max(0);
            n += 1;
            events.push((
                i.time.end.as_unix_nanos(),
                i.time.start.as_unix_nanos(),
                event_json(entry, i, window),
            ));
        }
        if n > 0 {
            let on_air_s = on_air_ns as f64 / 1e9;
            emitters.push(emitter_json(repo, entry, n, on_air_s)?);
        }
    }
    // Newest last-first, so the page a client opens on is the most recent activity in the box.
    events.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let total = events.len();
    let listed: Vec<Value> = events
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|(_, _, v)| v)
        .collect();
    let next = offset + listed.len();
    Ok(json!({
        "window": {
            "f_lo_hz": r.freq.lo_hz,
            "f_hi_hz": r.freq.hi_hz,
            "t0_s": ts_s(window.start),
            "t1_s": ts_s(window.end),
        },
        "events": listed,
        // Every emitter with at least one event in the window, listed once. Its `events` and
        // `on_air_s` describe the whole window, not this page.
        "emitters": emitters,
        "total": total,
        "limit": limit,
        "next_cursor": (next < total && next as u64 <= MAX_EVENTS_CURSOR).then(|| next.to_string()),
        "emitters_truncated": emitters_truncated,
        "emitters_no_interval": no_interval,
        "coverage": coverage_json(history, &r),
        "identity_access": "standard",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::ROUTES;

    #[test]
    fn the_route_is_declared() {
        assert!(
            ROUTES
                .iter()
                .any(|(m, p)| *m == "GET" && *p == "/api/events")
        );
    }

    #[test]
    fn coverage_keeps_unknown_no_data_and_quiet_apart() {
        let r = Region {
            freq: hk_model::FreqRange::new(100e6, 101e6),
            t0_ns: 0,
            t1_ns: 1_000_000_000,
        };
        let v = coverage_json(None, &r);
        assert!(v["observed_fraction"].is_null(), "{v}");
        let s = v["statement"].as_str().unwrap();
        assert!(s.contains("unknown"), "{s}");
        assert!(
            !s.contains("nothing was on the air"),
            "an unknown coverage must never read as a quiet band: {s}"
        );
    }
}
