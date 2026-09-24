//! T-844: the C38 shadow stage **wired on a path that runs**, through the mock SDR.
//!
//! This is the *wiring* half of T-844's non-vacuity pair (the T-287/T-297 pattern). An operator
//! installs a model and puts it in `shadow`; a scene of 2-FSK bursts replays through the device
//! interface; the fsk chain classifies each emission at the classifier's single call site; and
//! the test asserts that shadow records **reached hk-store** — read back from disk by a store
//! opened afresh, not believed from a counter. Remove the `MlStage::observe` call from
//! `chains/fsk.rs` and this fails, while the sink half (`src/ml.rs`,
//! `a_shadow_prediction_is_durable_in_hk_store_and_survives_the_stage`) still passes; replace the
//! hk-store sink with the in-memory one and that test fails while this one's counters still move.
//!
//! **Shadow means shadow**, asserted over the same run: no `Classification` row names a model or
//! the DL stage, the published rows are the ones a run with no model publishes, and ADR-0016
//! §7's exit gate — read from the stage's *real* mode table — holds with ML on.

mod common;

use std::collections::BTreeMap;

use common::*;
use hk_core::Pacing;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_ml::exit_gate::MlAttributedRow;
use hk_model::classify::Stage;
use hk_model::{DetectionId, InventoryQuery, Repository};
use hk_pipeline::ml::{self, MlMode, MlStage, ModeRequest};
use hk_store::ml::{ShadowQuery, ShadowStore};
use serde_json::json;

const FSK: [&str; 4] = ["2fsk", "gfsk", "msk", "4fsk"];

/// `(family, class)` of every classification row the run persisted, per emitter centre (kHz), in
/// order — what a reader of the inventory would see, independent of the run's random ids.
type Published = BTreeMap<i64, Vec<(String, Option<String>, String)>>;

fn published(repo: &Repository) -> Published {
    let mut out = Published::new();
    for e in inventory(repo, InventoryQuery::default()) {
        let key = (e.emitter.f_center_hz / 1e3).round() as i64;
        let rows = repo.classification_history(e.emitter.id).unwrap();
        out.entry(key)
            .or_default()
            .extend(rows.into_iter().map(|r| {
                let d = r.detail.as_ref();
                (
                    d.map_or_else(String::new, |d| d.family.clone()),
                    d.and_then(|d| d.class.as_ref()).map(|c| c.label.clone()),
                    r.stage.as_str().to_owned(),
                )
            }));
    }
    out
}

/// Classification rows attributed to a model (the exit gate's clause 1 input).
fn ml_rows(repo: &Repository) -> Vec<MlAttributedRow> {
    let mut out = Vec::new();
    for e in inventory(repo, InventoryQuery::default()) {
        for r in repo.classification_history(e.emitter.id).unwrap() {
            let model = r.detail.as_ref().and_then(|d| d.provenance.ml.as_ref());
            if model.is_some() || r.stage == Stage::Dl {
                out.push(MlAttributedRow {
                    subject: format!("{:?}", e.emitter.id),
                    model: model.map(|m| m.id.clone()),
                    stage: r.stage.as_str().to_owned(),
                });
            }
        }
    }
    out
}

#[test]
fn a_shadow_model_records_every_classified_fsk_emission_in_hk_store_and_changes_nothing() {
    let out = synth_or_skip!(
        SynthRequest::new("fsk_burst_train")
            .seed(844)
            .param("snr_db", 20.0)
            .param("duration_s", 1.2)
    );
    let meta = out.fixture(0).unwrap().meta_path;

    // The run with no model: what the classical cascade publishes on its own.
    let plain = TempDir::new("ml-plain");
    let (cfg, replay, _input) = blind_replay_config(&plain.0, &meta, json!({}), Pacing::Unpaced);
    let s = start(cfg, replay).wait().unwrap();
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    let baseline = published(&repo(&plain.0));
    assert!(
        baseline.values().flatten().any(|(f, ..)| f == "fsk"),
        "the scene must produce fsk classifications, or nothing below is exercised: {baseline:?}"
    );
    assert_eq!(
        ShadowStore::open(ml::shadow_dir(&plain.0))
            .unwrap()
            .stats()
            .records,
        0,
        "no model installed: the stage is idle and writes nothing"
    );

    // An operator installs a model and puts it in shadow (the PUT route's own call).
    let dir = TempDir::new("ml-shadow");
    let probe = ml::install_probe_model(&dir.0, "fsk", &FSK).unwrap();
    MlStage::open(&dir.0)
        .unwrap()
        .set_mode(&ModeRequest {
            id: probe.id.clone(),
            mode: MlMode::Shadow,
            ..ModeRequest::default()
        })
        .unwrap();

    let (cfg, replay, _input) = blind_replay_config(&dir.0, &meta, json!({}), Pacing::Unpaced);
    let handle = start(cfg, replay);
    let stage = handle.ml().expect("the run's ML stage opened");
    let s = handle.wait().unwrap();
    eprintln!("{}", s.to_text());
    assert!(s.errors.is_empty(), "{:?}", s.errors);

    // The records reached hk-store: counted from disk, by a store the run never held.
    let store = ShadowStore::open(ml::shadow_dir(&dir.0)).unwrap();
    let recs = store
        .query(&ShadowQuery {
            limit: Some(1000),
            ..ShadowQuery::default()
        })
        .unwrap();
    let observed = stage
        .stats
        .observed
        .load(std::sync::atomic::Ordering::Relaxed);
    eprintln!(
        "shadow: {} records on disk, producer {}",
        recs.len(),
        stage.models_json()["producer"]
    );
    assert!(
        !recs.is_empty(),
        "a model in shadow over a run that classified fsk emissions wrote no shadow record to \
         hk-store: the producer is not wired (producer {})",
        stage.models_json()["producer"]
    );
    assert_eq!(recs.len() as u64, observed, "every prediction is on disk");
    let repo = repo(&dir.0);
    for r in &recs {
        assert_eq!(r.model, probe.to_string());
        assert_eq!(r.prediction.mode, "shadow");
        assert_eq!(
            r.classical.family, "fsk",
            "a model runs only in its own family"
        );
        assert!(FSK.contains(&r.prediction.label.as_str()));
        let d: DetectionId = r.subject.detection.parse().unwrap();
        assert!(
            repo.detection(d).is_ok(),
            "the subject is a stored CFAR detection"
        );
    }
    let agg = store.aggregates(&ShadowQuery::default()).unwrap();
    assert_eq!(
        agg.iter().map(|a| a.counts.n).sum::<u64>(),
        recs.len() as u64
    );

    // Shadow means shadow: no row from a model, and the published rows are the plain run's.
    let rows = ml_rows(&repo);
    assert!(
        rows.is_empty(),
        "a shadow model wrote a classification: {rows:?}"
    );
    assert_eq!(
        published(&repo),
        baseline,
        "the shadow stage changed what the classical cascade published"
    );

    // ADR-0016 §7's ML row over the stage's real mode table: ML was on, and it holds.
    let snap = stage.gate_snapshot(rows, s.always_on_lost_samples);
    eprintln!("[T-844] {}", snap.summary());
    assert!(snap.ml_on().is_some(), "{}", snap.summary());
    assert!(snap.check().is_empty(), "{:?}", snap.check());
}
