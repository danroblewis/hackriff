//! T-085: the worked RDS recipe (`recipes/rds.recipe.json`, ADR-0011 §5) parses into the
//! recipe types, passes structural validation (field map included) and round-trips.

use hk_recipe::{FieldMap, FieldType, Length, OutputKind, PortType, Recipe};

fn load() -> (String, Recipe) {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../recipes/rds.recipe.json");
    let text = std::fs::read_to_string(path).expect("recipes/rds.recipe.json");
    let recipe =
        serde_json::from_str(&text).expect("RDS recipe parses (unknown fields are errors)");
    (text, recipe)
}

#[test]
fn rds_recipe_parses_and_validates_structurally() {
    let (_, r) = load();
    r.validate_structure().unwrap();
    assert_eq!((r.id.as_str(), r.version), ("rds", 1));
    assert_eq!(r.input.port, PortType::Iq);
    let blocks: Vec<&str> = r.nodes.iter().map(|n| n.block.as_str()).collect();
    assert_eq!(
        blocks,
        [
            "fm_demod",
            "subcarrier",
            "clock_recovery",
            "slicer",
            "diff_decode",
            "sync_search",
            "crc",
            "fields",
            "text",
            "text"
        ]
    );
    assert!(r.outputs.iter().any(|o| o.kind == OutputKind::Inspector));
    assert!(r.output_policy.content_class.permits_content());
}

#[test]
fn rds_group_field_map_lays_out_pi_pty_ps_and_radiotext() {
    let (_, r) = load();
    let map = &r.field_maps["rds_group"];
    map.validate().unwrap();
    let f = |name: &str| map.fields.iter().find(|f| f.name == name).unwrap();
    assert_eq!(f("pi").length, Some(Length::Fixed(16)));
    assert_eq!(f("pty").length, Some(Length::Fixed(5)));
    for layer in ["ps", "radiotext"] {
        assert_eq!(f(layer).ty, FieldType::Layer);
        assert_eq!(f(layer).offset, Some(27));
        assert_eq!(f(layer).length, Some(Length::Fixed(37)));
    }
    // 16 + 4 + 1 + 1 + 5 = 27 bits of header before the group-specific layers.
    let header: u32 = ["pi", "group_type", "version", "tp", "pty"]
        .iter()
        .map(|n| match f(n).length {
            Some(Length::Fixed(b)) => b,
            _ => unreachable!(),
        })
        .sum();
    assert_eq!(header, 27);
}

#[test]
fn rds_recipe_and_field_map_round_trip() {
    let (text, r) = load();
    let again: Recipe = serde_json::from_str(&serde_json::to_string_pretty(&r).unwrap()).unwrap();
    assert_eq!(again, r);
    // The file is in canonical form up to formatting: re-serialising loses nothing.
    let file: serde_json::Value = serde_json::from_str(&text).unwrap();
    let ours: serde_json::Value = serde_json::to_value(&r).unwrap();
    assert_eq!(normalise(file), normalise(ours));

    let map = &r.field_maps["rds_group"];
    let back: FieldMap = serde_json::from_value(serde_json::to_value(map).unwrap()).unwrap();
    assert_eq!(&back, map);
}

/// JSON numbers compare by value (`240000` vs `240000.0`).
fn normalise(v: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match v {
        Value::Number(n) => serde_json::json!(n.as_f64().unwrap()),
        Value::Array(a) => Value::Array(a.into_iter().map(normalise).collect()),
        Value::Object(o) => Value::Object(o.into_iter().map(|(k, v)| (k, normalise(v))).collect()),
        other => other,
    }
}
