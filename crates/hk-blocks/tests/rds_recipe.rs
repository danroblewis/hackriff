//! T-085: the worked RDS recipe validates against the pinned M1 block catalogue (every block,
//! parameter and port type checked), agrees with the existing Rust RDS decoder's constants (the
//! tutorial's oracle), and hot-edits as ADR-0011 §2.3 says.

use hk_blocks::catalogue;
use hk_demod::rds::{Offset, RDS_BITRATE_BD, block::CHECK_POLY};
use hk_recipe::{EditPlan, NodeChange, PortType, Recipe, parse_hex};
use serde_json::json;

fn load() -> Recipe {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../recipes/rds.recipe.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn rds_recipe_validates_against_the_pinned_catalogue() {
    let r = load();
    let resolved = r.validate(&catalogue::planned()).unwrap_or_else(|e| {
        panic!("RDS recipe invalid: {e:#?}");
    });
    assert!(resolved.warnings.is_empty(), "{:?}", resolved.warnings);
    let into = |node: &str| resolved.edges.iter().find(|e| e.node == node).unwrap().ty;
    use PortType::*;
    for (node, ty) in [
        ("fm", Iq),
        ("rds57", Real),
        ("clock", Iq),
        ("slice", Soft),
        ("diff", Bits),
        ("sync", Bits),
        ("crc", Frames),
        ("group", Frames),
        ("ps", Frames),
        ("rt", Frames),
    ] {
        assert_eq!(into(node), ty, "{node}");
    }
}

#[test]
fn rds_recipe_constants_match_the_rust_rds_oracle() {
    let r = load();
    let node = |id: &str| &r.nodes.iter().find(|n| n.id == id).unwrap().params;
    let hex = |v: &serde_json::Value| parse_hex(v.as_str().unwrap()).unwrap();

    let sync = node("sync");
    assert_eq!(hex(&sync["poly"]), u64::from(CHECK_POLY));
    let words: Vec<(String, u64)> = sync["offsets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| (o["name"].as_str().unwrap().to_owned(), hex(&o["word"])))
        .collect();
    let oracle = [
        ("A", Offset::A),
        ("B", Offset::B),
        ("C", Offset::C),
        ("C'", Offset::CPrime),
        ("D", Offset::D),
    ];
    for (name, off) in oracle {
        let w = words.iter().find(|(n, _)| n == name).unwrap().1;
        assert_eq!(w, u64::from(off.word()), "offset {name}");
    }
    let crc = node("crc");
    assert_eq!(hex(&crc["poly"]), u64::from(CHECK_POLY));
    let per_block = crc["blocks"]["offsets"].as_array().unwrap();
    for (slot, words) in per_block.iter().enumerate() {
        for w in words.as_array().unwrap() {
            let off = Offset::ALL
                .iter()
                .find(|o| u64::from(o.word()) == hex(w))
                .unwrap();
            assert!(off.fits_slot(slot), "offset word in block {slot}");
        }
    }

    assert_eq!(
        node("clock")["symbol_rate_bd"].as_f64().unwrap(),
        RDS_BITRATE_BD
    );
    let sc = node("rds57");
    assert_eq!(
        sc["carrier_hz"].as_f64().unwrap(),
        sc["reference"]["pilot_hz"].as_f64().unwrap()
            * sc["reference"]["multiple"].as_f64().unwrap()
    );
}

#[test]
fn rds_hot_edits_keep_or_reset_state_per_the_block_schemas() {
    let old = load();
    let cat = catalogue::planned();
    let hot = &cat;

    // A slicer threshold is hot: applied in place, nothing downstream resets.
    let mut tweak = old.clone();
    let slice = tweak.nodes.iter_mut().find(|n| n.id == "slice").unwrap();
    slice.params.insert("threshold".into(), json!(0.05));
    let plan = EditPlan::between(&old, &tweak, hot);
    assert!(matches!(
        plan.nodes["slice"],
        NodeChange::Params { hot: true, .. }
    ));
    assert!(plan.reset.is_empty());

    // Sync lock depth is cold: sync is rebuilt; crc, group, ps and rt reset; demod state stays.
    let mut cold = old.clone();
    let sync = cold.nodes.iter_mut().find(|n| n.id == "sync").unwrap();
    sync.params.insert("lock_blocks".into(), json!(3));
    let plan = EditPlan::between(&old, &cold, hot);
    assert!(matches!(
        plan.nodes["sync"],
        NodeChange::Params { hot: false, .. }
    ));
    let reset: Vec<&str> = plan.reset.iter().map(String::as_str).collect();
    assert_eq!(reset, ["crc", "group", "ps", "rt"]);
    assert_eq!(plan.nodes["clock"], NodeChange::Unchanged);

    // A field-map edit is a hot `map` change on `group` only: no DSP state touched, and the
    // text assemblers downstream keep their partial strings.
    let mut remap = old.clone();
    remap.field_maps.get_mut("rds_group").unwrap().fields[4].label = Some("PTY".into());
    let plan = EditPlan::between(&old, &remap, hot);
    assert!(plan.reset.is_empty());
    assert_eq!(
        plan.nodes["group"],
        NodeChange::Params {
            keys: vec!["map".into()],
            hot: true
        }
    );
    assert!(
        plan.nodes
            .iter()
            .all(|(id, c)| id == "group" || *c == NodeChange::Unchanged)
    );
    assert_eq!(plan.field_maps_changed.len(), 1);
}
