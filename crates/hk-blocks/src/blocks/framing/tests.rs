//! Framing block tests: synthetic streams with hidden truth, driven through the registry with
//! the worked recipes' own parameters (`recipes/*.recipe.json`).

use std::collections::BTreeMap;

use hk_demod::rds::block::{Offset, encode_block};
use hk_estimate::framing::CATALOGUE;
use hk_model::CrcStatus;
use hk_recipe::{Params, PortType};
use serde_json::{Value, json};

use super::common::testutil::{Owned, bits_of, build, bytes_bits, noise, run_bits, run_frames};
use crate::block::ParamUpdate;
use crate::blocks::fec::tests::pocsag_word;
use crate::registry::BuildCtx;
use crate::status::Lock;

fn recipe_node(recipe: &str, id: &str) -> Value {
    let path = format!(
        "{}/../../recipes/{recipe}.recipe.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let r: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    r["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == id)
        .unwrap_or_else(|| panic!("{recipe}.{id}"))["params"]
        .clone()
}

fn extra(s: crate::Status, key: &str) -> f64 {
    s.extra
        .iter()
        .find(|e| e.0 == key)
        .map_or(f64::NAN, |e| e.1)
}

// ---- RDS: offset words A/B/C/C'/D + CRC 0x5B9 ----

#[test]
fn rds_offset_words_find_groups_and_crc_checks_and_corrects_them() {
    let infos: Vec<u16> = noise(16 * 24, 5)
        .chunks(16)
        .map(|c| c.iter().fold(0u16, |a, &b| (a << 1) | u16::from(b)))
        .collect();
    let truth: Vec<&[u16]> = infos.chunks(4).collect();
    let mut stream = noise(37, 11);
    for (g, group) in truth.iter().enumerate() {
        let c = if g == 3 { Offset::CPrime } else { Offset::C };
        for (info, off) in group.iter().zip([Offset::A, Offset::B, c, Offset::D]) {
            stream.extend(bits_of(u64::from(encode_block(*info, off)), 26));
        }
    }
    stream.extend(noise(10, 3));
    // Hidden errors: 1 bit in group 2 block B; a 3-bit burst (2 flips) in group 4 block D.
    stream[37 + 2 * 104 + 26 + 5] ^= 1;
    stream[37 + 4 * 104 + 78 + 10] ^= 1;
    stream[37 + 4 * 104 + 78 + 12] ^= 1;

    let sync_params = recipe_node("rds", "sync");
    let mut sync = build("sync_search", sync_params.clone(), PortType::Bits);
    let frames = run_bits(sync.as_mut(), &stream, 17, false);
    assert_eq!(frames.len(), 6);
    for (f, k) in frames.iter().zip(0..) {
        assert_eq!(f.info.bit_len, 104);
        assert_eq!(
            f.info.source_index,
            (37 + 104 * k) * 10,
            "first bit's source index"
        );
    }
    let st = sync.status();
    assert_eq!(st.lock, Lock::Locked);
    assert_eq!(extra(st, "blocks_bad"), 2.0);
    // Chunking invariance.
    for chunk in [1, 26, 4096] {
        let mut s = build("sync_search", sync_params.clone(), PortType::Bits);
        assert_eq!(
            run_bits(s.as_mut(), &stream, chunk, false),
            frames,
            "chunk {chunk}"
        );
    }

    let crc_params = recipe_node("rds", "crc");
    let mut crc = build("crc", crc_params.clone(), PortType::Frames);
    let checked = run_frames(crc.as_mut(), &frames, 4, false);
    let data =
        |g: &[u16]| -> Vec<u8> { g.iter().flat_map(|&i| bits_of(u64::from(i), 16)).collect() };
    for (g, f) in checked.iter().enumerate() {
        assert_eq!(f.info.bit_len, 64);
        if g == 2 || g == 4 {
            assert_eq!(f.info.check, CrcStatus::Invalid, "group {g}");
        } else {
            assert_eq!(f.info.check, CrcStatus::Valid, "group {g}");
            assert_eq!(f.bits, data(truth[g]));
        }
    }
    assert_eq!(crc.status().error_rate, Some(2.0 / 24.0));

    // Burst correction on a 10-bit check would validate garbage (26 × 1 offset / 2^10 = 2.5 %
    // at 1 bit, ~72 % bound at 5 bits with C/C′): refused, so groups 2 and 4 stay invalid.
    for burst in [1, 5] {
        let mut fix = crc_params.clone();
        fix["correct_burst_bits"] = json!(burst);
        let err = super::common::testutil::try_build("crc", fix, PortType::Frames)
            .err()
            .unwrap_or_else(|| panic!("RDS burst {burst} accepted"));
        assert!(err.contains("correct_burst_bits"), "{err}");
    }
}

