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
