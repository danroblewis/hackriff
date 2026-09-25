//! Band-plan priors over a viewport (T-812, MAP-12; docs/24 §3f/§7): `GET /api/priors`.
//!
//! **What this is.** A *suggester*: the band-plan allocations (C17, [`hk_context::band_table`])
//! that intersect a (frequency × time) box, **ranked as explanations** for the energy the receiver
//! actually measured there, each with a backend-rendered `reason`. It is the exploration-first rule
//! served as a route: the database is **never a source of truth** — it never pre-populates the
//! inventory, never sets a family, never overrides a measurement, and a measured emission sitting
//! off its allocation's channel raster is **flagged, not snapped** (the "150 kHz off raster" case).
//!
//! **Read-only and computed on demand.** Nothing here writes: the rows come from the bundled
//! allocation table, and the measured side is read from what the pipeline already stored — each
//! window emitter's ranked explanations (`hk_pipeline::family`, written as a classifier
//! annotation). This route performs no matching of its own beyond "does this emitter's centre lie
//! in this allocation" and "did the emitter's own ranked explanations cite this row", so it can
//! never disagree with the explanation beside an inventory row.
//!
//! **Why not a tile channel** (docs/24 §7): priors are mutable reference data, and their ranking
//! depends on what *this* window measured, so sealing them into an immutable tile would spend the
//! immutability the tile store was bought for — the same argument `/api/tiles/events` makes.
//!
//! **Gated identically to `/api/events`**: the same required box (`f_lo`, `f_hi`, `t0`, `t1`,
//! parsed by the same [`parse_region`]), the same bearer/`?token=` rule every `/api/*` GET gets, the
//! same error shape, never audited. The result is capped at [`MAX_PRIORS`], with `truncated`
//! saying so.

use std::sync::OnceLock;

use hk_context::band_table::{AllocationRow, BandTable, FederalStatus, Region as TableRegion};
use hk_model::{InventoryQuery, Repository, TimeRange, Timestamp};
use serde_json::{Value, json};

use crate::http::ApiState;
use crate::query::{ApiError, Params, explanations_json, parse_region, ts_s};

/// Largest number of allocations one answer lists (the whole bundled table is ~40 rows; this bounds
/// the response if a larger table is ever loaded).
pub const MAX_PRIORS: usize = 64;
/// Emitters read to rank the allocations. A cap on work, never a claim: `emitters_truncated`
/// says when more matched the box.
pub const MAX_PRIOR_EMITTERS: u32 = 500;
/// The name the bundled allocation table is cited by (the prefix of every `prior_ref`).
pub const PRIORS_SOURCE: &str = "us-47cfr2106-compact";

/// The sentence every answer carries, so no client can present a prior as a detection.
pub const PRIORS_STATEMENT: &str = "Band-plan priors are suggestions, never truth: an allocation \
     says what is supposed to be here, not what was measured. They never add to the inventory, \
     and an emission off its expected channel is flagged, not snapped.";

fn table() -> Option<&'static BandTable> {
    static TABLE: OnceLock<Option<BandTable>> = OnceLock::new();
    TABLE
        .get_or_init(|| BandTable::bundled(TableRegion::Us).ok())
        .as_ref()
}

fn federal_str(f: FederalStatus) -> &'static str {
    match f {
        FederalStatus::Federal => "federal",
        FederalStatus::NonFederal => "non-federal",
        FederalStatus::Shared => "shared",
    }
}

/// A measured emitter in the window, reduced to what ranking needs.
struct Measured {
    id: String,
    f_center_hz: f64,
    /// The emitter's own ranked explanations, as the pipeline stored them.
    explanations: Vec<Value>,
}

/// One off-raster emitter inside an allocation.
struct OffRaster {
    emitter_id: String,
    f_center_hz: f64,
    offset_hz: f64,
    nearest_channel_hz: f64,
    raster_hz: f64,
}