/// T-185: block sync never locks on chance syndrome matches. 2^20 random bits (~15 min of RDS
/// bit rate): the recipe (three consecutive offset-consistent blocks) acquires nothing, where a
/// two-block chain acquires on chance (a match per position ≈ 1/1024, a chained pair ≈ 6e-6 per
/// bit, so ~6 expected).
#[test]
fn rds_sync_does_not_lock_on_random_bits() {
    let bits = noise(1 << 20, 185);
    let recipe = recipe_node("rds", "sync");
    let mut sync = build("sync_search", recipe.clone(), PortType::Bits);
    let frames = run_bits(sync.as_mut(), &bits, 4096, false);
    let st = sync.status();
    let mut two = recipe;
    two["lock_blocks"] = json!(2);
    let mut loose = build("sync_search", two, PortType::Bits);
    let loose_frames = run_bits(loose.as_mut(), &bits, 4096, false);
    let loose_acq = extra(loose.status(), "acquisitions");
    eprintln!(
        "random bits: recipe acquisitions {} frames {}; lock_blocks 2 acquisitions {loose_acq} frames {}",
        extra(st, "acquisitions"),
        frames.len(),
        loose_frames.len()
    );
    assert_eq!(extra(st, "acquisitions"), 0.0);
    assert!(frames.is_empty());
    assert_eq!(st.lock, Lock::Searching);
    assert!(
        loose_acq >= 1.0,
        "the two-block chain is the chance-lock baseline"
    );
}

/// T-185: lock needs three consecutive offset-consistent blocks, and a run of eight invalid
/// blocks drops it (well before the 20-in-50 window would).
#[test]
fn rds_sync_locks_on_three_blocks_and_drops_after_an_invalid_run() {
    let block = |info: u16, off: Offset| bits_of(u64::from(encode_block(info, off)), 26);
    let group = |g: u16| -> Vec<u8> {
        [Offset::A, Offset::B, Offset::C, Offset::D]
            .into_iter()
            .enumerate()
            .flat_map(|(i, off)| block(g * 7 + i as u16, off))
            .collect()
    };
    let params = recipe_node("rds", "sync");
    // A, B then garbage: two hits only, no lock.
    let mut stream = noise(40, 9);
    stream.extend(block(1, Offset::A));
    stream.extend(block(2, Offset::B));
    stream.extend(noise(26 * 12, 10));
    let mut s = build("sync_search", params.clone(), PortType::Bits);
    assert!(run_bits(s.as_mut(), &stream, 64, false).is_empty());
    assert_eq!(extra(s.status(), "acquisitions"), 0.0);

    // Two clean groups lock (at block C of the first) and emit both; then garbage: after eight
    // invalid blocks the lock is gone.
    let mut stream = noise(40, 9);
    stream.extend(group(1));
    stream.extend(group(2));
    let locked_len = stream.len();
    stream.extend(noise(26 * 7 + 5, 11));
    let mut s = build("sync_search", params.clone(), PortType::Bits);
    let frames = run_bits(s.as_mut(), &stream, 64, false);
    assert_eq!(
        frames.len(),
        3,
        "two groups and one garbage group while still locked"
    );
    assert_eq!(s.status().lock, Lock::Locked, "7 invalid blocks keep lock");
    assert_eq!(extra(s.status(), "acquisitions"), 1.0);
    stream.truncate(locked_len);
    stream.extend(noise(26 * 8 + 5, 11));
    let mut s = build("sync_search", params, PortType::Bits);
    run_bits(s.as_mut(), &stream, 64, false);
    assert_eq!(
        s.status().lock,
        Lock::Searching,
        "8 invalid blocks drop lock"
    );
}

