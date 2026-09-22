//! T-209 (SIGNAL-062): what one analog chain session writes, through the mock SDR over the real
//! HackRF FM capture (`fm_100p8M_2p4M_l32g30a1_t1p5_5s`, 101.3 MHz, PI 1694), replayed blind.
//!
//! - **One session, one occurrence.** The early identification (T-186, leading 1 s window) and
//!   the full window's RDS write are the same observation, and the tracker watched the same
//!   stretch of air: the station's emitter counts **one** occurrence for the session, not one per
//!   producer that saw it (T-209; semantics settled in T-329, and stated on
//!   `hk_model::repo::cluster::resolve`). The refined tuning is stored once.
//! - **Overload gate.** The same IQ with the capture's provenance marked overloaded (a front end
//!   in compression shows intermodulation images that can lock a pilot): no emitter is placed from
//!   mode evidence alone, early or late. An emitter carrying a CRC-valid decoded identity still
//!   may be. The unmodified capture still identifies early.

mod common;

use common::*;
use hk_core::{MockEnd, Pacing};
use hk_model::{EmitterId, IdentityScheme, InventoryIdentity, InventoryQuery, Repository};
use hk_pipeline::{
    Pipeline, PipelineConfig, RunSummary, TrackInventory, open_mock_replay, replay_plan,
};
use serde_json::json;

const SIGNAL_062: &str = "SIGNAL-062";
const NAME: &str = "fm_100p8M_2p4M_l32g30a1_t1p5_5s";

/// Replays `meta` blind through the mock SDR (unpaced, lossless).
fn run_mock(dir: &std::path::Path, meta: &std::path::Path) -> RunSummary {
    let input = TempDir::new("t209-blind");
    let blind = blind_meta(meta, &input.0);
    let dev = open_mock_replay(&blind, Pacing::Unpaced, MockEnd::Stop).unwrap();
    let info = dev.info;
    let mut plan = replay_plan(info.center_hz, info.sample_rate_hz, info.start_time);
    plan.extra = json!({});
    let mut cfg = PipelineConfig::new(dir, plan).unwrap();
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
    eprintln!("{}", s.to_text());
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    s
}

