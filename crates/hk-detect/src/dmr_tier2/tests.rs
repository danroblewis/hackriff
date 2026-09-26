//! Unit tests for conventional DMR identification (T-989).
//!
//! The two kinds here are deliberate. **Arithmetic tests** check the constants against relations a
//! mis-recollection would break (the outer-symbol property, the complement pairs, the widths that
//! have to sum). **Round-trip tests** build a burst with [`super::encode`] and read it back with
//! [`super::scan`], so the framing, both Hamming codes, the interleave, the Golay slot type and
//! every header check are exercised end to end against something that was assembled field by
//! field rather than recorded from this decoder's own output.

use super::encode::{
    Filler, data_burst, data_header, repeater_downlink, slot, terminator_with_lc, voice_burst,
    voice_lc_header,
};
use super::*;

/// Deterministic pseudo-random dibits — the noise every gate here has to survive.
fn noise_dibits(n: usize, seed: u64) -> Vec<u8> {
    let mut s = seed | 1;
    (0..n)
        .map(|_| {
            s = s
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1442695040888963407);
            (s >> 60 & 0b11) as u8
        })
        .collect()
}

#[test]
fn every_sync_word_is_all_outer_symbols() {
    // The documented property: under the 4FSK dibit map every sync symbol is +/-3, i.e. every
    // dibit is 1 or 3, i.e. every hex digit is one of 5, 7, D, F. A typo breaks it 3 times in 4.
    for s in DMR_SYNCS {
        assert!(
            s.dibits.iter().all(|&d| d == 1 || d == 3),
            "{}: {:012X} has an inner symbol",
            s.name,
            s.hex
        );
        assert_eq!(sync_dibits(s.hex), s.dibits, "{}: dibits", s.name);
    }
}

#[test]
fn voice_and_data_syncs_are_exact_dibit_complements() {
    for (voice, data) in [
        ("bs-voice", "bs-data"),
        ("ms-voice", "ms-data"),
        ("direct-ts1-voice", "direct-ts1-data"),
        ("direct-ts2-voice", "direct-ts2-data"),
    ] {
        let v = DMR_SYNCS.iter().find(|s| s.name == voice).unwrap();
        let d = DMR_SYNCS.iter().find(|s| s.name == data).unwrap();
        assert_eq!(
            v.hex ^ SYNC_COMPLEMENT,
            d.hex,
            "{voice} is not the dibit complement of {data}"
        );
    }
    // Every sync word is distinct, and no two are within the search tolerance of each other, so a
    // hit names one pattern rather than a family of near-misses.
    for (i, a) in DMR_SYNCS.iter().enumerate() {
        for b in DMR_SYNCS.iter().skip(i + 1) {
            let d = a
                .dibits
                .iter()
                .zip(b.dibits.iter())
                .filter(|(x, y)| x != y)
                .count();
            assert!(
                d > 2 * SYNC_TOLERANCE_DIBITS,
                "{} and {} differ in only {d} dibits",
                a.name,
                b.name
            );
        }
    }
}

#[test]
fn the_t271_constants_are_the_same_two_sync_words() {
    // `trunk::dmr` verified the base-station pair independently; this module must not have drifted
    // from it.
    assert_eq!(DMR_SYNCS[1].dibits, DMR_BS_DATA_SYNC_DIBITS);
    assert_eq!(DMR_SYNCS[0].dibits, DMR_BS_VOICE_SYNC_DIBITS);
}

#[test]
fn the_burst_widths_sum_to_the_air_interface() {
    assert_eq!(INFO_DIBITS * 2, 98);
    assert_eq!(SLOT_TYPE_HALF_DIBITS * 2 * 2, SLOT_TYPE_BITS);
    assert_eq!(
        2 * (INFO_DIBITS + SLOT_TYPE_HALF_DIBITS) + SYNC_DIBITS,
        BURST_DIBITS
    );
    assert_eq!(BURST_DIBITS * 2, 264);
    assert_eq!((CACH_DIBITS + BURST_DIBITS) * 2, 288);
    // 144 symbols at 4800 Bd is exactly 30 ms, which is what makes a slot a slot.
    assert!((SLOT_GRID_DIBITS as f64 / DMR_SYMBOL_RATE_BD - 0.030).abs() < 1e-12);
    assert_eq!(INFO_DIBITS * 2 * 2, fec::BPTC_BITS);
}

