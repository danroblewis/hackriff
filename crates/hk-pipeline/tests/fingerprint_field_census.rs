//! T-594: which `Fingerprint` fields does a **real run** actually supply to entity resolution?
//!
//! A comparison in [`hk_model::Fingerprint::compare`] fires only when **both** sides carry the
//! field. A field no production path ever writes therefore contributes nothing, and it does so
//! silently: a stage structurally unable to fire looks exactly like one that fired and agreed
//! (the T-321 / T-589 class). This census replays real HackRF captures blind through the mock SDR
//! — the full pipeline, every chain that runs on them — and counts, over every stored emitter
//! fingerprint:
//!
//! - how many fingerprints carry each field, and
//! - how many fingerprint **pairs** carry it on both sides, i.e. how many comparisons it could
//!   possibly have taken part in. Resolution compares a sighting against stored rows, and a stored
//!   row is the fold of its sightings (a field present on any sighting survives the fold), so a
//!   field absent from every stored row was absent from every comparison: at zero the bound is
//!   exact.
//!
//! **What it found (T-594).** Before T-594 the fingerprint carried `structure` (T-233's
//! `envelope_shape`), and over these runs it was present on **0** stored fingerprints, so it took
//! part in **0** comparisons: `hk_classify::modulation_structure` had no caller outside its own
//! tests and a probe binary. T-594 removed the field and its comparison rather than wiring a
//! producer, because every producer that holds IQ also writes an exact `family`, and every such
//! family is constant-envelope (`wfm`, `nfm`, the FSK chain, the four-level trunking control
//! channel), for which the statistic reads 1.00 by construction — see
//! `hk_model::cluster::Fingerprint::compare`. The test now asserts the removal holds: no real-run
//! fingerprint carries a key that is not a field `compare` reads.
//!
//! The per-field table is printed on every run (`--no-capture`), so the next field that loses its
//! producer shows up here as a zero instead of as nothing.

mod common;

use std::collections::BTreeMap;

use common::*;
use hk_core::{MockEnd, Pacing};
use hk_model::InventoryQuery;
use hk_pipeline::{Pipeline, PipelineConfig, TrackInventory, open_mock_replay, replay_plan};
use serde_json::json;

/// The fields `Fingerprint::compare` reads, by their serialised key. `version` and
/// `observations` are bookkeeping, never compared.
const COMPARED: &[&str] = &[
    "f_center_hz",
    "bandwidth_hz",
    "family",
    "symbol_rate_hz",
    "deviation_hz",
    "period_s",
    "duty_cycle",
    "burst_length_s",
    "hop_raster_hz",
    "hop_set_hz",
];
const BOOKKEEPING: &[&str] = &["version", "observations"];

/// Real captures with different chains on them: broadcast FM (the analogue chain + RDS) and the
/// 433 MHz ISM band (short FSK/OOK bursts).
const RUNS: &[&str] = &[
    "fm_100p8M_2p4M_l32g30a1_t1p5_5s",
    "ism_433p62M_2M_l24g30a1_t162p0_6s",
];

