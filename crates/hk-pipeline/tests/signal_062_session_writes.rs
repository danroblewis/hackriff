//! T-209 (SIGNAL-062): what one analog chain session writes, through the mock SDR over the real
//! HackRF FM capture (`fm_100p8M_2p4M_l32g30a1_t1p5_5s`, 101.3 MHz, PI 1694), replayed blind.
//!
//! - **One session, one sighting.** The early identification (T-186, leading 1 s window) and the
//!   full window's RDS write are the same observation: the chain's demodulation sightings add 1
//!   to the station's emitter, not 2. The refined tuning is stored once.
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

/// What `emitter`'s count holds from demodulation sightings (the analog chain's): its count less
/// what its other sources (tracks) counted. A re-measurement keeps its own ledger row but adds
/// nothing, so the demodulation rows' counts are not summed.
fn demodulation_count(dir: &std::path::Path, repo: &Repository, emitter: EmitterId) -> i64 {
    let rows = ledger(dir, emitter);
    eprintln!("[{SIGNAL_062}] emitter {emitter} ledger: {rows:?}");
    let others: i64 = rows
        .iter()
        .filter(|r| r.0 != "demodulation")
        .map(|r| r.1)
        .sum();
    repo.emitter(emitter).unwrap().count as i64 - others
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
    let counted = demodulation_count(&dir.0, &repo, id);
    eprintln!(
        "[{SIGNAL_062}] emitter {id}: count {}, demodulation sightings {counted}",
        e.count
    );
    assert_eq!(
        counted, 1,
        "[{SIGNAL_062}] one chain session (early + full window) is one sighting"
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