/// `emitter`'s observation ledger rows: (source kind, count, measurement key).
fn ledger(dir: &std::path::Path, emitter: EmitterId) -> Vec<(String, i64, Option<String>)> {
    let conn = rusqlite::Connection::open(dir.join("hackriff.db")).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT source_kind, count, measurement FROM emitter_observation \
             WHERE emitter_id = ?1 ORDER BY t_start",
        )
        .unwrap();
    stmt.query_map([emitter.as_uuid().as_bytes().to_vec()], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

fn pi_1694(repo: &Repository) -> Option<EmitterId> {
    inventory(
        repo,
        InventoryQuery {
            identity_scheme: Some(IdentityScheme::RdsPi),
            ..InventoryQuery::default()
        },
    )
    .into_iter()
    .find(|e| {
        matches!(&e.identity, InventoryIdentity::Clear { identity, .. } if identity.value == "1694")
    })
    .map(|e| e.emitter.id)
}

/// **T-605: a real analogue run swallows no storage error.**
///
/// This scene is the one that produced
/// `hk-pipeline: analog chain shape characterisation: storage engine: UNIQUE constraint failed:
/// emission_features.features_id` in six gates, including green ones. The error was caught,
/// printed and stepped past, so a run that lost the T-321 shape characterisation — and with it
/// the fourth field that keeps a purely analogue emitter above the clustering floor — looked
/// exactly like a run that wrote it.
///
/// The assertion is a **count**, not a log grep: every chain site that catches a `RepoError` and
/// carries on now goes through `hk_pipeline::stats::storage_error`, which counts it in
/// `/chains/storage_errors` as well as naming it. Zero is the only acceptable value, because a
/// storage error on this path is always a write that should have happened.
///
/// Anti-vacuity: the run must actually have demodulated an analogue session and characterised
/// its shape, or "zero storage errors" is a statement about a scene with nothing in it.
#[test]
fn t605_an_analogue_run_swallows_no_storage_error() {
    let Some(meta) = real_fixture(NAME) else {
        return;
    };
    let dir = TempDir::new("t605-storage");
    let s = run_mock(&dir.0, &meta);
    assert!(
        s.counter("/chains/demodulations") >= 1,
        "[{SIGNAL_062}] no analogue session ran, so this test judges nothing"
    );
    assert!(
        s.counter("/chains/identifications") >= 1,
        "[{SIGNAL_062}] the identify path (one of T-321's two characterise sites) never ran, so \
         this test judges nothing"
    );
    assert_eq!(
        s.counter("/chains/storage_errors"),
        0,
        "[{SIGNAL_062}] a chain caught a storage error and carried on; a run that cannot write \
         must not look like a run that wrote"
    );
}

#[test]
fn signal_062_one_session_counts_one_sighting_and_stores_its_refined_tuning_once() {
    let Some(meta) = real_fixture(NAME) else {
        return;
    };
    let dir = TempDir::new("t209-once");
    let s = run_mock(&dir.0, &meta);
    assert!(
        s.counter("/chains/identifications") >= 1,
        "[{SIGNAL_062}] the station is identified early"
    );
    assert_eq!(s.counter("/chains/mode_emitters_withheld"), 0);
    let repo = repo(&dir.0);
    let id = pi_1694(&repo).unwrap_or_else(|| panic!("[{SIGNAL_062}] no PI 1694 entry"));
    let e = repo.emitter(id).unwrap();
    let rows = ledger(&dir.0, id);
    eprintln!(
        "[{SIGNAL_062}] emitter {id}: count {}, ledger {rows:?}",
        e.count
    );
    // The chain wrote this session twice — the leading 1 s identification window and the full
    // window — and both rows are kept, under its one producer key. The ledger records who
    // measured what; it is not a tally of occurrences.
    let chain: Vec<_> = rows.iter().filter(|r| r.0 == "demodulation").collect();
    assert_eq!(
        chain.len(),
        2,
        "[{SIGNAL_062}] the early and full-window writes both reached the ledger: {rows:?}"
    );
    assert_eq!(
        chain[0].2, chain[1].2,
        "[{SIGNAL_062}] both offered under one producer key, so the second re-measures the \
         first rather than counting again: {chain:?}"
    );
    assert!(
        chain[0]
            .2
            .as_deref()
            .is_some_and(|k| k.contains("analog-chain")),
        "[{SIGNAL_062}] that key is the analog chain's: {chain:?}"
    );
    // **One session, one occurrence** (T-209; semantics settled in T-329, and stated in full on
    // `hk_model::repo::cluster::resolve`). `count` is a lifetime total of occurrences — times
    // this emitter was observed on the air — not a tally of the writes that reached the
    // repository (docs/07 §2.11, ADR-0017). This capture is one station, on the air continuously
    // for its whole 5 s, seen by the chain twice (early + full window) and by the tracker once,
    // all over that same stretch. That is ONE occurrence: the chain's full-window write resolves
    // as a re-measurement of its early one (T-209), and the tracker's entry folds in through the
    // same-emission merge, which discounts the shared stretch (docs/07 §2.11, "overlapping
    // observations not counted twice").
    //
    // Do not re-derive this by subtraction. This assertion used to read
    // `count - Σ(non-demodulation ledger rows) == 1`, inferring the chain's share by taking the
    // tracker's row at face value. A row's `count` is what its producer **measured**, never what
    // it **added**: a row that arrived by a discounted merge (or as a re-measurement) reads 1
    // and added 0. T-316 made that difference visible here — with its false alarms gone the
    // tracker's entry began merging in instead of landing directly on the chain's entry — and
    // the subtraction then credited the chain with 0 for a session it had counted correctly,
    // while the raw 2 it used to see was the shared 5 s counted twice.
    assert_eq!(
        e.count, 1,
        "[{SIGNAL_062}] one session over one continuous emission is one occurrence, however \
         many producers saw it: {rows:?}"
    );
    let tunings = repo.refined_tuning_history(id).unwrap();
    assert_eq!(
        tunings.len(),
        1,
        "[{SIGNAL_062}] one refined tuning per session: {tunings:?}"
    );
}

#[test]
fn signal_062_overloaded_capture_places_no_emitter_from_mode_evidence() {
    let Some(meta) = real_fixture(NAME) else {
        return;
    };
    // The same IQ, its recorded provenance marked overloaded.
    let src = TempDir::new("t209-overload-src");
    let mut m: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&meta).unwrap()).unwrap();
    let g = m["global"].as_object_mut().unwrap();
    g.remove("core:sha512");
    g["hackriff:provenance"]["overload"] = json!(true);
    let over_meta = src.0.join("overload.sigmf-meta");
    std::fs::write(&over_meta, serde_json::to_string_pretty(&m).unwrap()).unwrap();
    std::fs::copy(
        meta.with_extension("sigmf-data"),
        src.0.join("overload.sigmf-data"),
    )
    .unwrap();

    let dir = TempDir::new("t209-overload");
    let s = run_mock(&dir.0, &over_meta);
    assert!(
        s.counter("/chains/demodulations") >= 1,
        "[{SIGNAL_062}] the chain ran on the overloaded capture (not a vacuous pass)"
    );
    assert!(
        s.counter("/chains/mode_emitters_withheld") >= 1,
        "[{SIGNAL_062}] the overload gate refused a mode-evidence emitter"
    );
    assert_eq!(
        s.counter("/chains/identifications"),
        0,
        "[{SIGNAL_062}] no early identification under overload"
    );
    let repo = repo(&dir.0);
    for e in inventory(&repo, InventoryQuery::default()) {
        let decoded = matches!(e.identity, InventoryIdentity::Clear { .. });
        // Every emitter placed by a chain sighting holds a demodulation row in its ledger.
        let rows = ledger(&dir.0, e.emitter.id);
        assert!(
            decoded || !rows.iter().any(|r| r.0 == "demodulation"),
            "[{SIGNAL_062}] entry {} placed from mode evidence under overload",
            e.emitter.id
        );
        assert!(
            decoded || !e.emitter.classifications.iter().any(|c| c.family == "wfm"),
            "[{SIGNAL_062}] entry {} classified wfm without a decoded identity under overload",
            e.emitter.id
        );
    }
}
