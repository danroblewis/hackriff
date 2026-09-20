//! T-085: the worked RDS recipe (`recipes/rds.recipe.json`, ADR-0011 §5) parses into the
//! recipe types, passes structural validation (field map included) and round-trips.

use hk_recipe::{FieldMap, FieldType, Length, OutputKind, PortType, Recipe};
use hk_stream::inspector::{FitStatus, LayerTree};
use serde_json::{Value, json};

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
            "consensus",
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

// T-279: group-4A Clock-Time (CT). There is no captured RDS fixture with a real 4A group on
// hand, so the group bits below are constructed by hand from EN 50067 §3.1.5.6's field widths
// (MJD 17 bits, UTC hour 5, UTC minute 6, local-offset sign 1, local-offset half-hours 5, then
// 3 spare bits — 34 of the 37 group-specific bits used, matching the `ps`/`radiotext` layers'
// `offset: 27, length: 37`) and packed the same way `field_eval.rs`'s tests do.

/// Packs bits (0/1 bytes) MSB-first into octets.
fn pack(bits: &[u8]) -> Vec<u8> {
    bits.chunks(8)
        .map(|c| {
            c.iter()
                .enumerate()
                .fold(0u8, |a, (k, b)| a | (b << (7 - k)))
        })
        .collect()
}

fn push_msb(bits: &mut Vec<u8>, value: u64, width: u32) {
    bits.extend((0..width).rev().map(|k| ((value >> k) & 1) as u8));
}

fn node_value(t: &LayerTree, path: &str) -> Option<Value> {
    t.node(path)?.value.clone()
}

/// One 64-bit RDS group 4A (Clock-Time), as the four 16-bit information words the `rds_group`
/// field map expects after the check words are stripped.
#[allow(clippy::too_many_arguments)]
fn group_4a(
    pi: u16,
    tp: bool,
    pty: u8,
    mjd: u32,
    hour: u8,
    minute: u8,
    offset_sign: u8,
    offset_half_hours: u8,
) -> Vec<u8> {
    let mut bits = Vec::with_capacity(64);
    push_msb(&mut bits, u64::from(pi), 16);
    push_msb(&mut bits, 4, 4); // group_type = 4
    push_msb(&mut bits, 0, 1); // version = A (4A carries the clock; 4B does not)
    push_msb(&mut bits, u64::from(tp), 1);
    push_msb(&mut bits, u64::from(pty), 5);
    push_msb(&mut bits, u64::from(mjd), 17);
    push_msb(&mut bits, u64::from(hour), 5);
    push_msb(&mut bits, u64::from(minute), 6);
    push_msb(&mut bits, u64::from(offset_sign), 1);
    push_msb(&mut bits, u64::from(offset_half_hours), 5);
    push_msb(&mut bits, 0, 3); // spare
    assert_eq!(bits.len(), 64);
    pack(&bits)
}

#[test]
fn rds_ct_decodes_a_known_group_to_the_reported_utc_instant() {
    let (_, r) = load();
    let map = &r.field_maps["rds_group"];

    // 2024-01-01T13:45:00Z, MJD 60310 (cross-checked: (60310 − 40587) × 86400 + 13×3600 +
    // 45×60 = 1704116700, the well-known Unix timestamp for that instant), reported with a
    // negative local-time offset (sign bit set) of 5 hours (10 half-hours) — the offset does
    // not feed the UTC instant (RDS CT's Hour/Minute are already UTC), but must still decode.
    let bytes = group_4a(0xBEEF, true, 10, 60310, 13, 45, 1, 10);
    let tree = map.evaluate(&bytes, 64).unwrap();
    assert_eq!(tree.fit, FitStatus::Ok, "{:?}", tree.errors);

    assert_eq!(node_value(&tree, "ct.mjd"), Some(json!(60310)));
    assert_eq!(node_value(&tree, "ct.hour"), Some(json!(13)));
    assert_eq!(node_value(&tree, "ct.minute"), Some(json!(45)));
    assert_eq!(node_value(&tree, "ct.offset_sign"), Some(json!(1)));
    assert_eq!(
        tree.node("ct.offset_sign").unwrap().text.as_deref(),
        Some("-")
    );
    assert_eq!(node_value(&tree, "ct.offset_half_hours"), Some(json!(10)));

    // The derived absolute instant: seconds since the Unix epoch, UTC, declaring its unit.
    assert_eq!(node_value(&tree, "ct.utc"), Some(json!(1704116700.0)));
    assert_eq!(
        tree.node("ct.utc").unwrap().text.as_deref(),
        Some("1704116700 unix-s")
    );

    // A group of any other type carries no `ct` fields at all (never a wrong/default time).
    let ps_bits = {
        let mut bits = Vec::with_capacity(64);
        push_msb(&mut bits, 0xBEEF, 16);
        push_msb(&mut bits, 0, 4); // group_type = 0 (PS)
        push_msb(&mut bits, 0, 1);
        push_msb(&mut bits, 1, 1);
        push_msb(&mut bits, 10, 5);
        push_msb(&mut bits, 0, 37);
        pack(&bits)
    };
    let ps_tree = map.evaluate(&ps_bits, 64).unwrap();
    assert_eq!(ps_tree.fit, FitStatus::Ok, "{:?}", ps_tree.errors);
    assert!(node_value(&ps_tree, "ct.utc").is_none());
    assert!(node_value(&ps_tree, "ct.mjd").is_none());
}

#[test]
fn rds_ct_rejects_an_out_of_range_time_rather_than_a_nonsense_date() {
    let (_, r) = load();
    let map = &r.field_maps["rds_group"];

    // Hour 29 is not a valid 0–23 UTC hour (the 5-bit field can carry it, but EN 50067 reserves
    // it), so the derived instant must not appear at all — "not decoded", never a wrong or
    // default date. The raw hour is still exposed (useful while debugging a broken transmitter)
    // and the frame's fit stays `Ok`: an out-of-range component is absence, not a fit error, the
    // same rule a false `condition` already gives every other field (T-207/T-164).
    let bad_hour = group_4a(0xBEEF, false, 0, 60310, 29, 0, 0, 0);
    let tree = map.evaluate(&bad_hour, 64).unwrap();
    assert_eq!(tree.fit, FitStatus::Ok, "{:?}", tree.errors);
    assert!(node_value(&tree, "ct.utc").is_none());
    assert_eq!(node_value(&tree, "ct.hour"), Some(json!(29)));
    assert_eq!(node_value(&tree, "ct.mjd"), Some(json!(60310)));

    // Minute 61 is likewise out of the valid 0–59 range.
    let bad_minute = group_4a(0xBEEF, false, 0, 60310, 12, 61, 0, 0);
    let tree = map.evaluate(&bad_minute, 64).unwrap();
    assert_eq!(tree.fit, FitStatus::Ok, "{:?}", tree.errors);
    assert!(node_value(&tree, "ct.utc").is_none());
    assert_eq!(node_value(&tree, "ct.minute"), Some(json!(61)));
}
