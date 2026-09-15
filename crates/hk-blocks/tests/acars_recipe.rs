//! T-096: the worked ACARS recipe validates against the pinned M1 block catalogue (every block,
//! parameter and port type checked) and its bit-level constants (sync word, block-check
//! convention) reproduce py/hkpy/synth/acars.py's framing (checked bit-for-bit in
//! crates/hk-blocks/src/blocks/fec/tests.rs::parity_zero_then_crc_matches_pre_parity_check_t096).

use hk_blocks::catalogue;
use hk_recipe::{PortType, Recipe, parse_hex};

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
        ("sync", Bits),
        ("unwrap", Frames),
        ("msg", Frames),
        ("pz", Frames),
        ("crc", Frames),
    ] {
        assert_eq!(into(node), ty, "{node}");
    }
}

/// SYN SYN SOH (0x16 0x16 0x01), the framing preceding the ACARS block (no differential
/// decoding: this recipe drops `diff_decode`, matching T-086's
/// `acars_path_am_subcarrier_msk_recovers_hidden_bits`, which recovers the hidden bits directly
/// off `slicer`).
const SYN_SYN_SOH: u64 = 0x160116;

#[test]
fn acars_recipe_sync_word_and_block_check_match_the_synthetic_generator() {
    let r = load();
    let node = |id: &str| &r.nodes.iter().find(|n| n.id == id).unwrap().params;
    let hex = |v: &serde_json::Value| parse_hex(v.as_str().unwrap()).unwrap();

    assert!(
        r.nodes.iter().all(|n| n.block != "diff_decode"),
        "MSK mark/space maps straight to bit 1/0: no differential decoding step"
    );

    let sync = node("sync");
    assert_eq!(hex(&sync["sync_word"]), SYN_SYN_SOH);
    assert_eq!(sync["sync_bits"].as_u64().unwrap(), 24);
    assert_eq!(sync["include_sync"], serde_json::json!(false));
    assert_ne!(
        sync.get("bit_order"),
        Some(&serde_json::json!("lsb")),
        "the synthetic fixture sends each character MSB (parity) first, not LSB-first"
    );

    // `unwrap` (real parity bits, feeds `fields`) and `crc` (zeroed parity bits, block-check
    // status) both check CRC-16/XMODEM: poly 0x1021, init 0, not reflected.
    for id in ["unwrap", "crc"] {
        let n = node(id);
        assert_eq!(hex(&n["poly"]), 0x1021);
        assert_eq!(n["refin"], serde_json::json!(false));
        assert_eq!(n["refout"], serde_json::json!(false));
    }
    let pz = node("pz");
    assert_eq!(pz["zero"], serde_json::json!(true));
    assert_eq!(pz["position"], serde_json::json!("first"));
    assert_eq!(pz["span"]["end_trim_bits"].as_u64().unwrap(), 16);
}
