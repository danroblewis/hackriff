//! ADR-0015 §1.2: every beam node is a valid, runnable recipe prefix — `Recipe::validate` passes
//! against the real block catalogue and the tail is one `stage` output on the deepest node.
//! This is the invariant M-3's beam builds on, checked against `hk_blocks::Registry::builtin()`
//! rather than a stub catalogue, so a candidate that passes here is one the runtime can start.

use hk_blocks::Registry;
use hk_synth::Stage;
use hk_synth::candidate::{Candidate, CandidateError};

/// An S3 prefix of a generic FSK skeleton: discriminator, clock recovery, slicer.
fn s3_prefix(stage_output_from: &str) -> Candidate {
    serde_json::from_value(serde_json::json!({
        "skeleton": "generic-fsk-framed@1",
        "choices": { "S1": "fsk", "S3": "none" },
        "recipe": {
            "schema": "hackriff.recipe", "schema_version": 2,
            "id": "synth-prefix", "version": 1, "name": "S3 prefix",
            "input": { "port": "iq", "sample_rate_hz": 24000, "bandwidth_hz": 16000 },
            "nodes": [
                { "id": "fsk", "block": "fsk_demod" },
                { "id": "clock", "block": "clock_recovery",
                  "params": { "symbol_rate_bd": 1200, "pulse": "nrz", "algorithm": "gardner" } },
                { "id": "slice", "block": "slicer", "params": { "threshold": 0.0, "invert": false } }
            ],
            "outputs": [ { "id": "tail", "kind": "stage", "from": stage_output_from } ],
            "output_policy": { "content_class": "metadata-only" }
        },
        "free": [
            { "path": "nodes[clock].params.symbol_rate_bd",
              "domain": { "float": { "lo": 1150, "hi": 1250, "scale": "log", "resolution": 0.01 } },
              "seed": 1200, "source": "estimate" },
            { "path": "nodes[sync].params.sync_word", "domain": { "proposal": "assist.sync" } }
        ]
    }))
    .expect("the ADR §1.2 candidate shape parses")
}

#[test]
fn an_s3_prefix_is_a_runnable_recipe() {
    let c = s3_prefix("slice");
    let registry = Registry::builtin();
    c.check_prefix(&registry)
        .unwrap_or_else(|e| panic!("prefix should validate: {e}"));
    assert_eq!(c.deepest_choice(), Some(Stage::S3));
}

#[test]
fn a_stage_output_off_the_tail_is_not_a_prefix() {
    let c = s3_prefix("clock");
    assert_eq!(
        c.check_prefix(&Registry::builtin()),
        Err(CandidateError::TailNotStageOutput)
    );
}

#[test]
fn an_unknown_block_is_refused_by_recipe_validation() {
    let mut c = s3_prefix("slice");
    c.recipe.nodes[0].block = "no_such_block".into();
    assert!(matches!(
        c.check_prefix(&Registry::builtin()),
        Err(CandidateError::Recipe(_))
    ));
}
