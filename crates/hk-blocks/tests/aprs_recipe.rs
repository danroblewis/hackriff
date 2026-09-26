//! T-952: the worked APRS recipe (`recipes/aprs.recipe.json`) validates against the pinned M1
//! block catalogue (every block, parameter and port type checked) and carries the AX.25/Bell 202
//! conventions: the 0x7E flags are found on the still-stuffed line (T-1054, as the AIS recipe
//! does: the flag is unique only there), each frame's closing flag also opens the next
//! (`terminator.reopen`), then T-613's `bitstuff` destuffs each frame with `bit_order: lsb`
//! (every octet sent LSB first), FCS = CRC-16/X-25 over dest..info (`crc`). Before T-1054 the
//! line was destuffed first and framed at 8-bit steps, so a noise-opened frame never saw a flag
//! off its own lattice and swallowed real packets. The bit-level round trip
//! (encode -> destuff -> frame -> FCS-valid) is `crates/hk-blocks/src/blocks/framing/tests.rs`'s
//! `known_aprs_ui_frame_destuffs_frames_and_fcs_validates_blind`.

use hk_blocks::catalogue;
use hk_recipe::{PortType, Recipe, parse_hex};
use serde_json::json;

fn load() -> Recipe {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../recipes/aprs.recipe.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn aprs_recipe_validates_against_the_pinned_catalogue() {
    let r = load();
    let resolved = r.validate(&catalogue::planned()).unwrap_or_else(|e| {
        panic!("APRS recipe invalid: {e:#?}");
    });
    assert!(resolved.warnings.is_empty(), "{:?}", resolved.warnings);
    let into = |node: &str| resolved.edges.iter().find(|e| e.node == node).unwrap().ty;
    use PortType::*;
    for (node, ty) in [
        ("fm", Iq),
        ("tone", Real),
        ("afsk", Iq),
        ("clock", Real),
        ("slice", Soft),
        ("line", Bits),
        ("sync", Bits),
        ("destuff", Frames),
        ("crc", Frames),
        ("frame", Frames),
    ] {
        assert_eq!(into(node), ty, "{node}");
    }
}

#[test]
fn aprs_recipe_uses_the_ax25_conventions() {
    let r = load();
    let node = |id: &str| &r.nodes.iter().find(|n| n.id == id).unwrap().params;
    let hex = |v: &serde_json::Value| parse_hex(v.as_str().unwrap()).unwrap();

    let line = node("line");
    assert_eq!(line["mode"], json!("transition-is-0"));
    assert_eq!(line["direction"], json!("decode"));

    let sync = node("sync");
    assert_eq!(hex(&sync["sync_word"]), 0x7E);
    assert_eq!(sync["sync_bits"].as_u64().unwrap(), 8);
    assert!(
        sync.get("bit_order").is_none(),
        "the stuffed line has no octet boundary"
    );
    assert_eq!(sync["include_sync"], json!(false));
    assert_eq!(hex(&sync["terminator"]["words"][0]), 0x7E);
    assert_eq!(
        sync["terminator"]["step_bits"].as_u64().unwrap(),
        1,
        "every bit: a noise-opened frame must see a flag off its own lattice"
    );
    assert_eq!(
        sync["terminator"]["reopen"],
        json!(true),
        "T-1054: shared HDLC flag"
    );

    let destuff = node("destuff");
    assert_eq!(destuff["direction"], json!("destuff"));
    assert_eq!(
        destuff["bit_order"],
        json!("lsb"),
        "AX.25 octets are sent LSB first"
    );
    let pos = |id: &str| r.nodes.iter().position(|n| n.id == id).unwrap();
    assert!(pos("sync") < pos("destuff") && pos("destuff") < pos("crc"));

    let crc = node("crc");
    assert_eq!(hex(&crc["poly"]), 0x1021);
    assert_eq!(hex(&crc["init"]), 0xFFFF);
    assert_eq!(hex(&crc["xorout"]), 0xFFFF);
    assert_eq!(crc["refin"], json!(true));
    assert_eq!(crc["refout"], json!(true));
    assert_eq!(crc["strip"], json!(true));
    assert_eq!(
        crc["span"]["end_trim_bits"].as_u64().unwrap(),
        8,
        "the closing flag sync_search kept in the frame sits after the FCS"
    );
    assert_eq!(
        crc["drop_invalid"],
        json!(true),
        "the flag is only 8 bits and chance-matches noise often, unlike ACARS's 40-bit sync word"
    );
}