#[test]
fn the_cach_interleave_is_a_permutation() {
    let mut seen = [false; CACH_BITS];
    for &p in &CACH_INTERLEAVE {
        assert!(!seen[p], "position {p} twice");
        seen[p] = true;
    }
    assert!(seen.iter().all(|&s| s));
}

#[test]
fn a_slot_type_round_trips_and_survives_three_bit_errors() {
    for cc in 0..16u8 {
        for dt in [
            DataType::VoiceLcHeader,
            DataType::TerminatorWithLc,
            DataType::Csbk,
            DataType::DataHeader,
            DataType::Idle,
            DataType::Unnamed(12),
        ] {
            let bits = slot_type_encode(cc, dt);
            let got = slot_type_decode(&bits).expect("clean slot type");
            assert_eq!((got.colour_code, got.data_type), (cc, dt));
            let mut bad = bits;
            for b in [0usize, 9, 19] {
                bad[b] ^= 1;
            }
            let fixed = slot_type_decode(&bad).expect("three errors are inside the radius");
            assert_eq!(
                (fixed.colour_code, fixed.data_type, fixed.corrected),
                (cc, dt, 3)
            );
        }
    }
}

#[test]
fn a_cach_round_trips() {
    let short_lc = [1u8, 0, 1, 1, 0, 0, 0, 1, 1, 1, 0, 1, 0, 0, 1, 1, 0];
    for tc in 0..2u8 {
        for lcss in 0..4u8 {
            let bits = cach_encode(1, tc, lcss, &short_lc);
            let got = cach_decode(&bits).expect("clean cach");
            assert_eq!((got.access_type, got.tdma_channel, got.lcss), (1, tc, lcss));
            assert!(!got.corrected);
            assert_eq!(got.short_lc, short_lc);
            // One bit anywhere in the TACT is corrected; the interleave means that is one bit
            // anywhere in the first 24 transmitted bits.
            for p in CACH_INTERLEAVE.iter().take(7) {
                let mut bad = bits;
                bad[*p] ^= 1;
                let got = cach_decode(&bad).expect("one TACT bit is correctable");
                assert_eq!((got.access_type, got.tdma_channel, got.lcss), (1, tc, lcss));
                assert!(got.corrected);
            }
        }
    }
}

#[test]
fn a_voice_lc_header_burst_decodes_to_the_call_it_states() {
    let mut filler = Filler::new(7);
    let payload = voice_lc_header(0, 0x00_2A_F8, 0x12_34_56);
    let burst = data_burst(1, 5, DataType::VoiceLcHeader, &payload);
    let mut dibits = slot(0, 0, &burst, &mut filler);
    // A second burst, so the scan reaches its two-sync floor and its colour-code agreement.
    dibits.extend(slot(1, 0, &burst, &mut filler));
    let scan = scan(&dibits);
    assert!(scan.identified(), "two syncs is DMR");
    assert_eq!(scan.hits.len(), 2);
    assert_eq!(scan.grid_consistent, 2);
    assert_eq!(scan.colour_code, Some(5));
    assert_eq!(
        scan.verdict().as_deref(),
        Some("DMR Tier II, CC 5, sync 2/2")
    );
    let b = &scan.bursts[0];
    assert_eq!(b.word().name, "bs-data");
    assert_eq!(b.cach.map(|c| c.tdma_channel), Some(0));
    assert_eq!(
        b.slot_type.map(|s| s.data_type),
        Some(DataType::VoiceLcHeader)
    );
    match b.header {
        Some(Header::VoiceLc(lc)) => {
            assert_eq!(
                (lc.flco, lc.destination, lc.source),
                (0, 0x00_2A_F8, 0x12_34_56)
            );
        }
        other => panic!("expected a voice LC header, got {other:?}"),
    }
    assert_eq!(scan.bptc_failed, 0);
    assert_eq!(scan.check_failed, 0);
}

