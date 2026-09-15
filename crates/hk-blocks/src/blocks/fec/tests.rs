//! FEC block tests with known vectors and hidden-truth synthetic frames.

use hk_model::CrcStatus;
use hk_recipe::PortType;
use serde_json::json;

use crate::blocks::framing::common::testutil::{
    Owned, bits_of, build, bytes_bits, noise, run_frames,
};

fn frame(bits: &[u8]) -> Owned {
    Owned::from_bits(bits, 0, 0)
}

fn hex_bits(s: &str) -> Vec<u8> {
    let bytes: Vec<u8> = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect();
    bytes_bits(&bytes)
}

#[test]
fn crc24_mode_s_known_frame_checks_and_corrects_one_bit() {
    // DF17 identification of KLM1023 (ICAO 4840D6): parity 0x576098 = CRC-24 of the 88-bit body.
    let good = hex_bits("8D4840D6202CC371C32CE0576098");
    let params = json!({"width": 24, "poly": "0xFFF409", "span": {"start_bit": 0, "end_trim_bits": 0}, "strip": false});
    let mut bad = good.clone();
    bad[40] ^= 1;
    let mut b = build("crc", params.clone(), PortType::Frames);
    let out = run_frames(b.as_mut(), &[frame(&good), frame(&bad)], 1, false);
    assert_eq!(out[0].info.check, CrcStatus::Valid);
    assert_eq!(out[0].bits, good);
    assert_eq!(out[1].info.check, CrcStatus::Invalid);
    assert_eq!(b.status().error_rate, Some(0.5));

    let mut fix = params;
    fix["correct_burst_bits"] = json!(1);
    let mut b = build("crc", fix, PortType::Frames);
    let mut bad_check = good.clone();
    bad_check[100] ^= 1; // an error inside the parity field
    let out = run_frames(b.as_mut(), &[frame(&bad), frame(&bad_check)], 2, false);
    for f in &out {
        assert_eq!(f.info.check, CrcStatus::Valid);
        assert_eq!(f.info.corrected_bits, 1);
        assert_eq!(f.bits, good);
    }
}

#[test]
fn crc16_kermit_check_value_and_strip() {
    // CRC-16/KERMIT ("CRC-16/CCITT", ACARS): check("123456789") = 0x2189, sent low byte first.
    let mut bits = bytes_bits(b"123456789");
    bits.extend(bytes_bits(&[0x89, 0x21]));
    let params =
        json!({"width": 16, "poly": "0x1021", "refin": true, "refout": true, "strip": true});
    let mut b = build("crc", params, PortType::Frames);
    let mut wrong_variant = bytes_bits(b"123456789");
    wrong_variant.extend(bytes_bits(&[0x21, 0x89])); // big-endian: not how KERMIT is sent
    let out = run_frames(b.as_mut(), &[frame(&bits), frame(&wrong_variant)], 2, false);
    assert_eq!(out[0].info.check, CrcStatus::Valid);
    assert_eq!(out[0].bits, bytes_bits(b"123456789"));
    assert_eq!(out[1].info.check, CrcStatus::Invalid);
    // CCITT-FALSE (non-reflected, init 0xFFFF) over the same body is a different CRC.
    let mut false_ = bytes_bits(b"123456789");
    false_.extend(bits_of(0x29B1, 16));
    let mut b = build(
        "crc",
        json!({"width": 16, "poly": "0x1021", "init": "0xFFFF", "strip": false}),
        PortType::Frames,
    );
    assert_eq!(
        run_frames(b.as_mut(), &[frame(&false_)], 1, false)[0]
            .info
            .check,
        CrcStatus::Valid
    );
}

/// POCSAG codeword for 21 data bits: BCH(31,21) check (g = 0x769) and even parity.
pub(crate) fn pocsag_word(data21: u32) -> u32 {
    let mut reg = u64::from(data21) << 10;
    for i in (10..31).rev() {
        if reg >> i & 1 == 1 {
            reg ^= 0x769 << (i - 10);
        }
    }
    let cw = (u64::from(data21) << 10 | reg) as u32;
    (cw << 1) | (cw.count_ones() & 1)
}

