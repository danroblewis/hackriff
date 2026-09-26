//! T-366, the M3 exit gate's **ML row** (ADR-0016 §7), asserted over a real run through the mock
//! SDR — because until this file existed it was asserted nowhere.
//!
//! §7's exit-gate table carries one ML row:
//!
//! > shadow changes no Classification row; each `active` family has enable evidence; zero ring
//! > sample drops with ML on
//!
//! and `m3_grid.rs` / `m3_scene.rs` contained **no ML assertion at all**, so the row was satisfied
//! by nothing checking it. That is the shape of T-206's two vacuous gate dimensions, which this
//! project already paid to find once.
//!
//! # What is measured, and where the modes come from
//!
//! What this file asserts is a **predicate over what the run actually produced**
//! ([`hk_ml::exit_gate`]): every ML-attributed `Classification` row the run persisted must name a
//! model that is `active` with §4.6 enable evidence behind it, and a run with any mode on must
//! lose no samples. The `(model, consumer)` modes are **the run's own**, read from the ML stage
//! T-844 wired into the pipeline (`hk_pipeline::ml::MlStage::gate_snapshot`, one
//! [`MlGateSnapshot::from_host`] per host), and the shadow records are counted from hk-store's
//! durable log on disk — neither is this file's opinion. A run whose data directory has no model
//! installed has an idle stage: no mode, no record, no ML row, and the gate says clause 3 was not
//! exercised rather than claiming it held.
//!
//! **Non-vacuity is shown, not argued** (the T-287/T-297 pattern):
//! [`m3_ml_exit_gate_catches_the_states_the_adr_row_forbids`] takes *this run's own snapshot* and
//! constructs each forbidden state on it, asserting the gate reports each one. The same proof is
//! run against a live host inside `hk-ml` (`host.rs`,
//! `a_forced_active_model_without_evidence_fails_the_adr_0016_s7_exit_gate`) and against the
//! pipeline's stage (`hk_pipeline::ml`,
//! `active_is_refused_without_evidence_and_a_forced_active_fails_the_exit_gate`), where the one
//! path that can reach `active` without evidence — a forced mode change — is exercised for real.

use std::path::Path;

use hk_core::Pacing;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_ml::MlMode;
use hk_ml::exit_gate::{GateViolation, MlAttributedRow, MlGateSnapshot, ModeInForce};
use hk_model::classify::Stage;
use hk_model::{InventoryQuery, Repository};
use hk_pipeline::ml::{MlStage, shadow_dir};
use hk_store::ml::ShadowStore;
use serde_json::json;

use crate::blind::{replay_config, start};
use crate::common::*;

const T206: &str = "T-206";

/// The scene both tests drive: 2-FSK bursts a run will detect, classify and persist rows for, so
/// the gate is measured over a run that really produced `Classification` rows rather than over an
/// empty store.
///
/// A macro rather than a function because [`synth_or_skip!`] skips by returning from the enclosing
/// test, which only a `#[test]` body can do (the `m3_scene` pattern).
macro_rules! sensor_scene {
    () => {{
        let out = synth_or_skip!(
            SynthRequest::new("fsk_burst_train")
                .seed(366)
                .param("snr_db", 20.0)
                .param("duration_s", 1.2)
        );
        out.fixture(0).unwrap().meta_path
    }};
}

/// Runs a scene through the mock SDR and returns its store, the samples the ring lost, and the
/// run's ML stage (whose modes are the gate's clause 2 input).
fn run(dir: &Path, meta: &Path) -> (Repository, u64, std::sync::Arc<MlStage>) {
    let (cfg, replay) = replay_config(dir, meta, json!({}), Pacing::Unpaced);
    let handle = start(cfg, replay);
    let stage = handle
        .ml()
        .expect("the run's ML stage opened (its store is under the data directory)");
    let summary = finish(handle);
    (repo(dir), summary.always_on_lost_samples, stage)
}

/// Every `Classification` row the run persisted that is **attributed to a model**: one whose
/// provenance names a model, or one written by the DL stage.
///
/// Read from the classification *history*, not the arbitrating row, for T-247's reason: a C15/DL
/// row may legitimately sit below a chain's label, and a row that decided nothing is still a row a
/// shadow model must never have written.
fn ml_rows(repo: &Repository) -> Vec<MlAttributedRow> {
    let mut out = Vec::new();
    for entry in inventory(repo, InventoryQuery::default()) {
        let id = entry.emitter.id;
        for r in repo.classification_history(id).unwrap() {
            let model = r.detail.as_ref().and_then(|d| d.provenance.ml.as_ref());
            if model.is_some() || r.stage == Stage::Dl {
                out.push(MlAttributedRow {
                    subject: format!("{id:?}"),
                    model: model.map(|m| m.id.clone()),
                    stage: r.stage.as_str().to_owned(),
                });
            }
        }
    }
    out
}

