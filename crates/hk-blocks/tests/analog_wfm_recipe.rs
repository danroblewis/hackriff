//! T-867 (LP-3, ADR-0015 §12.9): `recipes/analog-wfm.recipe.json` validates against the block
//! catalogue: an `audio` output from `audio_out` plus the RDS sibling outputs off the same FM stage.

use hk_blocks::catalogue;
use hk_recipe::Recipe;

#[test]
fn analog_wfm_recipe_has_audio_and_rds_siblings() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../recipes/analog-wfm.recipe.json"
    );
    let r: Recipe = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let resolved = r
        .validate(&catalogue::planned())
        .unwrap_or_else(|e| panic!("analog-wfm invalid: {e:#?}"));
    assert!(resolved.warnings.is_empty(), "{:?}", resolved.warnings);
    let ids: Vec<&str> = r.outputs.iter().map(|o| o.id.as_str()).collect();
    for want in ["audio", "station", "radiotext", "groups"] {
        assert!(ids.contains(&want), "{want} in {ids:?}");
    }
    let from_fm = |n: &str| resolved.edges.iter().filter(|e| e.node == n).count();
    assert_eq!(from_fm("sq"), 1);
    assert_eq!(from_fm("rds57"), 1);
}