#[test]
fn every_header_this_build_reads_round_trips() {
    let mut filler = Filler::new(11);
    let blocks: [([u8; 12], DataType); 3] = [
        (voice_lc_header(3, 0x11, 0x22), DataType::VoiceLcHeader),
        (
            terminator_with_lc(0, 0x33, 0x44),
            DataType::TerminatorWithLc,
        ),
        (data_header(true, 0x55, 0x66, 4), DataType::DataHeader),
    ];
    let mut dibits = Vec::new();
    for (payload, dt) in blocks {
        let burst = data_burst(1, 9, dt, &payload);
        dibits.extend(slot(0, 0, &burst, &mut filler));
    }
    let scan = scan(&dibits);
    assert_eq!(scan.hits.len(), 3);
    assert_eq!(scan.colour_code, Some(9));
    let names: Vec<&str> = scan.headers().map(|h| h.name()).collect();
    assert_eq!(
        names,
        ["voice-lc-header", "terminator-with-lc", "data-header"]
    );
    match scan.bursts[2].header {
        Some(Header::Data(h)) => {
            assert_eq!(
                (h.destination, h.source, h.blocks_to_follow, h.group),
                (0x55, 0x66, 4, true)
            );
        }
        other => panic!("expected a data header, got {other:?}"),
    }
}

#[test]
fn a_csbk_reaches_the_tier_iii_parser_through_real_framing() {
    // The payoff of decoding the burst properly: T-271's CSBK parser, fed by BPTC rather than by
    // its own flattened framing.
    let mut filler = Filler::new(13);
    let mut block = [0u8; 12];
    block[0] = crate::trunk::dmr::CSBKO_C_ALOHA;
    block[1] = 0;
    for (i, b) in block.iter_mut().enumerate().take(10).skip(2) {
        *b = (i as u8) * 17;
    }
    let crc = crate::trunk::dmr::csbk_crc().compute(&block[..10], 0, 80) as u16;
    block[10] = (crc >> 8) as u8;
    block[11] = crc as u8;
    assert!(crate::trunk::dmr::csbk_crc_ok(&block));
    let burst = data_burst(1, 1, DataType::Csbk, &block);
    let mut dibits = slot(0, 0, &burst, &mut filler);
    dibits.extend(slot(1, 0, &burst, &mut filler));
    let scan = scan(&dibits);
    match scan.bursts[0].header {
        Some(Header::Csbk(c)) => assert_eq!(c.csbko, crate::trunk::dmr::CSBKO_C_ALOHA),
        other => panic!("expected a CSBK, got {other:?}"),
    }
}

#[test]
fn a_privacy_header_is_reported_and_nothing_is_read_out_of_it() {
    let mut filler = Filler::new(17);
    let key_material = [0xDEu8, 0xAD, 0xBE, 0xEF, 0, 1, 2, 3, 4, 5, 6, 7];
    let burst = data_burst(1, 2, DataType::PiHeader, &key_material);
    let mut dibits = slot(0, 0, &burst, &mut filler);
    dibits.extend(slot(1, 0, &burst, &mut filler));
    let scan = scan(&dibits);
    assert!(scan.headers().any(|h| *h == Header::PrivacyIndicator));
    // The variant carries no payload at all: there is nowhere for key material to go.
    assert_eq!(
        scan.headers()
            .filter(|h| **h == Header::PrivacyIndicator)
            .count(),
        2
    );
}

#[test]
fn a_repeater_downlink_is_identified_with_its_colour_code_and_call() {
    let dibits = repeater_downlink(40, 3, 0x00_10_01, 0x00_20_02);
    let scan = scan(&dibits);
    assert!(scan.identified());
    assert_eq!(scan.slots, 40);
    assert_eq!(scan.colour_code, Some(3));
    assert!(scan.colour_code_disagreements == 0, "{scan:?}");
    let verdict = scan.verdict().expect("identified");
    assert!(
        verdict.starts_with("DMR Tier II, CC 3, sync "),
        "verdict: {verdict}"
    );
    // Both base-station sync words are on the air: data bursts and the voice superframe's burst A.
    let names = scan.sync_names();
    assert!(
        names.iter().any(|(n, c)| *n == "bs-data" && *c >= 20),
        "{names:?}"
    );
    assert!(names.iter().any(|(n, _)| *n == "bs-voice"), "{names:?}");
    let lc = scan
        .headers()
        .find_map(|h| match h {
            Header::VoiceLc(lc) => Some(*lc),
            _ => None,
        })
        .expect("the call's link control");
    assert_eq!((lc.destination, lc.source), (0x00_10_01, 0x00_20_02));
    assert!(scan.headers().any(|h| matches!(h, Header::TerminatorLc(_))));
    assert_eq!(scan.bptc_failed, 0);
    assert_eq!(scan.check_failed, 0);
}