// ---- POCSAG: sync 0x7CD215D8 + BCH(31,21) + message assembly across batches ----

const IDLE: u32 = 0x7A89_C197;

fn address(ric: u32, function: u32) -> u32 {
    pocsag_word(((ric >> 3) & 0x3_FFFF) << 2 | function)
}

fn message_words(payload: &[u8]) -> Vec<u32> {
    payload
        .chunks(20)
        .map(|c| pocsag_word(1 << 20 | c.iter().fold(0, |a, &b| (a << 1) | u32::from(b))))
        .collect()
}

#[test]
fn pocsag_batches_decode_and_assemble_across_batches() {
    // Hidden truth: two pages.
    let (ric1, ric2) = (1_234_567u32, 2_000_004u32); // slots 7 and 4
    let digits = b"2026091512345678901220260915123456789012";
    let payload1: Vec<u8> = digits
        .iter()
        .flat_map(|d| (0..4).map(move |k| ((d - b'0') >> k) & 1)) // 4-bit BCD, LSB first
        .collect();
    let mut payload2: Vec<u8> = b"HELLO"
        .iter()
        .flat_map(|c| (0..7).map(move |k| (c >> k) & 1)) // 7-bit, LSB first
        .collect();
    payload2.resize(40, 0);
    let m1 = message_words(&payload1);
    let m2 = message_words(&payload2);
    assert_eq!((m1.len(), m2.len()), (8, 2));

    let mut batch1 = [IDLE; 16];
    batch1[14] = address(ric1, 0);
    batch1[15] = m1[0];
    let mut batch2 = [IDLE; 16];
    batch2[..7].copy_from_slice(&m1[1..]);
    batch2[8] = address(ric2, 3);
    batch2[9..11].copy_from_slice(&m2);

    let mut stream: Vec<u8> = (0..576).map(|i| (i % 2 == 0) as u8).collect();
    let b1 = stream.len();
    stream.extend(bits_of(0x7CD2_15D8, 32));
    stream.extend(batch1.iter().flat_map(|&w| bits_of(u64::from(w), 32)));
    let b2 = stream.len();
    stream.extend(bits_of(0x7CD2_15D8, 32));
    stream.extend(batch2.iter().flat_map(|&w| bits_of(u64::from(w), 32)));
    stream.extend((0..64).map(|i| (i % 2) as u8));
    // Hidden errors: 1 bit in address 1, 2 sync bits in batch 2, 2 bits in a message word,
    // and a parity bit.
    stream[b1 + 32 + 14 * 32 + 5] ^= 1;
    stream[b2 + 3] ^= 1;
    stream[b2 + 17] ^= 1;
    stream[b2 + 32 + 3 * 32 + 8] ^= 1;
    stream[b2 + 32 + 3 * 32 + 22] ^= 1;
    stream[b2 + 32 + 9 * 32 + 31] ^= 1;

    let mut sync = build("sync_search", recipe_node("pocsag", "sync"), PortType::Bits);
    let batches = run_bits(sync.as_mut(), &stream, 100, true);
    assert_eq!(batches.len(), 2);
    assert!(batches.iter().all(|b| b.info.bit_len == 512));
    assert_eq!(batches[0].info.source_index, (b1 as u64 + 32) * 10);
    assert_eq!(extra(sync.status(), "sync_errors"), 2.0);

    let mut bch = build("bch", recipe_node("pocsag", "bch"), PortType::Frames);
    let decoded = run_frames(bch.as_mut(), &batches, 1, false);
    assert_eq!(decoded[0].info.corrected_bits, 1);
    assert_eq!(decoded[1].info.corrected_bits, 3);
    assert!(decoded.iter().all(|f| f.info.check == CrcStatus::Valid));
    assert_eq!(extra(bch.status(), "words_corrected"), 3.0);

    let mut msg = build("assemble", recipe_node("pocsag", "msg"), PortType::Frames);
    let pages = run_frames(msg.as_mut(), &decoded, 1, true);
    assert_eq!(pages.len(), 2);
    let expect = |ric: u32, function: u64, payload: &[u8]| {
        let mut v = bits_of(u64::from(ric >> 3), 18);
        v.extend(bits_of(function, 2));
        v.extend(bits_of(u64::from(ric & 7), 3));
        v.extend_from_slice(payload);
        v
    };
    assert_eq!(pages[0].bits, expect(ric1, 0, &payload1));
    assert_eq!(pages[1].bits, expect(ric2, 3, &payload2));
    // RIC = address ‖ slot, as the pocsag_message field map reads it (skip function bits).
    let ric_of = |b: &[u8]| {
        b[..18]
            .iter()
            .chain(&b[20..23])
            .fold(0u32, |a, &x| (a << 1) | u32::from(x))
    };
    assert_eq!(
        (ric_of(&pages[0].bits), ric_of(&pages[1].bits)),
        (ric1, ric2)
    );
    // Frame-granular check status: page 1 spans both batches, page 2 only batch 2.
    assert_eq!(pages[0].info.check, CrcStatus::Valid);
    assert_eq!(
        (pages[0].info.corrected_bits, pages[1].info.corrected_bits),
        (4, 3)
    );
    assert_eq!(extra(msg.status(), "messages"), 2.0);

    // A lost batch between the two halves closes the message instead of joining them: idle
    // batch, batch 1, (batch lost), batch 2.
    let spacing = 544 * 10;
    let idle_bits: Vec<u8> = (0..16).flat_map(|_| bits_of(u64::from(IDLE), 32)).collect();
    let mut frames = vec![
        Owned::from_bits(&idle_bits, 0, 0),
        decoded[0].clone(),
        decoded[1].clone(),
    ];
    for (k, f) in frames.iter_mut().enumerate() {
        f.info.index = k as u64;
        f.info.source_index = [0, spacing, 3 * spacing][k];
    }
    let mut msg = build("assemble", recipe_node("pocsag", "msg"), PortType::Frames);
    let pages = run_frames(msg.as_mut(), &frames, 3, true);
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[0].bits, expect(ric1, 0, &payload1[..20]));
    assert_eq!(pages[1].bits, expect(ric2, 3, &payload2));
    assert_eq!(extra(msg.status(), "orphan_words"), 7.0);
}

