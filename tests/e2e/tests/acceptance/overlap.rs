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
/// The scene is the real FM recording with a second synthetic RDS station 500 kHz above its own,
/// on air with it — two emissions whose −3 dB extents are separated.
///
/// T-402: was 300 kHz (800 kHz offset). `fm_broadcast_rds` used to measure ~100 kHz OBW99
/// (unregulated peak deviation); regulating it to a realistic ~75 kHz peak deviation widens it to
/// ~190-220 kHz, close to what the recording's own real station measures, which collapsed 300 kHz
/// of separation into an overlap (this guard's own `assert_eq!(matched.len(), 1, ...)` started
/// failing with zero matches — the two stations were no longer "whose −3 dB extents are
/// separated" at all). 500 kHz (1000 kHz offset) restores that separation; see
/// `inventory_lifecycle::t082_two_nearby_fm_stations_stay_two_entries`, which is the same scene
/// shape and got the same fix.
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
            .param("offset_hz", 1_000e3)
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

// ---------------------------------------------------------------------------------------------
// T-390: one emission where there is one emission — the skirt geometry, on served rows.
// ---------------------------------------------------------------------------------------------

const T390: &str = "T-390";

/// **The property, with the measured edges.** Drive the user's real 45 s FM capture through the
/// mock SDR device and read `/api/inventory`: over each broadcast station the truth list holds,
/// **exactly one** served row's band intersects that station's occupied band, and no served row's
/// band lies inside a far wider concurrent row's band.
///
/// Blind: the truth edges are read only here, in the assertion, and the emission is found by
/// detection alone. What they pin is the failure the user saw on the live band — dotted candidate
/// boxes sitting in a confirmed station's skirt, and stacked on each other. Measured on this
/// fixture (hk replay, 2026-09-16), the station at 101.2988 MHz reports continuous 1000 ms
/// components 140 kHz wide at SNR 22–26 dB and −17 dBFS; in the same seconds the detector also
/// emits 6 ms, 1–3 bin components at SNR 3.7–7.6 dB and −37.7 to −45.3 dBFS at 101.26115,
/// 101.36517, 101.37074 and 101.38125 MHz, and the 99.6994 MHz station (89 kHz, SNR 13–17 dB,
/// −29 dBFS) draws the same 6–8 ms, SNR 5.2–6.9 dB boxes across 99.62812–99.65783 and
/// 99.73143–99.73898 MHz. Every one of them is 12–29 dB below its parent and one to two frames
/// long against its 45 s: that emission's own skirt. None of them may reach the inventory as a
/// row of its own, and the station must not be split by them either.
///
/// The counterpart guard is `t219_two_distinct_adjacent_stations_are_never_collapsed` above: this
/// test may never be satisfied by collapsing two genuinely distinct emitters (T-233).
#[test]
fn t390_one_row_per_station_and_no_row_inside_a_wider_one() {
    let Some(run) = crate::signal_062::fm_run() else {
        return;
    };
    let rows = inventory_rows(&run.dir.0);
    let stations = run.fx.of_kind("wfm-broadcast");
    assert!(!stations.is_empty(), "[{T390}] the truth list has stations");

    for station in stations {
        let (lo, hi) = (station.f_lo_hz, station.f_hi_hz);
        let over: Vec<(f64, f64)> = rows
            .iter()
            .map(row_band)
            .filter(|&(a, b)| a.max(lo) < b.min(hi))
            .collect();
        eprintln!(
            "[{T390}] station {:.5}..{:.5} MHz: {} served row(s) over it: {:?}",
            lo / 1e6,
            hi / 1e6,
            over.len(),
            over.iter()
                .map(|&(a, b)| (a / 1e6, b / 1e6))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            over.len(),
            1,
            "[{T390}] one emission, one row over its occupied band — not the station plus its \
             skirt boxes"
        );
    }

    // And the containment geometry the user sees as a dotted box drawn inside a solid one: no
    // served row's band lies within a concurrent row's band that is far wider. The width ratio is
    // the tracker's own in-band fragment gate, which is what keeps two adjacent broadcast stations
    // (whose bands genuinely overlap, but within a factor of 4) two rows.
    for (i, j) in stacked(&rows) {
        let ((alo, ahi), (blo, bhi)) = (row_band(&rows[i]), row_band(&rows[j]));
        let (narrow, wide) = if ahi - alo <= bhi - blo {
            ((alo, ahi), (blo, bhi))
        } else {
            ((blo, bhi), (alo, ahi))
        };
        let ratio = (wide.1 - wide.0) / (narrow.1 - narrow.0).max(f64::MIN_POSITIVE);
        assert!(
            !(wide.0 <= narrow.0 && narrow.1 <= wide.1 && ratio >= 4.0),
            "[{T390}] a {:.1} kHz row served inside a {:.1} kHz row {ratio:.1}x wider: \
             {narrow:?} inside {wide:?}",
            (narrow.1 - narrow.0) / 1e3,
            (wide.1 - wide.0) / 1e3
        );
    }
}
