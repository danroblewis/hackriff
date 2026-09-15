//! T-096/T-108: the worked ACARS recipe validates against the pinned M1 block catalogue (every
//! block, parameter and port type checked) and carries the real ARINC 618 conventions, as
//! acarsdec's receiver fixes them (docs/tutorials/03-acars.md §1): tones integrated to MSK
//! chips, either polarity, LSB-first characters, CRC-16/KERMIT over the parity-bearing
//! characters. The bit-level chain is checked in
//! crates/hk-blocks/src/blocks/framing/tests.rs::acars_terminator_lsb_characters_and_crc16_kermit.

use hk_blocks::catalogue;
use hk_recipe::{PortType, Recipe, parse_hex};
use serde_json::json;

fn load() -> Recipe {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../recipes/acars.recipe.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn acars_recipe_validates_against_the_pinned_catalogue() {
    let r = load();
    let resolved = r.validate(&catalogue::planned()).unwrap_or_else(|e| {
        panic!("ACARS recipe invalid: {e:#?}");
    });
    assert!(resolved.warnings.is_empty(), "{:?}", resolved.warnings);
    let into = |node: &str| resolved.edges.iter().find(|e| e.node == node).unwrap().ty;
    use PortType::*;
    for (node, ty) in [
        ("am", Iq),
        ("tone", Real),
        ("msk", Iq),
        ("clock", Real),
        ("slice", Soft),
        ("chips", Bits),
        ("sync", Bits),
        ("crc", Frames),
        ("msg", Frames),
    ] {
        assert_eq!(into(node), ty, "{node}");
    }
}

/// `+ * SYN SYN SOH` with odd parity, each character LSB first on air.
const PLUS_STAR_SYN_SYN_SOH: u64 = 0xD5_5468_6880;

#[test]
fn acars_recipe_uses_the_arinc_618_conventions() {
    let r = load();
    let node = |id: &str| &r.nodes.iter().find(|n| n.id == id).unwrap().params;
    let hex = |v: &serde_json::Value| parse_hex(v.as_str().unwrap()).unwrap();

    let chips = node("chips");
    assert_eq!(chips["mode"], json!("transition-is-0"));
    assert_eq!(chips["direction"], json!("encode"));

    let sync = node("sync");
    assert_eq!(hex(&sync["sync_word"]), PLUS_STAR_SYN_SYN_SOH);
    assert_eq!(sync["sync_bits"].as_u64().unwrap(), 40);
    assert_eq!(sync["bit_order"], json!("lsb"));
    assert_eq!(sync["polarity"], json!("either"));
    assert_eq!(sync["include_sync"], json!(false));

    let crc = node("crc");
    assert_eq!(hex(&crc["poly"]), 0x1021);
    assert_eq!(crc["refin"], json!(true));
    assert_eq!(crc["refout"], json!(true));
    assert!(
        r.nodes.iter().all(|n| n.block != "parity"),
        "the block check covers the parity bits: no parity rewrite before it"
    );
}