#[test]
fn sync_word_hot_edits_apply_in_place_and_geometry_rebuilds() {
    let params = recipe_node("pocsag", "sync");
    let mut sync = build("sync_search", params.clone(), PortType::Bits);
    let maps = BTreeMap::new();
    let ctx = BuildCtx {
        field_maps: &maps,
        input_types: &[PortType::Bits],
    };
    let mut p: Params = params.as_object().cloned().unwrap();
    p.insert("max_errors".into(), json!(0));
    assert_eq!(sync.update_params(&p, &ctx).unwrap(), ParamUpdate::Applied);
    p.insert("frame_bits".into(), json!(256));
    assert_eq!(sync.update_params(&p, &ctx).unwrap(), ParamUpdate::Rebuild);
    // With max_errors now 0, a 1-error sync word is no longer accepted.
    let mut stream = bits_of(0x7CD2_15D9, 32);
    stream.extend(vec![0u8; 512]);
    assert!(run_bits(sync.as_mut(), &stream, 64, false).is_empty());
    assert_eq!(sync.status().lock, Lock::Searching);
}

// ---- ACARS: LSB-first characters, ETX terminator + BCS trailer, CRC-16/KERMIT ----

fn odd_parity(c: u8) -> u8 {
    if c.count_ones() % 2 == 0 { c | 0x80 } else { c }
}