/// Shadow records the run wrote, counted from hk-store's durable log on disk rather than believed
/// from a counter (ADR-0016 §6: `<data dir>/ml/shadow/`).
fn shadow_records(dir: &Path) -> u64 {
    ShadowStore::open(shadow_dir(dir))
        .map(|s| s.stats().records)
        .unwrap_or(0)
}

fn snapshot(dir: &Path, meta: &Path) -> MlGateSnapshot {
    let (repo, lost_samples, stage) = run(dir, meta);
    let mut snap = stage.gate_snapshot(ml_rows(&repo), lost_samples);
    snap.shadow_records = shadow_records(dir);
    snap
}

/// **The gate row itself** (ADR-0016 §7, "all through the mock SDR").
///
/// A real run, and then the row's three clauses over what it produced. The printed line names what
/// was exercised and what was not, so a green ML row is never read as evidence that ML ran.
#[test]
fn m3_ml_the_exit_gate_row_holds_for_a_run_through_the_device() {
    let meta = sensor_scene!();
    let dir = TempDir::new("m3ml");
    let snap = snapshot(&dir.0, &meta);
    eprintln!("[{T206}] {}", snap.summary());

    let violations = snap.check();
    assert!(
        violations.is_empty(),
        "[{T206}] ADR-0016 §7's ML row is violated by a run through the mock SDR:\n  {}\n\n\
         {}\n\n\
         Each of these is a clause of the §7 exit-gate table. A classification attributed to a \
         model that is not `active` means a shadow (or unloaded) model changed a Classification \
         row; an `active` mode without §4.6 enable evidence means a family was enabled on no \
         evidence; lost samples with ML on means inference reached the capture path.",
        violations
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n  "),
        snap.summary(),
    );
}

/// **Non-vacuity, over this run's own data** (the T-287/T-297 pattern): construct each state the
/// §7 row forbids, on top of the snapshot the run really produced, and show the gate catching it.
///
/// Without this, the test above would be indistinguishable from an assertion that cannot fail —
/// which is the defect T-366 exists to close, not to re-commit in a new file.
#[test]
fn m3_ml_exit_gate_catches_the_states_the_adr_row_forbids() {
    let meta = sensor_scene!();
    let dir = TempDir::new("m3mlneg");
    let base = snapshot(&dir.0, &meta);
    assert!(base.check().is_empty(), "{}", base.summary());

    let shadow_model = ModeInForce {
        model: "amc-fsk@1.0.0".into(),
        consumer: "hk-classify/dl".into(),
        mode: MlMode::Shadow,
        forced: false,
        enable_evidence: None,
    };

    // Clause 1: a model in shadow whose prediction reached a Classification row.
    let mut wrote_from_shadow = base.clone();
    wrote_from_shadow.modes.push(shadow_model.clone());
    wrote_from_shadow.rows.push(MlAttributedRow {
        subject: "emitter-from-this-run".into(),
        model: Some("amc-fsk@1.0.0#1a2b3c4d".into()),
        stage: "dl".into(),
    });
    let v = wrote_from_shadow.check();
    assert!(
        v.iter()
            .any(|v| matches!(v, GateViolation::ClassificationFromANonActiveModel { .. })),
        "[{T206}] a shadow model writing a classification must fail the gate: {v:?}"
    );

    // Clause 2: `active` reached without the §4.6 enable evidence (the forced path, which the
    // host audits rather than refuses).
    let mut forced_active = base.clone();
    forced_active.modes.push(ModeInForce {
        mode: MlMode::Active,
        forced: true,
        ..shadow_model.clone()
    });
    let v = forced_active.check();
    assert!(
        v.iter()
            .any(|v| matches!(v, GateViolation::ActiveWithoutEnableEvidence { .. })),
        "[{T206}] an active model with no enable evidence must fail the gate: {v:?}"
    );

    // Clause 3: sample loss on a run with ML on. The same loss with ML off is not this row's
    // business — the run's own suites assert loss-free capture either way.
    let mut lossy_with_ml = base.clone();
    lossy_with_ml.modes.push(shadow_model);
    lossy_with_ml.lost_samples = 4096;
    let v = lossy_with_ml.check();
    assert!(
        v.iter()
            .any(|v| matches!(v, GateViolation::SamplesLostWithMlOn { .. })),
        "[{T206}] sample loss with ML on must fail the gate: {v:?}"
    );

    let mut lossy_without_ml = base;
    lossy_without_ml.lost_samples = 4096;
    assert!(
        lossy_without_ml.check().is_empty(),
        "[{T206}] this row is about ML, and must not quietly absorb the loss-free-capture \
         assertion the run's own suites own: {:?}",
        lossy_without_ml.check()
    );
}