#[test]
fn bch_31_21_corrects_every_one_and_two_bit_error_and_refuses_three() {
    let params = json!({"word_bits": 32, "n": 31, "k": 21, "poly": "0x769", "parity": "even", "correct_bits": 2});
    let truth = noise(21 * 8, 99);
    let words: Vec<u32> = truth
        .chunks(21)
        .map(|c| pocsag_word(c.iter().fold(0, |a, &b| (a << 1) | u32::from(b))))
        .collect();
    // Known vectors: the POCSAG idle word and sync word are themselves valid codewords.
    assert_eq!(pocsag_word(0x7A89_C197 >> 11), 0x7A89_C197);
    assert_eq!(pocsag_word(0x7CD2_15D8 >> 11), 0x7CD2_15D8);
    let clean: Vec<u8> = words
        .iter()
        .flat_map(|&w| bits_of(u64::from(w), 32))
        .collect();
    let mut frames = Vec::new();
    let mut expect_corrected = Vec::new();
    for i in 0..32 {
        for j in i..32 {
            let mut f = clean.clone();
            f[(i + 3) % 32] ^= 1;
            if j != i {
                f[64 + j] ^= 1;
            }
            frames.push(frame(&f));
            expect_corrected.push(if j == i { 1 } else { 2 });
        }
    }
    let mut b = build("bch", params.clone(), PortType::Frames);
    let out = run_frames(b.as_mut(), &frames, 7, false);
    assert_eq!(out.len(), frames.len());
    for (f, c) in out.iter().zip(expect_corrected) {
        assert_eq!(f.info.check, CrcStatus::Valid);
        assert_eq!(f.info.corrected_bits, c);
        assert_eq!(f.bits, clean);
    }
    // Three errors in one word: detected (parity or no syndrome), never silently accepted.
    let mut three = clean.clone();
    for k in [2, 9, 20] {
        three[32 + k] ^= 1;
    }
    let mut b = build("bch", params, PortType::Frames);
    let out = run_frames(b.as_mut(), &[frame(&three)], 1, false);
    assert_eq!(out[0].info.check, CrcStatus::Invalid);
    assert_eq!(
        b.status()
            .extra
            .iter()
            .find(|e| e.0 == "words_bad")
            .unwrap()
            .1,
        1.0
    );
}

#[test]
fn parity_units_and_strip() {
    // 7-bit characters with odd parity in the top bit (ACARS after LSB reversal), then 16 bits
    // of CRC not covered.
    let text = b"N12345";
    let chars: Vec<u8> = text
        .iter()
        .map(|&c| if c.count_ones() % 2 == 0 { c | 0x80 } else { c })
        .collect();
    let mut bits = bytes_bits(&chars);
    bits.extend(bits_of(0xBEEF, 16));
    let params = json!({"unit_bits": 8, "parity": "odd", "position": "first", "span": {"end_trim_bits": 16}, "strip": true});
    let mut bad = bits.clone();
    bad[9] ^= 1;
    let mut b = build("parity", params, PortType::Frames);
    let out = run_frames(b.as_mut(), &[frame(&bits), frame(&bad)], 1, false);
    assert_eq!(out[0].info.check, CrcStatus::Valid);
    assert_eq!(out[0].bits.len(), 6 * 7 + 16);
    let mut expect: Vec<u8> = text
        .iter()
        .flat_map(|&c| bits_of(u64::from(c), 7))
        .collect();
    expect.extend(bits_of(0xBEEF, 16));
    assert_eq!(out[0].bits, expect);
    assert_eq!(out[1].info.check, CrcStatus::Invalid);
}

#[test]
fn checksums_xor_and_ones_complement() {
    // NMEA: XOR of "GPGLL,…" characters.
    let body = b"GPGLL,5300.97914,N,00259.98174,E,125926,A";
    let x = body.iter().fold(0u8, |a, &b| a ^ b);
    let mut bits = bytes_bits(body);
    bits.extend(bits_of(u64::from(x), 8));
    let mut b = build("checksum", json!({"algorithm": "xor"}), PortType::Frames);
    let out = run_frames(b.as_mut(), &[frame(&bits)], 1, false);
    assert_eq!(out[0].info.check, CrcStatus::Valid);
    assert_eq!(out[0].bits, bytes_bits(body));
    // IPv4 header checksum (RFC 1071 example header), check field moved to the end.
    let header = [
        0x45, 0x00, 0x00, 0x73, 0x00, 0x00, 0x40, 0x00, 0x40, 0x11, 0xC0, 0xA8, 0x00, 0x01, 0xC0,
        0xA8, 0x00, 0xC7,
    ];
    let mut bits = bytes_bits(&header);
    bits.extend(bits_of(0xB861, 16));
    let params = json!({"algorithm": "ones-complement", "unit_bits": 16, "complement": true, "strip": false});
    let mut bad = bits.clone();
    bad[3] ^= 1;
    let mut b = build("checksum", params, PortType::Frames);
    let out = run_frames(b.as_mut(), &[frame(&bits), frame(&bad)], 2, false);
    assert_eq!(out[0].info.check, CrcStatus::Valid);
    assert_eq!(out[1].info.check, CrcStatus::Invalid);
}