#[test]
fn acars_terminator_lsb_characters_and_crc16_kermit() {
    let mut body = vec![b'2'];
    body.extend(b".N12345");
    body.push(0x15); // NAK
    body.extend(b"H1");
    body.push(b'5');
    body.push(0x02); // STX
    body.extend(b"HELLO WORLD");
    body.push(0x03); // ETX
    let body: Vec<u8> = body.into_iter().map(odd_parity).collect();
    let kermit = CATALOGUE
        .iter()
        .find(|e| e.name == "CRC-16/KERMIT")
        .unwrap()
        .params;
    let bcs = kermit.compute(&body) as u16;
    let air_char = |c: u8| (0..8).map(move |k| (c >> k) & 1);
    let mut stream = vec![1u8; 64];
    stream.extend(bits_of(0xD5_5468_6880, 40)); // + * SYN SYN SOH, air order
    for &c in body.iter().chain(&bcs.to_le_bytes()).chain(&[0x7F]) {
        stream.extend(air_char(c));
    }
    stream.extend(vec![1u8; 30]);
    let mut on_frame = body.clone();
    on_frame.extend(bcs.to_le_bytes());

    let sync_params = recipe_node("acars", "sync");
    let mut sync = build("sync_search", sync_params.clone(), PortType::Bits);
    let frames = run_bits(sync.as_mut(), &stream, 64, false);
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0].bits,
        bytes_bits(&on_frame),
        "characters reversed, ends after BCS"
    );
    let mut bad = frames[0].clone();
    bad.bits[8 * 14 + 2] ^= 1; // inside "HELLO"
    let mut crc = build("crc", recipe_node("acars", "crc"), PortType::Frames);
    let out = run_frames(crc.as_mut(), &[frames[0].clone(), bad], 2, false);
    assert_eq!(out[0].info.check, CrcStatus::Valid);
    assert_eq!(out[0].bits, bytes_bits(&body), "BCS stripped");
    assert_eq!(out[1].info.check, CrcStatus::Invalid);
    for chunk in [1, 7, 1000] {
        let mut s = build("sync_search", sync_params.clone(), PortType::Bits);
        assert_eq!(
            run_bits(s.as_mut(), &stream, chunk, false),
            frames,
            "chunk {chunk}"
        );
    }

    // T-108: a non-coherent MSK receiver sees tones (mark = chip unchanged) and the recipe's
    // `chips` node integrates them back to chips with an arbitrary start level, so the frame
    // arrives in either polarity (acarsdec `acars.c` also accepts ~SYN).
    assert_eq!(
        recipe_node("acars", "chips"),
        json!({"mode": "transition-is-0", "direction": "encode"})
    );
    let tones: Vec<u8> = std::iter::once(1)
        .chain(stream.windows(2).map(|w| u8::from(w[0] == w[1])))
        .collect();
    let integrate = |start: u8| -> Vec<u8> {
        let mut level = start;
        tones
            .iter()
            .map(|&t| {
                level ^= t ^ 1;
                level
            })
            .collect()
    };
    let noise: Vec<u8> = (0..200u32)
        .map(|k| (k.wrapping_mul(2_654_435_761) >> 31) as u8)
        .collect();
    for start in [0u8, 1] {
        let mut chips = noise.clone();
        chips.extend(integrate(start));
        let mut s = build("sync_search", sync_params.clone(), PortType::Bits);
        let got = run_bits(s.as_mut(), &chips, 13, false);
        assert_eq!(got.len(), 1, "start level {start}");
        assert_eq!(got[0].bits, frames[0].bits, "start level {start}");
    }
}

// ---- length_from: ADS-B DF decides 56/112 ----

