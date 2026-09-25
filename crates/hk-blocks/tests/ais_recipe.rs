//! T-963 (SIGNAL-015): the AIS recipe (`recipes/ais.recipe.json`) validates against the pinned
//! block catalogue and carries the ITU-R M.1371 conventions: GMSK 9600 Bd (deviation = rate/4),
//! NRZI (0 = transition), standard HDLC framing on 0x7E flags with zero-bit destuffing, FCS =
//! CRC-16/X-25 (a.k.a. CRC-16/IBM-SDLC: poly 0x1021, init 0xFFFF, refin/refout, xorout 0xFFFF)
//! over everything but the closing flag octet, and the two fixed international marine channels
//! followed together (not hop-discovered — the AIS channel plan is fixed by treaty, unlike a
//! pager net's unknown channel count). The bit-level HDLC/CRC chain is checked with the real
//! blocks in `crates/hk-blocks/src/blocks/framing/tests.rs::ais_hdlc_destuff_sync_and_crc16_x25`.

use hk_blocks::catalogue;
use hk_recipe::{ChannelsSpec, PortType, Recipe, parse_hex};

fn load() -> Recipe {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../recipes/ais.recipe.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap())
        .unwrap_or_else(|e| panic!("ais recipe parses: {e}"))
}

#[test]
fn ais_recipe_validates_against_the_pinned_catalogue() {
    let r = load();
    let resolved = r
        .validate(&catalogue::planned())
        .unwrap_or_else(|e| panic!("AIS recipe invalid: {e:#?}"));
    assert!(resolved.warnings.is_empty(), "{:?}", resolved.warnings);
    let into = |node: &str| resolved.edges.iter().find(|e| e.node == node).unwrap().ty;
    use PortType::*;
    for (node, ty) in [
        ("msk", Iq),
        ("clock", Real),
        ("slice", Soft),
        ("line", Bits),
        ("destuff", Bits),
        ("sync", Bits),
        ("crc", Frames),
        ("hops", Frames),
        ("msg", Frames),
    ] {
        assert_eq!(into(node), ty, "{node}");
    }
}

#[test]
fn ais_recipe_follows_both_fixed_marine_channels_not_a_hop_search() {
    let r = load();
    match &r.input.channels {
        ChannelsSpec::FollowHops { list_hz, .. } => {
            assert_eq!(
                *list_hz,
                vec![161_975_000.0, 162_025_000.0],
                "AIS1/Ch87B and AIS2/Ch88B"
            );
        }
        other => panic!("expected follow-hops with a fixed list_hz, got {other:?}"),
    }
}

#[test]
fn ais_recipe_uses_gmsk_9600_nrzi_hdlc_and_crc16_x25() {
    let r = load();
    let node = |id: &str| &r.nodes.iter().find(|n| n.id == id).unwrap().params;

    let msk = node("msk");
    assert_eq!(msk["symbol_rate_bd"].as_f64(), Some(9600.0));

    let clock = node("clock");
    assert_eq!(clock["symbol_rate_bd"].as_f64(), Some(9600.0));

    let line = node("line");
    assert_eq!(line["mode"], "transition-is-0", "NRZI: 0 = transition");
    assert_eq!(line["direction"], "decode");

    let destuff = node("destuff");
    assert_eq!(destuff["direction"], "destuff");
    assert_eq!(destuff["stuff_after"].as_u64(), Some(5));
    assert_eq!(destuff["abort_ones"].as_u64(), Some(7));

    let sync = node("sync");
    assert_eq!(parse_hex(sync["sync_word"].as_str().unwrap()), Some(0x7E));
    assert_eq!(sync["sync_bits"].as_u64(), Some(8));
    assert_eq!(
        parse_hex(sync["terminator"]["words"][0].as_str().unwrap()),
        Some(0x7E)
    );
    assert_eq!(sync["terminator"]["trailer_bits"].as_u64(), Some(0));

    let crc = node("crc");
    assert_eq!(crc["width"].as_u64(), Some(16));
    assert_eq!(parse_hex(crc["poly"].as_str().unwrap()), Some(0x1021));
    assert_eq!(parse_hex(crc["init"].as_str().unwrap()), Some(0xFFFF));
    assert_eq!(crc["refin"], serde_json::json!(true));
    assert_eq!(crc["refout"], serde_json::json!(true));
    assert_eq!(parse_hex(crc["xorout"].as_str().unwrap()), Some(0xFFFF));
    assert_eq!(crc["strip"], serde_json::json!(true));
    // `sync_search`'s terminator match (the closing flag) is part of the frame it emits (the
    // ACARS convention); `end_trim_bits` excludes that octet from the FCS itself.
    assert_eq!(crc["span"]["end_trim_bits"].as_u64(), Some(8));
}
