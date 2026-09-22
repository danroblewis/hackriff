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
//! # The honest complication, and what is done about it
//!
//! The model host is **dormant by design** (T-363, `hk-ml/src/lib.rs`): nothing constructs it, no
//! family clears ADR-0016 §4.6 for even `shadow`, and no durable sink exists to drain. So two of
//! the row's three clauses are trivially true today and the third has no antecedent — a test
//! written naively here would be exactly as vacuous as the row it replaces.
//!
//! What this file asserts instead is a **predicate over what the run actually produced**
//! ([`hk_ml::exit_gate`]): every ML-attributed `Classification` row the run persisted must name a
//! model that is `active` with §4.6 enable evidence behind it. Today the run persists no such row
//! and no mode is in force, so the predicate holds — *measured*, not assumed. The day a model
//! writes a classification from shadow, or reaches `active` by force with no evidence, this gate
//! goes red. That is the property T-366 asked for: a check that fails the day ML becomes active
//! without its evidence, rather than one that passes because nothing is on.
//!
//! **Non-vacuity is shown, not argued** (the T-287/T-297 pattern):
//! [`m3_ml_exit_gate_catches_the_states_the_adr_row_forbids`] takes *this run's own snapshot* and
//! constructs each forbidden state on it, asserting the gate reports each one. The same proof is
//! run against a live host inside `hk-ml` (`host.rs`,
//! `a_forced_active_model_without_evidence_fails_the_adr_0016_s7_exit_gate`), where the one path
//! that can reach `active` without evidence — a forced `set_mode` — is exercised for real.
//!
//! # Where the "no mode is in force" half comes from
//!
//! It is not this file's opinion. No mode can be in force because nothing in the workspace
//! constructs the host, and that is guarded by `crates/hk-ml/tests/no_production_caller.rs`, whose
//! failure message tells whoever wires it to come back and supply the enumeration. This file
//! asserts that guard still stands ([`m3_ml_the_dormant_premise_of_this_gate_is_still_guarded`]),
//! so the premise cannot rot silently: delete the guard without giving this gate a real mode
//! enumeration and the M3 acceptance suite fails.

use std::path::{Path, PathBuf};

use hk_core::Pacing;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_ml::MlMode;
use hk_ml::exit_gate::{GateViolation, MlAttributedRow, MlGateSnapshot, ModeInForce};
use hk_model::classify::Stage;
use hk_model::{InventoryQuery, Repository};
use serde_json::json;

use crate::blind::{replay_config, start};
use crate::common::*;

const T206: &str = "T-206";

/// The guard whose existence is this gate's licence to report an empty mode table (T-363).
const DORMANCY_GUARD: &str = "crates/hk-ml/tests/no_production_caller.rs";
const DORMANCY_GUARD_ASSERTION: &str = "the_model_host_still_has_no_caller_outside_this_crate";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("tests/e2e sits two levels under the workspace root")
        .to_path_buf()
}

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

/// Runs a scene through the mock SDR and returns its store and the samples the ring lost.
fn run(dir: &Path, meta: &Path) -> (Repository, u64) {
    let (cfg, replay) = replay_config(dir, meta, json!({}), Pacing::Unpaced);
    let summary = finish(start(cfg, replay));
    (repo(dir), summary.always_on_lost_samples)
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

/// Shadow records the run wrote, counted from the store rather than believed.
///
/// ADR-0016 §6 puts them in `<store>/ml/shadow/`; T-365 owns building that. Until it exists the
/// directory is absent and the count is 0 — and the day it appears with records in it, they are
/// counted here, which is what turns clause 3's antecedent on.
fn shadow_records(dir: &Path) -> u64 {
    let d = dir.join("ml").join("shadow");
    let Ok(entries) = std::fs::read_dir(&d) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| e.path().is_file())
        .map(|e| {
            std::fs::read_to_string(e.path())
                .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count() as u64)
                .unwrap_or(0)
        })
        .sum()
}

/// The `(model, consumer)` modes in force during the run.
///
/// Empty, and derived rather than declared: nothing in the workspace constructs the host, so no
/// mode can exist — see [`m3_ml_the_dormant_premise_of_this_gate_is_still_guarded`], which fails
/// if that stops being true. When the host is wired this becomes
/// `MlGateSnapshot::from_host(&host, rows, lost)` and every clause starts biting on real modes.
fn modes_in_force() -> Vec<ModeInForce> {
    Vec::new()
}

fn snapshot(dir: &Path, meta: &Path) -> MlGateSnapshot {
    let (repo, lost_samples) = run(dir, meta);
    MlGateSnapshot {
        modes: modes_in_force(),
        rows: ml_rows(&repo),
        shadow_records: shadow_records(dir),
        lost_samples,
    }
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

/// **The premise this gate reports an empty mode table on is still guarded** (T-363).
///
/// The gate above reads "no `(model, consumer)` mode was in force" from the fact that nothing in
/// the workspace constructs the model host. That fact is guarded by `DORMANCY_GUARD`, which fails
/// the day a caller appears. If that guard is deleted — which is exactly what the wiring change is
/// told to do — this assertion fires, and whoever wired the host must give
/// [`modes_in_force`] a real enumeration (`MlGateSnapshot::from_host`) before M3 can pass its own
/// exit gate again.
#[test]
fn m3_ml_the_dormant_premise_of_this_gate_is_still_guarded() {
    let guard = workspace_root().join(DORMANCY_GUARD);
    let text = std::fs::read_to_string(&guard).unwrap_or_default();
    assert!(
        text.contains(DORMANCY_GUARD_ASSERTION),
        "[{T206}] {DORMANCY_GUARD} no longer asserts `{DORMANCY_GUARD_ASSERTION}`, so the model \
         host may now have a production caller — and ADR-0016 §7's ML row is being measured here \
         with an empty mode table, which would make it vacuous again.\n\
         Supply the real enumeration in `m3_ml.rs::modes_in_force` (from the wired host, via \
         `MlGateSnapshot::from_host`) in the same change that removes the guard."
    );
}
