//! FEC block tests with known vectors and hidden-truth synthetic frames.

use hk_model::CrcStatus;
use hk_recipe::PortType;
use serde_json::json;

use crate::blocks::framing::common::testutil::{
    Owned, bits_of, build, bytes_bits, noise, run_frames, run_frames_period, try_build,
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

fn extra(b: &dyn crate::block::Block, key: &str) -> f64 {
    b.status()
        .extra
        .iter()
        .find(|e| e.0 == key)
        .unwrap_or_else(|| panic!("{key}"))
        .1
}

#[test]
fn crc_burst_correction_never_manufactures_valid_frames_from_noise() {
    // RDS blocks (10-bit check, two offsets at C/C′): rejected at any burst.
    let rds = |burst: u32| {
        json!({"width": 10, "poly": "0x5B9", "correct_burst_bits": burst,
            "blocks": {"data_bits": 16, "check_bits": 10,
                "offsets": [["0x0FC"], ["0x198"], ["0x168", "0x350"], ["0x1B4"]]}})
    };
    assert!(try_build("crc", rds(0), PortType::Frames).is_ok());
    for burst in [1, 5] {
        assert!(
            try_build("crc", rds(burst), PortType::Frames).is_err(),
            "burst {burst}"
        );
    }
    // CRC-8 whole frame: even 1 bit over the check word alone is 8 / 256.
    assert!(
        try_build(
            "crc",
            json!({"width": 8, "poly": "0x07", "correct_burst_bits": 1}),
            PortType::Frames
        )
        .is_err()
    );

    // CRC-24 Mode S, 1-bit correction: accepted; 112 / 2^24 ≈ 6.7e-6 per frame, so 4000 noise
    // frames produce no valid frame.
    let adsb = json!({"width": 24, "poly": "0xFFF409", "strip": false, "correct_burst_bits": 1});
    let mut b = try_build("crc", adsb, PortType::Frames).unwrap();
    let frames: Vec<Owned> = noise(112 * 4000, 7).chunks(112).map(frame).collect();
    let out = run_frames(b.as_mut(), &frames, 64, false);
    assert_eq!(out.len(), frames.len());
    assert!(out.iter().all(|f| f.info.check == CrcStatus::Invalid));
    assert_eq!(extra(b.as_ref(), "correction_skipped"), 0.0);

    // CRC-16, 1 bit: accepted at build (short frames), refused at run time on 200-bit frames
    // (200 / 2^16 ≈ 3e-3): no corrections, every failing frame counted as skipped.
    let ccitt = json!({"width": 16, "poly": "0x1021", "strip": false, "correct_burst_bits": 1});
    let mut b = try_build("crc", ccitt, PortType::Frames).unwrap();
    let frames: Vec<Owned> = noise(200 * 1000, 11).chunks(200).map(frame).collect();
    let out = run_frames(b.as_mut(), &frames, 64, false);
    assert!(out.iter().all(|f| f.info.corrected_bits == 0));
    let invalid = out
        .iter()
        .filter(|f| f.info.check == CrcStatus::Invalid)
        .count();
    assert!(invalid >= 998, "{invalid}");
    assert_eq!(extra(b.as_ref(), "correction_skipped"), invalid as f64);
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

// --- T-210: RDS block correction while block-synced -------------------------------------------

/// The RDS group of four information words as 104 bits (check words and offsets from the
/// existing decoder, the tutorial's oracle).
fn rds_group_bits(words: [u16; 4]) -> Vec<u8> {
    use hk_demod::rds::{Offset, encode_block};
    let offsets = [Offset::A, Offset::B, Offset::C, Offset::D];
    words
        .iter()
        .zip(offsets)
        .flat_map(|(w, o)| bits_of(u64::from(encode_block(*w, o)), 26))
        .collect()
}

const RDS_WORDS: [u16; 4] = [0x1694, 0x0841, 0xe0cd, 0x5544];
/// Frame period in source samples (2.4 MS/s over 104 bits at 1187.5 Bd is ≈ 210 k; any
/// consistent value works, the block only checks that frames follow each other).
const RDS_PERIOD: f64 = 2080.0;

fn rds_params(synced: bool) -> serde_json::Value {
    let mut p = json!({"width": 10, "poly": "0x5B9", "strip": true,
        "blocks": {"data_bits": 16, "check_bits": 10,
            "offsets": [["0x0FC"], ["0x198"], ["0x168", "0x350"], ["0x1B4"]]}});
    if synced {
        p["synced_correction"] = json!({"burst_bits": 2, "lock_blocks": 3, "unlock_run": 8});
    }
    p
}

/// `n` frames of `bits`, one frame period apart from `first` (frame index `from`).
fn rds_frames(bits: &[Vec<u8>], from: u64) -> Vec<Owned> {
    bits.iter()
        .enumerate()
        .map(|(i, b)| Owned::from_bits(b, from + i as u64, (from + i as u64) * RDS_PERIOD as u64))
        .collect()
}

/// Every ≤2-bit burst in a block is corrected once the lattice is synced, the group is
/// `corrected` (never `valid`) with its bits restored, and nothing is corrected before sync.
#[test]
fn rds_synced_correction_fixes_every_burst_of_one_or_two_bits_and_marks_the_group_corrected() {
    let clean = rds_group_bits(RDS_WORDS);
    let payload: Vec<u8> = RDS_WORDS
        .iter()
        .flat_map(|w| bits_of(u64::from(*w), 16))
        .collect();
    // Frame 0 clean (its blocks sync the lattice), then one frame per error pattern.
    let mut bits = vec![clean.clone()];
    let mut want = Vec::new();
    for slot in 0..4usize {
        for st in 0..26usize {
            for len in [1usize, 2] {
                if st + len > 26 {
                    continue;
                }
                let mut f = clean.clone();
                for k in 0..len {
                    f[slot * 26 + st + k] ^= 1;
                }
                bits.push(f);
                want.push((slot, len as u32));
            }
        }
    }
    let mut b = build("crc", rds_params(true), PortType::Frames);
    let out = run_frames_period(b.as_mut(), &rds_frames(&bits, 0), 8, false, RDS_PERIOD);
    assert_eq!(out.len(), bits.len());
    assert_eq!(out[0].info.check, CrcStatus::Valid);
    assert_eq!(out[0].bits, payload);
    // Block C accepts two offset words (C and C′), so a few bursts are ambiguous: two different
    // bursts turn the block into an allowed offset. Those are refused (invalid), never guessed.
    let (mut corrected, mut ambiguous) = (0usize, 0usize);
    for (f, (slot, len)) in out[1..].iter().zip(&want) {
        match f.info.check {
            CrcStatus::Corrected => {
                assert_eq!(f.info.corrected_bits, *len, "slot {slot}");
                assert_eq!(
                    f.bits, payload,
                    "corrected bits differ from the clean group"
                );
                corrected += 1;
            }
            CrcStatus::Invalid => {
                assert_eq!(*slot, 2, "only block C (two offsets) may be ambiguous");
                assert_eq!(f.info.corrected_bits, 0);
                ambiguous += 1;
            }
            other => panic!("{other:?}"),
        }
    }
    eprintln!("[T-210] ≤2-bit bursts corrected {corrected}, ambiguous (block C) {ambiguous}");
    assert_eq!(corrected + ambiguous, want.len());
    assert!(
        corrected >= want.len() * 9 / 10,
        "{corrected} of {} corrected",
        want.len()
    );
    assert_eq!(extra(b.as_ref(), "frames_corrected"), corrected as f64);
    assert_eq!(extra(b.as_ref(), "frames_ok"), 1.0, "only frame 0 is clean");
    assert_eq!(
        extra(b.as_ref(), "frames_bad"),
        (corrected + ambiguous) as f64,
        "corrected frames are not CRC-valid evidence"
    );
}

/// Correction is inactive while the lattice is unsynced: a stream that never has `lock_blocks`
/// clean blocks in a row is never corrected, and a frame that does not follow the previous one
/// (a gap, or a discontinuity) drops sync again.
#[test]
fn rds_correction_is_inactive_while_unsynced_and_after_a_gap() {
    let clean = rds_group_bits(RDS_WORDS);
    let one_error = |slot: usize| {
        let mut f = clean.clone();
        f[slot * 26 + 5] ^= 1;
        f
    };
    // Every block carries an error: no three consecutive clean blocks, so never synced.
    let never: Vec<Vec<u8>> = (0..12)
        .map(|i| {
            let mut f = clean.clone();
            for slot in 0..4 {
                f[slot * 26 + 3 + i % 7] ^= 1;
            }
            f
        })
        .collect();
    let mut b = build("crc", rds_params(true), PortType::Frames);
    let out = run_frames_period(b.as_mut(), &rds_frames(&never, 0), 4, false, RDS_PERIOD);
    assert!(out.iter().all(|f| f.info.check == CrcStatus::Invalid));
    assert!(out.iter().all(|f| f.info.corrected_bits == 0));
    assert_eq!(extra(b.as_ref(), "frames_corrected"), 0.0);
    assert_eq!(b.status().lock, crate::status::Lock::Searching);

    // Synced (a clean frame), then a frame that starts two periods later: the gap drops sync, so
    // its single-bit error is not corrected; clean frames re-sync and the next one is corrected.
    let mut b = build("crc", rds_params(true), PortType::Frames);
    let mut input = rds_frames(std::slice::from_ref(&clean), 0);
    input.push(Owned::from_bits(&one_error(0), 1, 2 * RDS_PERIOD as u64));
    input.extend(
        rds_frames(&[clean.clone(), one_error(3)], 3)
            .into_iter()
            .map(|mut f| {
                f.info.source_index += RDS_PERIOD as u64;
                f
            }),
    );
    let out = run_frames_period(b.as_mut(), &input, 1, false, RDS_PERIOD);
    assert_eq!(out[0].info.check, CrcStatus::Valid);
    assert_eq!(
        out[1].info.check,
        CrcStatus::Invalid,
        "a frame after a gap is not corrected"
    );
    assert_eq!(out[2].info.check, CrcStatus::Valid);
    assert_eq!(out[3].info.check, CrcStatus::Corrected);
}

/// The ≤2-bit burst syndrome table of the RDS (26,16) code: every burst has its own syndrome,
/// and weight-3 errors are miscorrected no more often than a code with random syndromes would
/// (51 correctable patterns out of 1023 non-zero syndromes ≈ 4.99 %). This is why the generic
/// `correct_burst_bits` refuses a 10-bit check (5 % ≫ the 1e-3 random-block bound) and why
/// correction is gated on sync instead.
#[test]
fn rds_burst_table_is_unique_and_miscorrects_three_bit_errors_below_the_random_rate() {
    use hk_estimate::framing::crc::BitCrc;

    use super::crc::{false_correction, find_burst, units};
    let crc = BitCrc::new(10, 0x5B9, 0, false, false, 0).unwrap();
    let u = units(&crc, 16, 10, false);
    assert_eq!(u.len(), 26);
    // Unique: each of the 51 bursts is recovered exactly from its own syndrome.
    let mut patterns = Vec::new();
    for st in 0..26 {
        for len in [1usize, 2] {
            if st + len <= 26 {
                patterns.push((st, if len == 1 { 1u32 } else { 3 }));
            }
        }
    }
    assert_eq!(patterns.len(), 51);
    for &(st, mask) in &patterns {
        let s = (0..2)
            .filter(|k| mask >> k & 1 == 1)
            .fold(0, |a, k| a ^ u[st + k]);
        assert_eq!(
            find_burst(&u, 2, |x| x == s),
            Some((st, mask)),
            "burst ({st}, {mask:b})"
        );
    }
    // Weight-3 errors: exhaustive.
    let (mut three, mut miscorrected, mut burst3, mut burst3_bad) = (0usize, 0usize, 0, 0);
    for i in 0..26 {
        for j in i + 1..26 {
            for k in j + 1..26 {
                let s = u[i] ^ u[j] ^ u[k];
                let bad = find_burst(&u, 2, |x| x == s).is_some();
                three += 1;
                miscorrected += usize::from(bad);
                if k - i == 2 {
                    burst3 += 1;
                    burst3_bad += usize::from(bad);
                }
            }
        }
    }
    let rate = miscorrected as f64 / three as f64;
    let random = 51.0 / 1023.0;
    eprintln!(
        "[T-210] weight-3 errors miscorrected {miscorrected}/{three} = {rate:.4} (random-syndrome \
         rate {random:.4}); 3-bit bursts {burst3_bad}/{burst3}"
    );
    assert!(
        rate <= random,
        "weight-3 miscorrection {rate:.4} above the random-syndrome rate {random:.4}"
    );
    assert_eq!(
        burst3_bad, 0,
        "a 3-bit burst is never taken for a ≤2-bit one"
    );
    // The generic bound refuses this table outright (that is what `synced_correction` exists for).
    assert!(false_correction(26, 2, 1, 10) > 1e-3);
}

/// `synced_correction` is block mode only, and its burst is bounded at 2 bits by the schema.
#[test]
fn synced_correction_is_refused_outside_block_mode_and_above_two_bits() {
    let mut span = json!({"width": 24, "poly": "0xFFF409", "strip": false});
    span["synced_correction"] = json!({"burst_bits": 2});
    assert!(try_build("crc", span, PortType::Frames).is_err());
    let mut wide = rds_params(true);
    wide["synced_correction"]["burst_bits"] = json!(3);
    assert!(try_build("crc", wide, PortType::Frames).is_err());
}
