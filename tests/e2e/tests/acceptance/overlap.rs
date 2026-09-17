//! T-219 (AWARE-053, C40): overlapping-candidate resolution through the mock SDR, blind.
//!
//! - **One physical station, one shown row.** The real FM fixture's broadcast station appears
//!   once in `/api/inventory`. Any further row over the same emission is still there — kept in
//!   full, listed with `relations=all` — but carries a recorded reason and a link saying which row
//!   it defers to, so the list is not cluttered with offset duplicates of one station.
//! - **The guard.** Two genuinely distinct adjacent stations are never collapsed: each stays its
//!   own shown row with no relation claimed against it.
//!
//! Blind: the truth list is read only in the assertions, after the run. No frequency is looked up
//! and nothing about the scene is configured.

use std::collections::BTreeSet;

use hk_e2e::blind::matching;
use hk_e2e::{Fixture, SynthRequest, synth_or_skip};
use hk_model::sigmf::Datatype;
use hk_model::{EmitterId, InventoryEntry, InventoryQuery, RelationVisibility};
use serde_json::Value;

use crate::blind::{BlindSource, blind_replay, center_tol_hz, inventory_rows, private_truth};
use crate::common::*;

const T219: &str = "T-219";

/// The extent of an `/api/inventory` row, for [`matching`].
fn row_extent(r: &Value) -> (f64, f64) {
    (
        r["f_center_hz"].as_f64().unwrap_or(f64::NAN),
        r["bandwidth_hz"].as_f64().unwrap_or(0.0),
    )
}

/// The extent of a repository inventory entry, for [`matching`].
fn entry_extent(e: &InventoryEntry) -> (f64, f64) {
    (e.emitter.f_center_hz, e.emitter.bandwidth_hz)
}

fn row_id(r: &Value) -> EmitterId {
    r["id"].as_str().unwrap().parse().unwrap()
}

/// One physical station yields exactly one shown row. Rows that defer are kept in full and each
/// carries the reason and the link that put it there.
#[test]
fn t219_one_physical_station_yields_one_shown_row() {
    let Some(run) = crate::signal_062::fm_run() else {
        return;
    };
    let shown = inventory_rows(&run.dir.0);
    let r = repo(&run.dir.0);
    let all = inventory(
        &r,
        InventoryQuery {
            relations: RelationVisibility::All,
            ..InventoryQuery::default()
        },
    );
    let stations = run.fx.of_kind("wfm-broadcast");
    assert!(!stations.is_empty());
    for station in stations {
        let tol = center_tol_hz(station);
        let shown_hits = matching(station, 0.0, &shown, row_extent, tol);
        let all_hits = matching(station, 0.0, &all, entry_extent, tol);
        eprintln!(
            "[{T219}] station {:.4}..{:.4} MHz: {} shown of {} rows over this emission: {:?}",
            station.f_lo_hz / 1e6,
            station.f_hi_hz / 1e6,
            shown_hits.len(),
            all_hits.len(),
            all_hits
                .iter()
                .map(|e| (
                    e.emitter.f_center_hz,
                    e.emitter.bandwidth_hz,
                    r.emitter_relations(e.emitter.id)
                        .unwrap()
                        .first()
                        .map(|x| x.reason.clone())
                ))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            shown_hits.len(),
            1,
            "[{T219}] one physical station, one shown candidate - not several offset duplicates"
        );
        let kept = row_id(shown_hits[0]);
        for e in &all_hits {
            if e.emitter.id == kept {
                continue;
            }
            // Every further row over this emission defers, with its reasoning disclosed and a link
            // to the row that is shown. Nothing was deleted: it is still here, in full.
            let rel = r.emitter_relations(e.emitter.id).unwrap();
            assert!(
                !rel.is_empty(),
                "[{T219}] an unshown row over the station must say why it defers: {:?}",
                e.emitter.id
            );
            assert!(!rel[0].reason.is_empty(), "[{T219}] the reason is recorded");
            assert!(
                r.emitter(e.emitter.id).unwrap().count > 0,
                "[{T219}] the deferring row keeps its observations"
            );
            assert!(
                !r.emitter_relation_history(e.emitter.id).unwrap().is_empty(),
                "[{T219}] the claim is an append-only, reversible record"
            );
        }
    }
}