/// What the window measured inside one allocation.
#[derive(Default)]
struct Support {
    /// Emitters whose centre lies in the allocation.
    in_band: Vec<String>,
    /// Of those, the ones whose own ranked explanations cite this allocation row, with the best
    /// (lowest) rank they gave it.
    cited: Vec<(String, u64)>,
    off_raster: Vec<OffRaster>,
}

fn support_of(row: &AllocationRow, measured: &[Measured]) -> Support {
    let prior_ref = row.prior_ref();
    let mut s = Support::default();
    for m in measured {
        if !(m.f_center_hz >= row.freq.lo_hz && m.f_center_hz <= row.freq.hi_hz) {
            continue;
        }
        s.in_band.push(m.id.clone());
        let citing: Vec<&Value> = m
            .explanations
            .iter()
            .filter(|e| e["prior_ref"].as_str() == Some(prior_ref.as_str()))
            .collect();
        if let Some(best) = citing.iter().filter_map(|e| e["rank"].as_u64()).min() {
            s.cited.push((m.id.clone(), best));
        }
        // The raster fit the pipeline recorded for this allocation's service — never recomputed
        // here, so the flag and the inventory row's explanation are one fact.
        let raster = citing
            .iter()
            .flat_map(|e| e["evidence"].as_array().into_iter().flatten())
            .find(|ev| ev["kind"] == "raster");
        if let Some(r) = raster
            && r["on_raster"] == Value::Bool(false)
            && let (Some(off), Some(near), Some(step)) = (
                r["offset_hz"].as_f64(),
                r["nearest_channel_hz"].as_f64(),
                r["raster_hz"].as_f64(),
            )
        {
            s.off_raster.push(OffRaster {
                emitter_id: m.id.clone(),
                f_center_hz: m.f_center_hz,
                offset_hz: off,
                nearest_channel_hz: near,
                raster_hz: step,
            });
        }
    }
    s
}

fn mhz(hz: f64) -> String {
    format!("{:.3} MHz", hz / 1e6)
}

