//! T-873 (ADR-0015 §12.13 LP-9): a stereo FM chain is expressible in a recipe against the
//! catalogue — `fm_demod` (the whole multiplex, no de-emphasis) → `stereo_decode` → one
//! `deemphasis` per channel, wired by `st.left` / `st.right`. Schema 3 allows one mono `audio`
//! output per recipe, so the right channel leaves as a stage output here: carrying both channels
//! in one stream is LP-10's wire change. This pins only that the block's two ports are
//! addressable and typed.

use hk_blocks::catalogue;
use hk_recipe::{PortType, Recipe};
use serde_json::json;

#[test]
fn a_stereo_chain_wires_left_and_right_through_their_own_deemphasis() {
    let r: Recipe = serde_json::from_value(json!({
        "schema": "hackriff.recipe", "schema_version": 3, "id": "t873-stereo", "version": 1,
        "name": "T-873 WFM stereo",
        "input": {"port": "iq", "sample_rate_hz": 240000, "bandwidth_hz": 200000},
        "nodes": [
            {"id": "fm", "block": "fm_demod", "params": {"deviation_hz": 75000}},
            {"id": "st", "block": "stereo_decode"},
            {"id": "de_l", "block": "deemphasis", "params": {"tau_s": 50e-6},
             "inputs": {"in": "st.left"}},
            {"id": "out_l", "block": "audio_out"},
            {"id": "de_r", "block": "deemphasis", "params": {"tau_s": 50e-6},
             "inputs": {"in": "st.right"}}
        ],
        "outputs": [
            {"id": "left", "kind": "audio", "from": "out_l", "channels": "mono"},
            {"id": "right", "kind": "stage", "from": "de_r"}
        ],
        "output_policy": {"content_class": "unrestricted"}
    }))
    .expect("parses");
    let resolved = r
        .validate(&catalogue::planned())
        .unwrap_or_else(|e| panic!("stereo recipe invalid: {e:#?}"));
    assert!(resolved.warnings.is_empty(), "{:?}", resolved.warnings);
    for node in ["st", "de_l", "de_r"] {
        let e = resolved.edges.iter().find(|e| e.node == node).unwrap();
        assert_eq!(e.ty, PortType::Real, "{node}");
    }
}