#[test]
fn identification_survives_the_sync_tolerance_and_stops_past_it() {
    let mut filler = Filler::new(19);
    let payload = voice_lc_header(0, 1, 2);
    let burst = data_burst(1, 4, DataType::VoiceLcHeader, &payload);
    let mut dibits = slot(0, 0, &burst, &mut filler);
    dibits.extend(slot(1, 0, &burst, &mut filler));
    let sync_at: Vec<usize> = scan(&dibits).hits.iter().map(|h| h.dibit).collect();
    assert_eq!(sync_at.len(), 2);
    // Two wrong dibits per sync: still found, and the burst behind it still decodes.
    let mut damaged = dibits.clone();
    for &p in &sync_at {
        damaged[p] ^= 0b10;
        damaged[p + 7] ^= 0b10;
    }
    let s = scan(&damaged);
    assert!(s.identified());
    assert!(s.hits.iter().all(|h| h.errors == 2));
    assert_eq!(s.colour_code, Some(4));
    // Three wrong dibits is past the a-priori tolerance: no hit, so no identification.
    let mut past = dibits;
    for &p in &sync_at {
        past[p] ^= 0b10;
        past[p + 7] ^= 0b10;
        past[p + 13] ^= 0b10;
    }
    assert!(!scan(&past).identified());
}

#[test]
fn noise_is_not_dmr() {
    // 200 000 random dibits is ~170 times the window a classification runs on, and the expected
    // number of false sync hits over it is ~2e-5 (module docs). Several seeds, so the claim is
    // about the floor and not about one draw.
    for seed in [1u64, 2, 3, 4, 5] {
        let s = scan(&noise_dibits(200_000, seed));
        assert!(!s.identified(), "seed {seed}: {} hits", s.hits.len());
        assert!(s.verdict().is_none());
    }
}

#[test]
fn a_corrupted_burst_is_counted_and_never_guessed_at() {
    let mut filler = Filler::new(23);
    let payload = voice_lc_header(0, 0x77, 0x88);
    let burst = data_burst(1, 6, DataType::VoiceLcHeader, &payload);
    let mut dibits = slot(0, 0, &burst, &mut filler);
    dibits.extend(slot(1, 0, &burst, &mut filler));
    // Wreck the first burst's information bits, leaving its sync and slot type intact.
    let first = scan(&dibits).bursts[0].dibit;
    let mut damaged = dibits;
    for i in 0..INFO_DIBITS {
        damaged[first + i] ^= 0b11;
    }
    let s = scan(&damaged);
    assert!(s.identified(), "the sync is untouched, so it is still DMR");
    assert_eq!(
        s.bptc_failed, 1,
        "the wrecked block is refused, not decoded"
    );
    // The surviving burst still states the call: a refusal costs one header, not the emission.
    let lc: Vec<FullLc> = s
        .headers()
        .filter_map(|h| match h {
            Header::VoiceLc(lc) => Some(*lc),
            _ => None,
        })
        .collect();
    assert_eq!(lc.len(), 1);
    assert_eq!((lc[0].destination, lc[0].source), (0x77, 0x88));
}

#[test]
fn a_header_whose_check_fails_is_counted_and_not_published() {
    let mut filler = Filler::new(29);
    // A block with the right data type and a BPTC that resolves, but parity that is not the LC's.
    let bogus = [0x11u8; 12];
    let burst = data_burst(1, 7, DataType::VoiceLcHeader, &bogus);
    let mut dibits = slot(0, 0, &burst, &mut filler);
    dibits.extend(slot(1, 0, &burst, &mut filler));
    let s = scan(&dibits);
    assert!(s.identified());
    assert_eq!(s.bptc_failed, 0, "the block resolved");
    assert_eq!(s.check_failed, 2, "and then its RS parity refused it");
    assert_eq!(s.headers().count(), 0);
}

#[test]
fn a_voice_only_burst_carries_no_slot_type() {
    let mut filler = Filler::new(31);
    let mut dibits = Vec::new();
    for _ in 0..4 {
        let burst = voice_burst(Some(0), &mut filler);
        dibits.extend(slot(0, 0, &burst, &mut filler));
    }
    let s = scan(&dibits);
    assert!(s.identified());
    assert_eq!(s.hits.len(), 4);
    assert!(s.bursts.iter().all(|b| b.slot_type.is_none()));
    assert!(s.bursts.iter().all(|b| b.header.is_none()));
    // No colour code was decoded, and the verdict says so rather than inventing one.
    assert_eq!(s.colour_code, None);
    assert_eq!(
        s.verdict().as_deref(),
        Some("DMR Tier II, CC unknown, sync 4/4")
    );
}
