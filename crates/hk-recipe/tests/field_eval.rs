//! T-089: the declarative parser over the M1 worked recipes' field maps (ADR-0011 §3, §5.1):
//! ADS-B (DF-conditioned layers, altitude without its Q bit, scaled units), POCSAG (RIC around
//! the function bits, numeric BCD and 7-bit text, both LSB first), misfit frames, and
//! re-parsing a recorded decoded stream of 10 000 frames (stream-contract §14.7) in well under
//! a second. Expected values are hidden truth written independently of the maps.

use std::time::{Duration, Instant};

use hk_model::{ContentClass, CrcStatus};
use hk_recipe::fields::eval::Evaluator;
use hk_recipe::{FieldMap, Recipe};
use hk_stream::frame::encode_frame;
use hk_stream::inspector::{
    FitErrorKind, FitStatus, FitSummary, FrameContent, FrameMetadata, FrameRecord,
    INSPECTOR_MESSAGE_SCHEMA, LayerTree, RecordedFrames, from_hex, to_hex,
};
use hk_stream::{StreamHeader, StreamKind};
use serde_json::{Value, json};

fn field_map(recipe: &str, map: &str) -> FieldMap {
    let path = format!(
        "{}/../../recipes/{recipe}.recipe.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let r: Recipe = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    r.field_maps[map].clone()
}

fn value(t: &LayerTree, path: &str) -> Value {
    t.node(path)
        .unwrap_or_else(|| {
            panic!(
                "{path} missing: {:?}",
                t.nodes.iter().map(|n| &n.path).collect::<Vec<_>>()
            )
        })
        .value
        .clone()
        .unwrap_or(Value::Null)
}

fn text(t: &LayerTree, path: &str) -> String {
    t.node(path).and_then(|n| n.text.clone()).unwrap()
}

fn eval_hex(ev: &Evaluator, hex: &str) -> LayerTree {
    let bytes = from_hex(hex).unwrap();
    ev.eval(&bytes, bytes.len() as u32 * 8)
}

/// Packs bits MSB-first.
fn pack(bits: &[u8]) -> Vec<u8> {
    bits.chunks(8)
        .map(|c| c.iter().enumerate().fold(0, |a, (k, b)| a | (b << (7 - k))))
        .collect()
}

fn push_msb(bits: &mut Vec<u8>, value: u64, width: u32) {
    bits.extend((0..width).rev().map(|k| ((value >> k) & 1) as u8));
}

fn push_lsb(bits: &mut Vec<u8>, value: u64, width: u32) {
    bits.extend((0..width).map(|k| ((value >> k) & 1) as u8));
}

#[test]
fn adsb_airborne_position_velocity_and_short_replies() {
    let ev = Evaluator::new(&field_map("adsb", "adsb_frame")).unwrap();

    // DF17 airborne position, ICAO 40621D, 38 000 ft, even CPR 93000/51372.
    let t = eval_hex(&ev, "8d40621d58c382d690c8ac2863a7");
    assert_eq!(t.fit, FitStatus::Ok, "{:?}", t.errors);
    assert_eq!(value(&t, "df"), json!(17));
    assert_eq!(value(&t, "icao"), json!(0x40621d));
    assert_eq!(text(&t, "icao"), "0x40621D");
    assert_eq!(value(&t, "me.tc"), json!(11));
    assert_eq!(value(&t, "me.airborne_position.q"), json!(1));
    assert_eq!(value(&t, "me.airborne_position.altitude"), json!(38000.0));
    assert_eq!(text(&t, "me.airborne_position.altitude"), "38000 ft");
    assert_eq!(text(&t, "me.airborne_position.f"), "even");
    assert_eq!(value(&t, "me.airborne_position.cpr_lat"), json!(93000));
    assert_eq!(value(&t, "me.airborne_position.cpr_lon"), json!(51372));
    assert!(t.node("me.velocity").is_none() && t.node("me.identification").is_none());
    // Ranges: the altitude spans ME bits 8–19 (frame bits 40–51) → bytes 5–6.
    let alt = t.node("me.airborne_position.altitude").unwrap();
    assert_eq!((alt.bits, alt.bytes), ([40, 12], [5, 7]));
    assert!(t.fields_at_byte(5).contains(&alt.id));
    assert_eq!(t.byte_index.len(), 14);
    assert_eq!(t.node("me").unwrap().parent, None);

    // DF17 airborne velocity: 8 kt west, 159 kt south, 832 ft/min down.
    let t = eval_hex(&ev, "8d485020994409940838175b284f");
    assert_eq!(t.fit, FitStatus::Ok, "{:?}", t.errors);
    assert_eq!(value(&t, "me.tc"), json!(19));
    assert_eq!(text(&t, "me.velocity.dew"), "west");
    assert_eq!(text(&t, "me.velocity.vew"), "8 kt");
    assert_eq!(text(&t, "me.velocity.dns"), "south");
    assert_eq!(value(&t, "me.velocity.vns"), json!(159.0));
    assert_eq!(text(&t, "me.velocity.svr"), "down");
    assert_eq!(text(&t, "me.velocity.vertical_rate"), "832 ft/min");

    // DF11 all-call reply (56 bits): ICAO, no ME layer. DF4: neither.
    let t = eval_hex(&ev, "5d4840d6202cc3");
    assert_eq!(t.fit, FitStatus::Ok, "{:?}", t.errors);
    assert_eq!(
        (value(&t, "df"), value(&t, "icao")),
        (json!(11), json!(0x4840d6))
    );
    assert!(t.node("me").is_none());
    let t = eval_hex(&ev, "20000f1f684a6c");
    assert_eq!(value(&t, "df"), json!(4));
    assert!(t.node("icao").is_none() && t.node("me").is_none());
}

#[test]
fn pocsag_numeric_bcd_and_alphanumeric_text() {
    let ev = Evaluator::new(&field_map("pocsag", "pocsag_message")).unwrap();
    let ric: u64 = 1_234_567;
    let header = |function: u64| {
        let mut bits = Vec::new();
        push_msb(&mut bits, ric >> 3, 18);
        push_msb(&mut bits, function, 2);
        push_msb(&mut bits, ric & 7, 3);
        bits
    };

    // Numeric page "123-45 " (function 0): 4-bit BCD, LSB first per character.
    let mut bits = header(0);
    for c in [1u64, 2, 3, 0xd, 4, 5, 0xc] {
        push_lsb(&mut bits, c, 4);
    }
    let bytes = pack(&bits);
    let t = ev.eval(&bytes, bits.len() as u32);
    assert_eq!(t.fit, FitStatus::Ok, "{:?}", t.errors);
    assert_eq!(value(&t, "ric"), json!(ric));
    assert_eq!(value(&t, "function"), json!(0));
    assert_eq!(value(&t, "numeric"), json!("123-45 "));
    assert!(t.node("alpha").is_none());
    assert_eq!(t.node("numeric").unwrap().bits, [23, 28]);

    // Alphanumeric page "Hi!" (function 3): 7-bit ASCII LSB first, 5 padding bits after.
    let mut bits = header(3);
    for c in "Hi!".bytes() {
        push_lsb(&mut bits, u64::from(c), 7);
    }
    bits.extend([0; 5]);
    let t = ev.eval(&pack(&bits), bits.len() as u32);
    assert_eq!(t.fit, FitStatus::Ok, "{:?}", t.errors);
    assert_eq!(value(&t, "ric"), json!(ric));
    assert_eq!(value(&t, "alpha"), json!("Hi!"));
    assert_eq!(
        t.node("alpha").unwrap().bits,
        [23, 21],
        "padding is not a character"
    );
    assert!(t.node("numeric").is_none());
}

#[test]
fn misfit_frames_report_per_field_errors_and_keep_going() {
    let adsb = Evaluator::new(&field_map("adsb", "adsb_frame")).unwrap();
    // A DF17 cut to 56 bits: header fields decode, the ME layer runs out of bits.
    let t = eval_hex(&adsb, "8d40621d58c382");
    assert_eq!(t.fit, FitStatus::Partial);
    assert_eq!(value(&t, "icao"), json!(0x40621d));
    let me = t.node("me").unwrap();
    assert!(me.error);
    assert_eq!(me.bits, [32, 24], "clipped to the bits present");
    let e = t.errors.iter().find(|e| e.path == "me").unwrap();
    assert_eq!(
        (e.kind, e.need_bits, e.have_bits),
        (FitErrorKind::OutOfBounds, Some(56), Some(24))
    );
    // Inside the clipped layer, tc still decodes (the first 5 ME bits are present).
    assert_eq!(value(&t, "me.tc"), json!(11));

    // POCSAG: 10 bits cannot hold the RIC; the conditional text fields depend on `function`,
    // which is missing too, so they report missing references. Nothing decodes.
    let pocsag = Evaluator::new(&field_map("pocsag", "pocsag_message")).unwrap();
    let t = pocsag.eval(&[0xff, 0xc0], 10);
    assert_eq!(t.fit, FitStatus::Failed);
    let kinds: Vec<_> = t.errors.iter().map(|e| (e.path.as_str(), e.kind)).collect();
    assert_eq!(
        kinds,
        [
            ("ric", FitErrorKind::OutOfBounds),
            ("function", FitErrorKind::OutOfBounds),
            ("numeric", FitErrorKind::MissingReference),
            ("alpha", FitErrorKind::MissingReference),
        ]
    );

    // ACARS characters with a parity failure: rendered U+FFFD, the frame is partial.
    let acars = Evaluator::new(&field_map("acars", "acars_block")).unwrap();
    let odd = |c: u8| if c.count_ones() % 2 == 0 { c | 0x80 } else { c };
    let mut block: Vec<u8> = b"2.N123AB Q01".iter().map(|&c| odd(c)).collect();
    block[3] ^= 0x01; // corrupt '1' of the registration
    block.push(0x02); // STX (a uint, no parity)
    block.extend(b"HELLO".iter().map(|&c| odd(c)));
    let t = acars.eval(&block, block.len() as u32 * 8);
    assert_eq!(t.fit, FitStatus::Partial);
    assert_eq!(value(&t, "registration"), json!(".N\u{FFFD}23AB"));
    assert_eq!(value(&t, "label"), json!("Q0"));
    assert_eq!(value(&t, "text"), json!("HELLO"));
    assert_eq!(t.errors.len(), 1);
    assert_eq!(t.errors[0].kind, FitErrorKind::Parity);
}

/// Encodes a recorded decoded stream (§14.7): header frame, then frame records without layers,
/// with a status record interleaved.
fn recording(frames: usize) -> Vec<u8> {
    let mut h = StreamHeader::new(
        "inspector/p1/groups",
        StreamKind::Messages,
        ContentClass::Unrestricted,
        "hk-pipeline:recipe:rds@1",
    );
    h.message_schema = Some(INSPECTOR_MESSAGE_SCHEMA.into());
    let mut out = Vec::new();
    encode_frame(&mut out, &h.to_json_bytes().unwrap(), h.max_frame_len).unwrap();
    let ps = b"HACKRIFF";
    for i in 0..frames {
        // Group 0A PS segment i % 4, PI 0x54A8, PTY 10.
        let seg = (i % 4) as u64;
        let mut bits = Vec::new();
        push_msb(&mut bits, 0x54a8, 16);
        push_msb(&mut bits, 0, 4);
        push_msb(&mut bits, 0, 1);
        push_msb(&mut bits, 1, 1);
        push_msb(&mut bits, 10, 5);
        push_msb(&mut bits, 0b010, 3);
        push_msb(&mut bits, seg, 2);
        push_msb(&mut bits, 0xe0cd, 16);
        push_msb(&mut bits, u64::from(ps[2 * seg as usize]), 8);
        push_msb(&mut bits, u64::from(ps[2 * seg as usize + 1]), 8);
        let rec = FrameRecord {
            record_type: "frame".into(),
            seq: i as u64,
            t: 1_789_300_800_000_000_000 + i as i64 * 87_000_000,
            content_class: ContentClass::Unrestricted,
            gated: false,
            crc_status: Some(CrcStatus::Valid),
            decoder: Some("recipe:rds@1".into()),
            frame_model: Some("rds".into()),
            emitter_id: None,
            metadata: FrameMetadata {
                frame: Some(i as u64),
                sample_index: Some(i as u64 * 20_880),
                bit_len: Some(64),
                recipe_version: Some(1),
                edit_rev: Some(0),
                fit: Some(FitStatus::None),
                ..Default::default()
            },
            content: Some(FrameContent {
                hex: to_hex(&pack(&bits)),
                layers: None,
            }),
        };
        encode_frame(
            &mut out,
            &serde_json::to_vec(&rec).unwrap(),
            h.max_frame_len,
        )
        .unwrap();
        if i % 1000 == 0 {
            let status = json!({"type": "status", "seq": i, "t_ns": 0, "content_class": "unrestricted",
                                "gated": false, "metadata": {"crc.error_rate": 0.0}});
            encode_frame(
                &mut out,
                &serde_json::to_vec(&status).unwrap(),
                h.max_frame_len,
            )
            .unwrap();
        }
    }
    out
}

#[test]
fn reparsing_a_recorded_stream_of_10k_frames_is_instant() {
    const FRAMES: usize = 10_000;
    let bytes = recording(FRAMES);
    let map = field_map("rds", "rds_group");

    let started = Instant::now();
    let ev = Evaluator::new(&map).unwrap();
    let mut reader = RecordedFrames::open(&bytes[..]).unwrap();
    let mut fit = FitSummary::default();
    let mut ps = [[0u8; 2]; 4];
    while let Some(rec) = reader.next_frame().unwrap() {
        let tree = ev.eval_record(&rec);
        fit.add(tree.as_ref());
        let tree = tree.unwrap();
        let seg = tree
            .node("ps.segment")
            .unwrap()
            .value
            .as_ref()
            .unwrap()
            .as_u64()
            .unwrap();
        let chars = tree.node("ps.chars").unwrap().value.clone().unwrap();
        ps[seg as usize].copy_from_slice(chars.as_str().unwrap().as_bytes());
    }
    let elapsed = started.elapsed();
    eprintln!(
        "re-parsed {FRAMES} recorded frames ({} bytes) in {elapsed:?}",
        bytes.len()
    );

    assert_eq!(
        (fit.frames, fit.ok),
        (FRAMES as u64, FRAMES as u64),
        "{fit:?}"
    );
    assert_eq!(reader.skipped(), 10, "status records skipped");
    assert_eq!(ps.concat(), b"HACKRIFF");
    // Debug builds on a loaded CI machine included; release is several times faster.
    let budget = if cfg!(debug_assertions) {
        Duration::from_millis(1500)
    } else {
        Duration::from_millis(300)
    };
    assert!(elapsed < budget, "re-parse took {elapsed:?}");
}