#[test]
fn length_from_cuts_adsb_frames_by_downlink_format() {
    let ppm = recipe_node("adsb", "ppm");
    let params = json!({"mode": "sync-word", "sync_word": "0xA1", "sync_bits": 8, "frame_bits": 112,
        "length_from": ppm["length_from"]});
    let df17 = bytes_bits(&[
        0x8D, 0x48, 0x40, 0xD6, 0x20, 0x2C, 0xC3, 0x71, 0xC3, 0x2C, 0xE0, 0x57, 0x60, 0x98,
    ]);
    let df11 = bytes_bits(&[0x5D, 0x48, 0x40, 0xD6, 0x12, 0x34, 0x56]);
    let mut stream = vec![0u8; 20];
    stream.extend(bits_of(0xA1, 8));
    stream.extend(&df17);
    stream.extend(vec![0u8; 20]);
    stream.extend(bits_of(0xA1, 8));
    stream.extend(&df11);
    stream.extend(df17[..40].iter().copied()); // after the 56-bit frame: not part of it
    let mut sync = build("sync_search", params, PortType::Bits);
    let frames = run_bits(sync.as_mut(), &stream, 33, false);
    assert_eq!(
        frames.iter().map(|f| f.info.bit_len).collect::<Vec<_>>(),
        [112, 56]
    );
    assert_eq!(frames[1].bits, df11);
    let mut crc = build("crc", recipe_node("adsb", "crc"), PortType::Frames);
    assert_eq!(
        run_frames(crc.as_mut(), &frames[..1], 1, false)[0]
            .info
            .check,
        CrcStatus::Valid
    );
}

// ---- deframe, interleave ----

#[test]
fn deframe_fixed_bits_with_offset_and_variable_subframes() {
    let mut stream = vec![1u8, 1, 1];
    stream.extend(bytes_bits(&[0x12, 0x34, 0x56]));
    stream.extend([1, 0, 1]);
    let params = json!({"frame_bits": 8, "offset_bits": 3});
    let mut a = build("deframe", params.clone(), PortType::Bits);
    let frames = run_bits(a.as_mut(), &stream, 2, true);
    let lens: Vec<u32> = frames.iter().map(|f| f.info.bit_len).collect();
    assert_eq!(lens, [8, 8, 8, 3], "partial frame flushed at END");
    assert_eq!(frames[1].bits, bytes_bits(&[0x34]));
    let mut b = build("deframe", params, PortType::Bits);
    assert_eq!(run_bits(b.as_mut(), &stream, 100, true), frames);

    // Length byte = payload bytes after it.
    let parent = Owned::from_bits(&bytes_bits(&[0x02, 0xAA, 0xBB, 0x01, 0xCC, 0x05]), 0, 70);
    let params = json!({"frame_bits": 64, "length_from": {"offset_bits": 0, "bits": 8, "scale": 8, "add": 8}});
    let mut d = build("deframe", params, PortType::Frames);
    let subs = run_frames(d.as_mut(), &[parent], 1, false);
    assert_eq!(subs.len(), 2);
    assert_eq!(subs[0].bits, bytes_bits(&[0x02, 0xAA, 0xBB]));
    assert_eq!(subs[1].bits, bytes_bits(&[0x01, 0xCC]));
    assert_eq!(subs[1].info.source_index, 70);
    assert_eq!(extra(d.status(), "bits_dropped"), 8.0);
}

#[test]
fn interleavers_permute_and_invert() {
    let f = Owned::from_bits(&[1, 0, 1, 1, 0, 0, 1, 0], 0, 0);
    let mut il = build("interleave", json!({"depth": 3}), PortType::Frames);
    assert_eq!(
        run_frames(il.as_mut(), &[f], 1, false)[0].bits,
        [1, 1, 1, 0, 0, 0, 1, 0]
    );
    for params in [json!({"depth": 7}), json!({"permutation": [2, 0, 3, 1]})] {
        let truth = Owned::from_bits(&noise(1001, 17), 0, 0);
        let mut il = build("interleave", params.clone(), PortType::Frames);
        let mixed = run_frames(il.as_mut(), std::slice::from_ref(&truth), 1, false);
        assert_ne!(mixed[0].bits, truth.bits);
        let mut de = build("deinterleave", params, PortType::Frames);
        assert_eq!(
            run_frames(de.as_mut(), &mixed, 1, false)[0].bits,
            truth.bits
        );
    }
}