/// The guard: two genuinely distinct adjacent stations are not merged, suppressed or superseded.
/// The scene is the real FM recording with a second synthetic RDS station 300 kHz above its own,
/// on air with it — two emissions whose −3 dB extents are separated.
#[test]
fn t219_two_distinct_adjacent_stations_are_never_collapsed() {
    let Some((real, _)) = private_truth(crate::signal_062::FM_FIXTURE) else {
        return;
    };
    let near = synth_or_skip!(
        SynthRequest::new("fm_broadcast_rds")
            .seed(7219)
            .datatype(Datatype::Cf32Le)
            .param("sample_rate", 2.4e6)
            .param("center_hz", 100.8e6)
            .param("offset_hz", 800e3)
            .param("duration_s", 5.0)
            .param("power_dbfs", -16.0)
            .param("noise_dbfs", -120.0)
            .param("pi_hex", "7219")
            .param("ps", "T219GUARD")
    );
    let work = TempDir::new("t219-guard");
    let meta = crate::inventory_lifecycle::add_station(&real, &near, &work.0);
    let fx = Fixture::load(&meta).unwrap();
    let stations = fx.of_kind("wfm-broadcast");
    assert_eq!(stations.len(), 2, "[{T219}] two truth stations");

    let run = blind_replay(&meta, "t219-guard", BlindSource::default());
    let r = repo(&run.dir.0);
    let mut ids = BTreeSet::new();
    for station in stations {
        let matched = matching(
            station,
            0.0,
            &run.api_rows,
            row_extent,
            center_tol_hz(station),
        );
        eprintln!(
            "[{T219}] guard station {:.4}..{:.4} MHz: {:?}",
            station.f_lo_hz / 1e6,
            station.f_hi_hz / 1e6,
            matched.iter().map(|x| row_extent(x)).collect::<Vec<_>>()
        );
        assert_eq!(
            matched.len(),
            1,
            "[{T219}] each distinct station stays one shown row"
        );
        let id = row_id(matched[0]);
        assert!(
            matched[0]["relation"].is_null(),
            "[{T219}] a genuinely distinct adjacent station never defers: {}",
            matched[0]
        );
        assert!(
            r.emitter_relations(id).unwrap().is_empty(),
            "[{T219}] no suppression, duplicate or artifact claim against a distinct station"
        );
        ids.insert(id);
    }
    assert_eq!(
        ids.len(),
        2,
        "[{T219}] two adjacent stations stay two entries"
    );
}

// ---------------------------------------------------------------------------------------------
// T-369: overlap is an error signal, checked on the wire the UI actually receives.
// ---------------------------------------------------------------------------------------------

const T369: &str = "T-369";

/// The presence extent of a served row: its last interval, or the hull when a row carries none.
/// Exactly what the waterfall draws a box between (docs/api.md `presence.last_interval`).
fn row_time(r: &Value) -> (f64, f64) {
    let p = &r["presence"]["last_interval"];
    match (p["t_start_s"].as_f64(), p["t_end_s"].as_f64()) {
        (Some(a), Some(b)) => (a, b),
        _ => (
            r["first_seen_s"].as_f64().unwrap_or(f64::NAN),
            r["last_seen_s"].as_f64().unwrap_or(f64::NAN),
        ),
    }
}

/// The band of a served row, as the overlay draws it (`f_lo_hz`, `f_hi_hz`).
fn row_band(r: &Value) -> (f64, f64) {
    (
        r["f_lo_hz"].as_f64().unwrap_or(f64::NAN),
        r["f_hi_hz"].as_f64().unwrap_or(f64::NAN),
    )
}

/// Every pair of served rows whose boxes overlap in **time and frequency** — two boxes drawn on
/// top of each other, which is the error signal itself.
fn stacked(rows: &[Value]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for i in 0..rows.len() {
        for j in (i + 1)..rows.len() {
            let ((alo, ahi), (blo, bhi)) = (row_band(&rows[i]), row_band(&rows[j]));
            let ((at0, at1), (bt0, bt1)) = (row_time(&rows[i]), row_time(&rows[j]));
            if alo.max(blo) < ahi.min(bhi) && at0.max(bt0) <= at1.min(bt1) {
                out.push((i, j));
            }
        }
    }
    out
}

/// Whether the repository holds a re-analysis verdict for this row (a contested finding, or a
/// claim it made) — i.e. the overlap was seen and reasoned about rather than left in silence.
fn has_verdict(r: &hk_model::Repository, id: EmitterId) -> bool {
    r.emitter_relation_history(id).unwrap().iter().any(|x| {
        x.detail
            .as_ref()
            .is_some_and(|d| d.get("verdict").is_some())
    })
}