/// Replays `meta` blind through the mock SDR (unpaced, lossless) and returns every stored emitter
/// fingerprint as its raw JSON object.
fn stored_fingerprints(
    name: &str,
    meta: &std::path::Path,
) -> Vec<serde_json::Map<String, serde_json::Value>> {
    let dir = TempDir::new("t594-census");
    let input = TempDir::new("t594-blind");
    let blind = blind_meta(meta, &input.0);
    let dev = open_mock_replay(&blind, Pacing::Unpaced, MockEnd::Stop).unwrap();
    let info = dev.info;
    let mut plan = replay_plan(info.center_hz, info.sample_rate_hz, info.start_time);
    plan.extra = json!({});
    let mut cfg = PipelineConfig::new(&dir.0, plan).unwrap();
    cfg.source_class = dev.class;
    cfg.lossless = true;
    let handle = Pipeline::start(
        cfg,
        Box::new(dev.source),
        info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let s = handle.wait().unwrap();
    assert!(s.errors.is_empty(), "[{name}] {:?}", s.errors);
    let repo = repo(&dir.0);
    inventory(&repo, InventoryQuery::default())
        .into_iter()
        .filter_map(|e| e.emitter.fingerprint.as_object().cloned())
        .collect()
}

#[test]
fn t594_real_run_fingerprints_carry_only_fields_entity_resolution_compares() {
    let mut runs = Vec::new();
    for name in RUNS {
        let Some(meta) = real_fixture(name) else {
            return;
        };
        let got = stored_fingerprints(name, &meta);
        eprintln!("[T-594] {name}: {} stored fingerprints", got.len());
        runs.push(got);
    }
    let fps: Vec<_> = runs.iter().flatten().cloned().collect();
    // Anti-vacuity: a census of nothing proves nothing about what is supplied.
    assert!(
        fps.len() >= 2,
        "[T-594] the real runs stored {} fingerprints; the census judges nothing",
        fps.len()
    );

    let mut carry: BTreeMap<&str, usize> = BTreeMap::new();
    let mut pairs: BTreeMap<&str, usize> = BTreeMap::new();
    // Present = not null, not an empty hop set, and — for `bandwidth_hz` only, whose `0` the type
    // documents as "unknown" — not zero. `compare` skips a zero bandwidth the same way.
    let has = |fp: &serde_json::Map<String, serde_json::Value>, k: &str| {
        fp.get(k).is_some_and(|v| {
            let empty_set = v.as_array().is_some_and(Vec::is_empty);
            let unknown_bw = k == "bandwidth_hz" && v.as_f64().is_some_and(|b| b <= 0.0);
            !(v.is_null() || empty_set || unknown_bw)
        })
    };
    // Pairs are counted within a run: rows from two separate replays never meet in one store.
    let pairs_of = |n: usize| n * n.saturating_sub(1) / 2;
    for &k in COMPARED {
        carry.insert(k, fps.iter().filter(|fp| has(fp, k)).count());
        let usable = runs
            .iter()
            .map(|r| pairs_of(r.iter().filter(|fp| has(fp, k)).count()))
            .sum();
        pairs.insert(k, usable);
    }
    let total_pairs: usize = runs.iter().map(|r| pairs_of(r.len())).sum();
    eprintln!(
        "[T-594] {} fingerprints, {total_pairs} pairs\n{:<16} {:>8} {:>14}",
        fps.len(),
        "field",
        "carried",
        "pairs-usable"
    );
    for &k in COMPARED {
        eprintln!("{k:<16} {:>8} {:>14}", carry[k], pairs[k]);
    }

    // What every real fingerprint must carry for resolution to compare anything at all.
    assert_eq!(carry["f_center_hz"], fps.len(), "[T-594] {fps:?}");
    // The analogue chain's family label reached the store (the run is not tracker-only).
    assert!(
        carry["family"] >= 1,
        "[T-594] no stored fingerprint carries a family; the chains did not reach the store"
    );

    // The removal holds: nothing stored outside the compared set. A key here would be a field
    // resolution never reads — exactly the dead comparison T-594 removed.
    let mut unread: BTreeMap<String, usize> = BTreeMap::new();
    for fp in &fps {
        for k in fp.keys() {
            if !COMPARED.contains(&k.as_str()) && !BOOKKEEPING.contains(&k.as_str()) {
                *unread.entry(k.clone()).or_default() += 1;
            }
        }
    }
    assert!(
        unread.is_empty(),
        "[T-594] real-run fingerprints carry fields entity resolution never compares: {unread:?}"
    );
    // And specifically T-233's field: carried by none, so used by no comparison.
    assert_eq!(
        fps.iter().filter(|fp| fp.contains_key("structure")).count(),
        0
    );
}
