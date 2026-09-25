//! T-884 item 1 — **the configured `ConfirmPolicy.synthesized` reaches the MAUTO attach step.**
//!
//! Found by the T-860 Opus review: `hk_cli::pipeline::serve_api` built its `RepoAttacher` with
//! `SynthesizedConfirm::default()`, so the `ConfirmPolicy.synthesized` field a run is configured
//! with was never read by anything. The rule it configures gates an **irreversible** transition
//! (candidate → confirmed, undone only by a user delete), so a configuration that silently does
//! not apply is the worst possible place for one.
//!
//! The seam is `PipelineHandle::synthesized_confirm()`: the run reads the clause off its inventory
//! once at start and carries it as a value, and the CLI hands *that* to the attacher. This test
//! drives the seam through a real run — a configured inventory in, the configured clause out —
//! because the defect was precisely that the two ends were never connected.

mod common;

use std::time::Duration;

use common::*;
use hk_core::Pacing;
use hk_pipeline::inventory::SynthesizedConfirm;
use hk_pipeline::{ConfirmPolicy, Pipeline, TrackInventory};

/// A rule tightened everywhere it can be: nothing here is a default, so a default answer is
/// visibly wrong rather than accidentally right.
fn tightened() -> SynthesizedConfirm {
    SynthesizedConfirm {
        enabled: true,
        min_analytic_holdout_bits: 48.0,
        hard_check_floor_bits: 32.0,
        min_check_width: 16,
        assumed_decisions_per_week: 5_000,
        require_null_control_when_searched: true,
        max_suspect_detection_fraction: 0.1,
        forbid_overload_in_window: true,
    }
}

#[test]
fn t884_the_runs_configured_synthesized_confirm_rule_reaches_the_handle() {
    let dir = TempDir::new("t884-confirm-config");
    let meta = tone_recording(&dir.0, "tone", 2e6, 0.2, 433.9e6, None);
    let (cfg, replay) = replay_config(&dir.0, &meta, serde_json::json!({}), Pacing::Unpaced);
    let policy = ConfirmPolicy {
        synthesized: tightened(),
        ..ConfirmPolicy::default()
    };
    let handle = Pipeline::start(
        cfg,
        Box::new(replay.source),
        replay.info,
        None,
        Box::new(TrackInventory::with_policy(policy)),
    )
    .unwrap();
    // Read before the run is stopped: this is the read `serve_api` makes, while the run is live.
    let served = handle.synthesized_confirm();
    assert_eq!(
        served,
        tightened(),
        "the attacher would confirm under a rule nobody configured"
    );
    assert_ne!(
        served,
        SynthesizedConfirm::default(),
        "the control: the configured rule is not the default one"
    );
    let (_summary, _) = wait_guarded(handle, Duration::from_secs(60));
}

/// An inventory with no policy of its own is no weaker than the shipped rule: the trait's default
/// is the default policy's clause, never a permissive stand-in.
#[test]
fn t884_an_inventory_with_no_policy_answers_the_shipped_rule() {
    use hk_pipeline::inventory::{Inventory, NullInventory};
    assert_eq!(
        NullInventory.synthesized_confirm(),
        ConfirmPolicy::default().synthesized
    );
    assert_eq!(
        TrackInventory::default().synthesized_confirm(),
        ConfirmPolicy::default().synthesized
    );
}