/// **The property, on the wire.** Drive the real 45 s off-air FM capture through the mock SDR
/// device and read `/api/inventory` from a running server — the same bytes the waterfall lays its
/// boxes out from. No two boxes are drawn stacked, because the region re-analysis collapsed the
/// ones that measured as one emission.
///
/// What this replaces, measured on this very fixture before the change: three candidate boxes at
/// 101.6654-101.6836, 101.6751-101.6922 and 101.6913-101.7054 MHz, all on the air over the same
/// 40 s, overlapping in **two** pairs — and `/api/inventory` served all three with
/// `"relation": null` and **no relation row of any kind in the repository**, standing or revoked.
/// The collapse was not failing to reach the wire; it was never running, because the middle box
/// overlaps its neighbours by 49.7 % and 6 % of the narrower band and `bands_compete` gates every
/// T-219 stage at 60 % of **both**.
#[test]
fn t369_no_two_boxes_are_served_stacked_on_the_real_fm_capture() {
    let Some(run) = crate::fm_band_2026_09_15::run() else {
        return;
    };
    let rows = inventory_rows(&run.dir.0);
    assert!(
        rows.len() >= 8,
        "[{T369}] the scene is not empty: {}",
        rows.len()
    );
    let pairs = stacked(&rows);
    assert!(
        pairs.is_empty(),
        "[{T369}] boxes served stacked in time and frequency: {:?}",
        pairs
            .iter()
            .map(|&(i, j)| (row_band(&rows[i]), row_band(&rows[j])))
            .collect::<Vec<_>>()
    );

    // And the collapse really happened here rather than the scene having nothing to collapse:
    // a row is kept in full, hidden from the default list, deferring for a reason that names the
    // re-analysis and the measured mode behind it.
    let r = repo(&run.dir.0);
    let all = inventory(
        &r,
        InventoryQuery {
            relations: RelationVisibility::All,
            ..InventoryQuery::default()
        },
    );
    let shown: BTreeSet<EmitterId> = rows.iter().map(row_id).collect();
    let deferring: Vec<&InventoryEntry> = all
        .iter()
        .filter(|e| !shown.contains(&e.emitter.id))
        .collect();
    assert_eq!(
        deferring.len(),
        1,
        "[{T369}] one of the stacked boxes collapsed: {:?}",
        deferring
            .iter()
            .map(|e| entry_extent(e))
            .collect::<Vec<_>>()
    );
    let rel = r.emitter_relations(deferring[0].emitter.id).unwrap();
    assert_eq!(rel.len(), 1);
    assert!(
        rel[0].reason.contains("region re-analysis"),
        "[{T369}] the served reason is the re-analysis, not a ranking: {}",
        rel[0].reason
    );
    let detail = rel[0].detail.as_ref().unwrap();
    assert_eq!(detail["verdict"], "one-emission");
    assert_eq!(
        detail["members"], 3,
        "[{T369}] three boxes were in the region"
    );
    assert!(
        r.emitter(deferring[0].emitter.id).unwrap().count > 0,
        "[{T369}] the collapsed row keeps its observations"
    );
    eprintln!(
        "[{T369}] 45 s real capture: {} shown, 0 stacked; {:?} collapsed into the region's \
         best-supported box",
        rows.len(),
        entry_extent(deferring[0])
    );
}

/// **The honest half.** On the 5 s capture one pair genuinely cannot be resolved — a 9.4 kHz box
/// measured in a single frame wholly inside a 22.1 kHz box, 2.35x apart in bandwidth, which is the
/// geometry of a subcarrier as much as of a fragment. Merging is the dangerous direction, so it is
/// **not** merged. What must never happen is the old behaviour: two boxes stacked with nothing
/// anywhere saying the system noticed. Every stacked pair the wire still serves carries a recorded
/// verdict naming the region and what blocked the merge.
#[test]
fn t369_every_stacked_pair_still_served_carries_a_recorded_verdict() {
    let Some(run) = crate::signal_062::fm_run() else {
        return;
    };
    let rows = inventory_rows(&run.dir.0);
    let r = repo(&run.dir.0);
    for (i, j) in stacked(&rows) {
        let (a, b) = (row_id(&rows[i]), row_id(&rows[j]));
        assert!(
            has_verdict(&r, a) || has_verdict(&r, b),
            "[{T369}] {:?} and {:?} are served stacked with no re-analysis verdict at all",
            row_band(&rows[i]),
            row_band(&rows[j])
        );
        let v: Vec<String> = r
            .emitter_relation_history(b)
            .unwrap()
            .into_iter()
            .filter(|x| {
                x.detail
                    .as_ref()
                    .is_some_and(|d| d.get("verdict").is_some())
            })
            .map(|x| x.reason)
            .collect();
        eprintln!("[{T369}] contested and left alone, with reasons: {v:?}");
    }
}