fn khz(hz: f64) -> String {
    format!("{:.1} kHz", hz / 1e3)
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The backend-rendered reason for one allocation: what it is, what the window measured in it, and
/// any off-raster flag — worded as a suggestion.
fn reason(row: &AllocationRow, s: &Support, measured_known: bool) -> String {
    let services = row.primary_services.join(", ");
    let mut out = format!(
        "{} is allocated {} {}–{} ({}, {PRIORS_SOURCE}).",
        row.id,
        if services.is_empty() {
            "to no primary service".to_owned()
        } else {
            format!("primary to {services}")
        },
        mhz(row.freq.lo_hz),
        mhz(row.freq.hi_hz),
        federal_str(row.federal),
    );
    if !measured_known {
        out.push_str(
            " No signal inventory on this server, so nothing measured ranks it: context only.",
        );
    } else if s.in_band.is_empty() {
        out.push_str(" No measured emission in this window lies in it: context only.");
    } else {
        out.push_str(&format!(
            " {} in this window lie in it",
            plural(s.in_band.len(), "measured emission", "measured emissions")
        ));
        if s.cited.is_empty() {
            out.push_str(", but none ranked it as an explanation.");
        } else {
            out.push_str(&format!(
                "; {} ranked it among their explanations.",
                s.cited.len()
            ));
        }
    }
    if let Some(worst) = s
        .off_raster
        .iter()
        .max_by(|a, b| a.offset_hz.abs().total_cmp(&b.offset_hz.abs()))
    {
        out.push_str(&format!(
            " {} off the {} raster (largest {} from {}): flagged, not snapped.",
            plural(s.off_raster.len(), "emission sits", "emissions sit"),
            khz(worst.raster_hz),
            khz(worst.offset_hz.abs()),
            mhz(worst.nearest_channel_hz),
        ));
    }
    if row.unverified {
        out.push_str(
            " This row's edges were not individually re-checked against a primary source.",
        );
    }
    out.push_str(" A suggestion, never truth.");
    out
}

/// The window's emitters, read through the same inventory predicate `/api/events` expands
/// (occupied band overlaps the box, presence overlaps the window). `None` with no inventory.
fn measured(
    state: &ApiState,
    r: &crate::query::Region,
    window: TimeRange,
) -> Result<Option<(Vec<Measured>, bool)>, ApiError> {
    let Some(repo) = state.inventory.as_ref() else {
        return Ok(None);
    };
    let repo: std::sync::MutexGuard<'_, Repository> = repo
        .lock()
        .map_err(|_| ApiError::new(500, "inventory store poisoned"))?;
    let query = InventoryQuery {
        freq: Some(r.freq),
        time: Some(window),
        limit: MAX_PRIOR_EMITTERS,
        ..InventoryQuery::default()
    };
    let failed = |_| ApiError::new(500, "priors query failed");
    let page = repo.query_inventory(&query).map_err(failed)?;
    let mut out = Vec::with_capacity(page.entries.len());
    for entry in &page.entries {
        let e = &entry.emitter;
        let explanations = match explanations_json(&repo, e.id).map_err(failed)? {
            Value::Array(a) => a,
            _ => Vec::new(),
        };
        out.push(Measured {
            id: e.id.to_string(),
            f_center_hz: e.f_center_hz,
            explanations,
        });
    }
    Ok(Some((out, page.next_offset.is_some())))
}

/// `/api/priors`: the ranked band-plan allocations intersecting the box, as explanations.
///
/// Ranking, best first: allocations the window's own emitters **cited** in their ranked
/// explanations (more citing emitters first, then the better rank they gave it); then allocations
/// with measured emissions in them; then context-only rows. Ties go to the **narrower** row — the
/// more specific allocation (`ism-433-part15` before the `amateur-70cm` it sits inside) — then to
/// the lower edge.
pub fn priors_json(state: &ApiState, q: &Params) -> Result<Value, ApiError> {
    let r = parse_region(q)?;
    let window = TimeRange::new(
        Timestamp::from_unix_nanos(r.t0_ns),
        Timestamp::from_unix_nanos(r.t1_ns),
    );
    let table = table().ok_or_else(|| ApiError::new(500, "band-plan table failed to load"))?;
    let (measured, emitters_truncated, measured_known) = match measured(state, &r, window)? {
        Some((m, t)) => (m, Some(t), true),
        None => (Vec::new(), None, false),
    };

    let mut rows: Vec<(&AllocationRow, Support)> = table
        .overlapping(r.freq.lo_hz, r.freq.hi_hz)
        .into_iter()
        .map(|row| (row, support_of(row, &measured)))
        .collect();
    let best_rank = |s: &Support| s.cited.iter().map(|c| c.1).min().unwrap_or(u64::MAX);
    rows.sort_by(|(ra, sa), (rb, sb)| {
        sb.cited
            .len()
            .cmp(&sa.cited.len())
            .then_with(|| best_rank(sa).cmp(&best_rank(sb)))
            .then_with(|| sb.in_band.len().cmp(&sa.in_band.len()))
            .then_with(|| ra.freq.width_hz().total_cmp(&rb.freq.width_hz()))
            .then_with(|| ra.freq.lo_hz.total_cmp(&rb.freq.lo_hz))
    });
    let total = rows.len();
    let view_w = r.freq.width_hz();
    let priors: Vec<Value> = rows
        .iter()
        .take(MAX_PRIORS)
        .enumerate()
        .map(|(i, (row, s))| {
            let (lo, hi) = (row.freq.lo_hz.max(r.freq.lo_hz), row.freq.hi_hz.min(r.freq.hi_hz));
            let worst = s
                .off_raster
                .iter()
                .max_by(|a, b| a.offset_hz.abs().total_cmp(&b.offset_hz.abs()));
            json!({
                "rank": i + 1,
                "id": row.id,
                "f_lo_hz": row.freq.lo_hz,
                "f_hi_hz": row.freq.hi_hz,
                // The share of the viewport's width this allocation covers, 0–1 (backend-computed
                // so a client never derives it).
                "viewport_fraction": if view_w > 0.0 { ((hi - lo).max(0.0) / view_w).min(1.0) } else { 0.0 },
                "service": row.primary_services.first().or(row.secondary_services.first()),
                "primary_services": row.primary_services,
                "secondary_services": row.secondary_services,
                "allocation": federal_str(row.federal),
                "tags": row.tags,
                "source": row.prior_ref(),
                "unverified": row.unverified,
                "support": if !measured_known {
                    "no-inventory"
                } else if !s.cited.is_empty() {
                    "cited"
                } else if !s.in_band.is_empty() {
                    "in-band"
                } else {
                    "context"
                },
                "emitters_in_band": s.in_band.len(),
                "emitters_citing": s.cited.len(),
                // Signed centre-minus-channel of the furthest off-raster emission, or null.
                "off_raster_hz": worst.map(|w| w.offset_hz),
                "off_raster": s.off_raster.iter().map(|o| json!({
                    "emitter_id": o.emitter_id,
                    "f_center_hz": o.f_center_hz,
                    "offset_hz": o.offset_hz,
                    "nearest_channel_hz": o.nearest_channel_hz,
                    "raster_hz": o.raster_hz,
                })).collect::<Vec<_>>(),
                "reason": reason(row, s, measured_known),
            })
        })
        .collect();
    Ok(json!({
        "window": {
            "f_lo_hz": r.freq.lo_hz,
            "f_hi_hz": r.freq.hi_hz,
            "t0_s": ts_s(window.start),
            "t1_s": ts_s(window.end),
        },
        "kind": "suggestion",
        "source": PRIORS_SOURCE,
        "region": TableRegion::Us.to_string(),
        "priors": priors,
        "total": total,
        "truncated": total > MAX_PRIORS,
        "emitters_considered": measured.len(),
        "emitters_truncated": emitters_truncated,
        "statement": PRIORS_STATEMENT,
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
                .any(|(m, p)| *m == "GET" && *p == "/api/priors")
        );
    }

    fn fm_row() -> &'static AllocationRow {
        table()
            .unwrap()
            .rows()
            .iter()
            .find(|r| r.id == "fm-broadcast")
            .unwrap()
    }

    #[test]
    fn an_off_raster_emission_is_flagged_from_its_own_stored_explanation() {
        let expl = json!([{
            "rank": 1, "service": "fm-broadcast", "prior_ref": "us-47cfr2106-compact:fm-broadcast",
            "flags": ["off-raster"],
            "evidence": [{"kind": "raster", "raster_hz": 200e3, "nearest_channel_hz": 98.1e6,
                          "offset_hz": 150e3, "tolerance_hz": 20e3, "on_raster": false,
                          "source": "47 CFR 73.201", "center_source": "detected"}]
        }]);
        let m = vec![
            Measured {
                id: "a".into(),
                f_center_hz: 98.25e6,
                explanations: expl.as_array().unwrap().clone(),
            },
            Measured {
                id: "b".into(),
                f_center_hz: 101.1e6,
                explanations: Vec::new(),
            },
        ];
        let s = support_of(fm_row(), &m);
        assert_eq!(s.in_band.len(), 2);
        assert_eq!(s.cited.len(), 1);
        assert_eq!(s.off_raster.len(), 1);
        let why = reason(fm_row(), &s, true);
        assert!(why.contains("150.0 kHz"), "{why}");
        assert!(why.contains("flagged, not snapped"), "{why}");
        assert!(why.contains("never truth"), "{why}");
    }

    #[test]
    fn an_empty_allocation_reads_as_context_only() {
        let s = support_of(fm_row(), &[]);
        let why = reason(fm_row(), &s, true);
        assert!(why.contains("context only"), "{why}");
        let why = reason(fm_row(), &s, false);
        assert!(why.contains("No signal inventory"), "{why}");
    }
}
