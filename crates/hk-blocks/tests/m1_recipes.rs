//! T-085 review: the POCSAG, ACARS and ADS-B recipe skeletons (`recipes/*.recipe.json`,
//! ADR-0011 §5.1) validate against the pinned M1 catalogue, so the framing, field-map and
//! message-assembly contracts can express all four tutorials. Placeholder blocks (params not
//! pinned yet) may warn; nothing may error.

use hk_blocks::catalogue;
use hk_recipe::{Catalogue, ChannelsSpec, PortType, Recipe, Resolved, parse_hex};

fn load(name: &str) -> Recipe {
    let path = format!(
        "{}/../../recipes/{name}.recipe.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap())
        .unwrap_or_else(|e| panic!("{name} recipe parses: {e}"))
}

fn validate(r: &Recipe) -> Resolved {
    let cat = catalogue::planned();
    let resolved = r
        .validate(&cat)
        .unwrap_or_else(|e| panic!("{} recipe invalid: {e:#?}", r.id));
    for w in &resolved.warnings {
        let i: usize = w
            .path
            .strip_prefix("nodes[")
            .and_then(|p| p.split(']').next())
            .and_then(|i| i.parse().ok())
            .unwrap_or_else(|| panic!("{}: unexpected warning {w:?}", r.id));
        let d = cat.descriptor(&r.nodes[i].block).unwrap();
        assert!(!d.params_pinned, "{}: warning on pinned {}", r.id, d.name);
    }
    resolved
}

fn into(resolved: &Resolved, node: &str) -> PortType {
    resolved.edges.iter().find(|e| e.node == node).unwrap().ty
}

fn params<'a>(r: &'a Recipe, id: &str) -> &'a hk_recipe::Params {
    &r.nodes.iter().find(|n| n.id == id).unwrap().params
}

#[test]
fn adsb_ppm_frames_have_df_decided_length_and_crc24() {
    let r = load("adsb");
    let resolved = validate(&r);
    assert!(resolved.warnings.is_empty(), "{:?}", resolved.warnings);
    assert_eq!(into(&resolved, "crc"), PortType::Frames);
    let ppm = params(&r, "ppm");
    assert_eq!(parse_hex(ppm["preamble"].as_str().unwrap()), Some(0xA140));
    assert_eq!(ppm["length_from"]["cases"][0]["frame_bits"], 112);
    assert_eq!(ppm["length_from"]["default_bits"], 56);
    assert_eq!(
        parse_hex(params(&r, "crc")["poly"].as_str().unwrap()),
        Some(0xFFF409)
    );
}

#[test]
fn pocsag_assembles_messages_per_channel_before_following_hops() {
    let r = load("pocsag");
    let resolved = validate(&r);
    assert!(matches!(r.input.channels, ChannelsSpec::FollowHops { .. }));
    assert_eq!(
        parse_hex(params(&r, "sync")["sync_word"].as_str().unwrap()),
        Some(0x7CD2_15D8)
    );
    for node in ["bch", "msg", "hops", "page"] {
        assert_eq!(into(&resolved, node), PortType::Frames, "{node}");
    }
    // Assembly is upstream of the merge, so messages never mix channels.
    let pos = |id: &str| r.nodes.iter().position(|n| n.id == id).unwrap();
    assert!(pos("msg") < pos("hops"));
}

#[test]
fn acars_frames_end_at_etx_and_read_lsb_first_characters() {
    let r = load("acars");
    let resolved = validate(&r);
    assert_eq!(into(&resolved, "crc"), PortType::Frames);
    let sync = params(&r, "sync");
    assert_eq!(sync["bit_order"], "lsb");
    assert_eq!(sync["terminator"]["trailer_bits"], 16);
}
