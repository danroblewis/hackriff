//! Blind recovery tests (T-091). Generators build synthetic bitstreams/frames with known
//! structure; the assist is never told the answer, and the hidden truth appears only in asserts.

use super::*;
use hk_model::synth::tally::CheckTally;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn bits(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| (self.next() & 1) as u8).collect()
    }
}

fn push(out: &mut Vec<u8>, v: u64, n: usize) {
    for i in (0..n).rev() {
        out.push(((v >> i) & 1) as u8);
    }
}

fn push_lsb_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    for &b in bytes {
        for i in 0..8 {
            out.push((b >> i) & 1);
        }
    }
}

/// Bit-serial CRC (direct register): `(M·x^w + I·x^L) mod G ⊕ X`.
fn crc_bits(bits: &[u8], g_full: u64, w: usize, init: u64, xorout: u64) -> u64 {
    let mask = (1u64 << w) - 1;
    let mut reg = init & mask;
    for &b in bits {
        let top = ((reg >> (w - 1)) & 1) as u8 ^ b;
        reg = (reg << 1) & mask;
        if top == 1 {
            reg ^= g_full & mask;
        }
    }
    reg ^ xorout
}

fn value(bits: &str) -> u64 {
    bits.bytes().fold(0, |a, b| (a << 1) | u64::from(b == b'1'))
}

// --- RDS ----------------------------------------------------------------------------------------

const RDS_G: u64 = 0x5B9;
const RDS_OFFSETS: [u64; 4] = [0x0FC, 0x198, 0x168, 0x1B4];

fn rds_stream(rng: &mut Rng, lead: usize, groups: usize) -> Vec<u8> {
    let mut bits = rng.bits(lead);
    let pi = 0xC201u64;
    let ps = b"HACKRIFF";
    let rt = b"Blind assist: sync, CRC and fields recovered from bits alone....";
    let mut rt_seg = 0;
    for g in 0..groups {
        let tp_pty = (1 << 10) | (10 << 5);
        let (b, c, d) = if g % 3 == 0 {
            let seg = (g / 3) % 4;
            let af = 0xE000 | (0x05 + (g as u64 % 12));
            (
                tp_pty | seg as u64,
                af,
                (u64::from(ps[2 * seg]) << 8) | u64::from(ps[2 * seg + 1]),
            )
        } else {
            let seg = rt_seg % 16;
            rt_seg += 1;
            let ch = |i: usize| u64::from(rt[i]);
            (
                (2 << 12) | tp_pty | seg as u64,
                (ch(4 * seg) << 8) | ch(4 * seg + 1),
                (ch(4 * seg + 2) << 8) | ch(4 * seg + 3),
            )
        };
        for (i, data) in [pi, b, c, d].into_iter().enumerate() {
            let mut blk = Vec::new();
            push(&mut blk, data, 16);
            let check = crc_bits(&blk, RDS_G, 10, 0, 0) ^ RDS_OFFSETS[i];
            push(&mut blk, check, 10);
            bits.extend(blk);
        }
    }
    bits.extend(rng.bits(7));
    bits
}

#[test]
fn assist_rds_block_period_generator_and_offset_words_blind() {
    let mut rng = Rng(0x5EED_0001);
    let lead = 11;
    let bits = rds_stream(&mut rng, lead, 96);
    let r = analyze_stream(&bits, &SyncConfig::default());
    assert!(!r.work.partial, "{:?}", r.work);
    let lb = r
        .periods
        .iter()
        .find(|p| p.method == PeriodMethod::LinearBlock)
        .expect("a linear-block period");
    assert_eq!(lb.period_bits, 26, "{lb:?}");
    assert_eq!(lb.offset_bits, Some(lead % 26), "{lb:?}");
    assert!(
        r.periods
            .iter()
            .any(|p| p.method == PeriodMethod::Autocorrelation && p.period_bits == 104),
        "group period 104 by autocorrelation: {:?}",
        r.periods
    );
    let code = r.block_codes.first().expect("a block code");
    assert_eq!(
        (code.generator, code.width, code.classes),
        (0x5B9, 10, 4),
        "{code:?}"
    );
    let k = &code.class_constants;
    assert!(
        (0..4).any(|rot| (0..4).all(|i| k[(i + rot) % 4] == RDS_OFFSETS[i])),
        "offset words up to rotation: {k:03X?}"
    );
    assert!(r.offset_words.is_some());
    assert_eq!(code.cyclic.as_ref().map(|c| c.n), Some(341));
}

// --- POCSAG -------------------------------------------------------------------------------------

const POCSAG_SYNC: u64 = 0x7CD2_15D8;
const POCSAG_IDLE: u64 = 0x7A89_C197;

fn bch_encode(data21: u64) -> u64 {
    let mut d = Vec::new();
    push(&mut d, data21, 21);
    let cw = (data21 << 11) | (crc_bits(&d, 0x769, 10, 0, 0) << 1);
    cw | u64::from(cw.count_ones() & 1)
}

fn pocsag_stream(rng: &mut Rng, transmissions: usize) -> Vec<u8> {
    let mut bits = rng.bits(50);
    for _ in 0..transmissions {
        for i in 0..576 {
            bits.push(((i + 1) % 2) as u8);
        }
        let ric = rng.below(1 << 21);
        let frame = (ric % 8) as usize;
        let func = rng.below(4);
        let mut words = vec![bch_encode(((ric >> 3) << 2) | func)];
        for _ in 0..(8 + rng.below(20)) {
            words.push(bch_encode((1 << 20) | rng.below(1 << 20)));
        }
        let mut slots = vec![POCSAG_IDLE; 2 * frame];
        slots.extend(words);
        while slots.len() % 16 != 0 {
            slots.push(POCSAG_IDLE);
        }
        for batch in slots.chunks(16) {
            push(&mut bits, POCSAG_SYNC, 32);
            for &w in batch {
                push(&mut bits, w, 32);
            }
        }
        let gap = 100 + rng.below(300) as usize;
        bits.extend(rng.bits(gap));
    }
    bits
}

#[test]
fn assist_pocsag_sync_codeword_period_and_bch_blind() {
    assert_eq!(
        bch_encode(POCSAG_SYNC >> 11),
        POCSAG_SYNC,
        "generator sanity"
    );
    assert_eq!(
        bch_encode(POCSAG_IDLE >> 11),
        POCSAG_IDLE,
        "generator sanity"
    );
    let mut rng = Rng(0x5EED_0002);
    let bits = pocsag_stream(&mut rng, 6);
    let r = analyze_stream(&bits, &SyncConfig::default());
    assert!(!r.work.partial, "{:?}", r.work);
    let sync = r.syncs.first().expect("a sync suggestion");
    assert_eq!(sync.kind, PatternKind::Sync, "{:?}", r.syncs);
    assert_eq!(
        (value(&sync.bits), sync.bit_len),
        (POCSAG_SYNC, 32),
        "{:?}",
        r.syncs
    );
    assert_eq!(sync.modal_interval_bits, Some(544));
    let idle_twice = format!("{0:032b}{0:032b}", POCSAG_IDLE);
    assert!(
        r.syncs.iter().any(|s| s.kind == PatternKind::Fill
            && s.bit_len >= 24
            && idle_twice.contains(&s.bits)),
        "idle word as fill: {:?}",
        r.syncs
    );
    let lb = r.block_period.as_ref().expect("block period");
    assert_eq!(lb.period_bits, 32, "{lb:?}");
    let code = r.block_codes.first().expect("a block code");
    assert_eq!(code.kind, CodeKind::Bch, "{code:?}");
    assert_eq!((code.generator, code.width, code.tail_bits), (0x769, 10, 1));
    assert_eq!(
        code.cyclic.as_ref().and_then(|c| c.name.as_deref()),
        Some("BCH(31,21)")
    );
    match &code.fragment.params {
        FragmentParams::Bch(b) => {
            assert_eq!(
                (b.n, b.k, b.word_bits, b.parity.as_deref()),
                (31, 21, 32, Some("even"))
            );
        }
        other => panic!("{other:?}"),
    }

    // Inverted demodulator: the same word, flagged as the complement.
    let inv: Vec<u8> = bits.iter().map(|b| 1 - b).collect();
    let r = analyze_stream(&inv, &SyncConfig::default());
    let sync = r.syncs.first().expect("a sync suggestion");
    assert!(
        value(&sync.bits) == POCSAG_SYNC || value(&sync.bits) == !POCSAG_SYNC & 0xFFFF_FFFF,
        "{sync:?}"
    );
    assert_eq!(sync.complement_hex, "0x7CD215D8");
}

// --- ADS-B --------------------------------------------------------------------------------------

#[test]
fn assist_adsb_crc24_blind() {
    let mut rng = Rng(0x5EED_0003);
    let icaos: Vec<u64> = (0..6).map(|_| rng.below(1 << 24)).collect();
    let frames: Vec<Vec<u8>> = (0..40)
        .map(|i| {
            let mut f = Vec::new();
            push(&mut f, 17, 5);
            push(&mut f, 5, 3);
            push(&mut f, icaos[i % icaos.len()], 24);
            push(&mut f, rng.next() >> 8, 56);
            let pi = crc_bits(&f, 0x1FF_F409, 24, 0, 0);
            push(&mut f, pi, 24);
            f
        })
        .collect();
    let r = search_codes(&frames, &CodeSearchConfig::default());
    let c = r.codes.first().expect("a code");
    assert_eq!(
        (c.generator, c.width, c.poly),
        (0x1FF_F409, 24, 0xFF_F409),
        "{c:?}"
    );
    assert_eq!((c.start_bit, c.tail_bits, c.init, c.xorout), (0, 0, 0, 0));
    assert!(c.score > 0.9, "{c:?}");
}

// --- ACARS --------------------------------------------------------------------------------------

fn odd_parity(c: u8) -> u8 {
    let c = c & 0x7F;
    if c.count_ones() % 2 == 0 { c | 0x80 } else { c }
}

fn kermit(bytes: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in bytes {
        crc ^= u16::from(b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0x8408
            } else {
                crc >> 1
            };
        }
    }
    crc
}

/// ACARS frames from the mode character through the BCS (what `sync_search` emits), and bursts
/// with lead noise, pre-key and the `+* SYN SYN SOH` sync.
fn acars(rng: &mut Rng, count: usize) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let modes = [b'2', b'A', b'E', b'H'];
    let (mut frames, mut bursts) = (Vec::new(), Vec::new());
    for i in 0..count {
        let mut chars = vec![modes[rng.below(4) as usize]];
        chars.extend(format!(".N{:05}", rng.below(100_000)).bytes());
        chars.push(0x15);
        chars.extend(b"H1");
        chars.push(b'0' + (i % 10) as u8);
        chars.push(0x02);
        let text_len = [10, 20, 30][rng.below(3) as usize];
        chars.extend((0..text_len).map(|_| b' ' + rng.below(90) as u8));
        chars.push(0x03);
        let bytes: Vec<u8> = chars.iter().map(|&c| odd_parity(c)).collect();
        let bcs = kermit(&bytes);
        let mut frame = Vec::new();
        push_lsb_bytes(&mut frame, &bytes);
        push_lsb_bytes(&mut frame, &bcs.to_le_bytes());
        let lead = rng.below(24) as usize;
        let mut burst = rng.bits(lead);
        burst.extend(std::iter::repeat_n(1u8, 64));
        push_lsb_bytes(&mut burst, &[0xAB, 0x2A, 0x16, 0x16, 0x01]);
        burst.extend(&frame);
        push_lsb_bytes(&mut burst, &[0x7F]);
        let trail = rng.below(16) as usize;
        burst.extend(rng.bits(trail));
        frames.push(frame);
        bursts.push(burst);
    }
    (frames, bursts)
}

#[test]
fn assist_acars_crc16_and_lsb_first_sync_blind() {
    let mut rng = Rng(0x5EED_0004);
    let (frames, bursts) = acars(&mut rng, 30);
    let r = search_codes(&frames, &CodeSearchConfig::default());
    let c = r.codes.first().expect("a code");
    assert_eq!((c.generator, c.width), (0x1_1021, 16), "{c:?}");
    assert_eq!((c.init, c.xorout), (0, 0), "{c:?}");
    assert_eq!(
        c.reveng.as_ref().map(|m| m.name.as_str()),
        Some("CRC-16/KERMIT"),
        "{c:?}"
    );
    assert!(
        r.parity.iter().any(|p| p.parity == "odd"
            && matches!(
                p.scope,
                ParityScope::Character {
                    char_bits: 8,
                    phase: 0
                }
            )),
        "{:?}",
        r.parity
    );

    let s = hunt_sync_frames(&bursts, &SyncConfig::default());
    let sync = s.syncs.first().expect("a sync");
    assert_eq!(sync.hex, "0xD554686880", "{:?}", s.syncs);
    assert_eq!(sync.hex_lsb_first.as_deref(), Some("0xAB2A161601"));
    assert_eq!(sync.frames_with, Some(30));
}

// --- Synthetic FSK packet -----------------------------------------------------------------------

#[test]
fn assist_fsk_packet_sync_and_field_boundaries_blind() {
    let mut rng = Rng(0x5EED_0005);
    let crc = |bits: &[u8]| crc_bits(bits, 0x1_1021, 16, 0xFFFF, 0);
    let bursts: Vec<Vec<u8>> = (0..40)
        .map(|i| {
            let lead = rng.below(24) as usize;
            let mut b = rng.bits(lead);
            push(&mut b, 0xAAAA_AAAA, 32);
            push(&mut b, 0x2DD4, 16);
            let len = [4u64, 8, 12, 16][rng.below(4) as usize];
            let mut body = Vec::new();
            push(&mut body, len, 8);
            push(&mut body, 0x5A, 8);
            push(&mut body, (37 + i) & 0xFF, 8);
            for _ in 0..len {
                push(&mut body, rng.below(256), 8);
            }
            let c = crc(&body);
            push(&mut body, c, 16);
            b.extend(body);
            b
        })
        .collect();

    let s = hunt_sync_frames(&bursts, &SyncConfig::default());
    let sync = s.syncs.first().expect("a sync");
    assert_eq!(
        (sync.hex.as_str(), sync.bit_len),
        ("0x2DD4", 16),
        "{:?}",
        s.syncs
    );
    assert!(sync.preamble_bits >= 24, "{sync:?}");

    let pattern: Vec<u8> = sync.bits.bytes().map(|b| u8::from(b == b'1')).collect();
    let aligned = align_on_sync(&bursts, &pattern, sync.max_errors);
    assert_eq!(aligned.len(), 40);
    let f = suggest_fields(&aligned, &FieldsConfig::default());
    assert!(f.field_map_errors.is_empty(), "{:?}", f.field_map_errors);
    let at = |kind: FieldKind, off: usize| {
        f.suggestions
            .iter()
            .find(|s| s.kind == kind && s.bit_offset == off && !s.from_end)
            .unwrap_or_else(|| panic!("{kind:?} at {off}: {:#?}", f.suggestions))
    };
    let len = at(FieldKind::Length, 0);
    assert_eq!((len.bit_len, len.length_scale), (Some(8), Some(8)));
    let addr = at(FieldKind::Constant, 8);
    assert_eq!(
        (addr.bit_len, addr.value_hex.as_deref()),
        (Some(8), Some("0x5A"))
    );
    assert_eq!(at(FieldKind::Counter, 16).bit_len, Some(8));
    assert_eq!(at(FieldKind::HighEntropy, 24).bit_len, None);
    let check = f
        .suggestions
        .iter()
        .find(|s| s.kind == FieldKind::Check)
        .expect("check field");
    assert_eq!((check.bit_len, check.from_end), (Some(16), true));
    let code = f.codes.first().expect("code");
    assert_eq!(
        (code.generator, code.init, code.xorout),
        (0x1_1021, 0xFFFF, 0)
    );
}

// --- Bounded compute ----------------------------------------------------------------------------

// --- Honest scores: few frames, noise, repeats, degenerate input ------------------------------

fn mode_s_frames(rng: &mut Rng, n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|_| {
            let mut f = Vec::new();
            push(&mut f, 17, 5);
            push(&mut f, 5, 3);
            push(&mut f, rng.below(1 << 24), 24);
            push(&mut f, rng.next() >> 8, 56);
            let pi = crc_bits(&f, 0x1FF_F409, 24, 0, 0);
            push(&mut f, pi, 24);
            f
        })
        .collect()
}

#[test]
fn assist_crc_few_frames_never_confidently_wrong() {
    // With 3–4 frames the GCD of the differences often carries a chance factor, so
    // CRC-24 × (small factor) fits as well as CRC-24 itself: never claim it confidently.
    for nf in [3usize, 4] {
        let mut wrong = 0;
        for seed in 0..120u64 {
            let mut rng = Rng(0xABCD_0000 + seed * 7919);
            let frames = mode_s_frames(&mut rng, nf);
            let r = search_codes(&frames, &CodeSearchConfig::default());
            if let Some(c) = r.codes.first()
                && c.generator != 0x1FF_F409
            {
                wrong += 1;
                assert!(c.score < 0.5, "{nf} frames, seed {seed}: {c:?}");
            }
        }
        assert!(
            wrong > 0,
            "{nf} frames: the probe should hit ambiguous cases"
        );
    }
    // Enough frames settle it.
    let mut rng = Rng(0xABCD_0001);
    let r = search_codes(&mode_s_frames(&mut rng, 8), &CodeSearchConfig::default());
    let c = r.codes.first().expect("a code");
    assert_eq!(c.generator, 0x1FF_F409, "{c:?}");
    assert!(c.score > 0.8, "{c:?}");
}

#[test]
fn assist_noise_gives_no_confident_sync() {
    for seed in 0..4u64 {
        let mut rng = Rng(0x7777 + seed);
        let frames: Vec<Vec<u8>> = (0..200).map(|_| rng.bits(112)).collect();
        let s = hunt_sync_frames(&frames, &SyncConfig::default());
        assert!(
            s.syncs.iter().all(|x| x.score < 0.2),
            "frames, seed {seed}: {:?}",
            s.syncs
        );
        let st = analyze_stream(&rng.bits(20_000), &SyncConfig::default());
        assert!(
            st.syncs.iter().all(|x| x.score < 0.2),
            "stream, seed {seed}: {:?}",
            st.syncs
        );
        assert!(st.block_codes.iter().all(|c| c.score < 0.2), "{st:?}");
    }
}

/// Short 902–928 MHz sensor frames with a byte-wide CRC — the canonical ISM shape — must keep
/// their credited differences (T-921 review).
///
/// ADR-0022 §4.3.1's count is taken per candidate width, not once at the cell's widest, because
/// the degenerate guard trims `w` bits off each end and a *wider* trim leaves a *shorter* span,
/// which reads constant or two-period far more often. Trimming a 34-bit frame at 32 leaves two
/// bits, constant half the time; measured on random spans, a max-width trim refuses 75 % of
/// 34-bit, 44 % of 36-bit and 16 % of 40-bit frames that the `crc` block at w = 8 counts. The
/// search would then quietly stop suggesting a real CRC-8 on exactly the frames it exists for.
#[test]
fn assist_short_ism_frames_keep_their_credited_differences() {
    // 8-bit id, 4-bit channel, N-bit reading, CRC-8 — 34, 36 and 40 bits in total.
    for reading in [14usize, 16, 20] {
        let mut rng = Rng(0x5EED_0921 + reading as u64);
        let ids: Vec<u64> = (0..4).map(|_| rng.below(1 << 8)).collect();
        let frames: Vec<Vec<u8>> = (0..24)
            .map(|i| {
                let mut f = Vec::new();
                push(&mut f, ids[i % ids.len()], 8);
                push(&mut f, (i % 4) as u64, 4);
                push(&mut f, rng.below(1 << reading), reading);
                let c = crc_bits(&f, 0x12F, 8, 0, 0);
                push(&mut f, c, 8);
                f
            })
            .collect();
        let len = 20 + reading;
        assert_eq!(frames[0].len(), len);
        let r = search_codes(&frames, &CodeSearchConfig::default());
        let c = r
            .codes
            .iter()
            .find(|c| c.width == 8)
            .unwrap_or_else(|| panic!("a CRC-8 at {len} bits, got {:?}", r.codes));
        assert_eq!(
            (c.generator, c.start_bit, c.tail_bits),
            (0x12F, 0, 0),
            "{c:?}"
        );
        // The invariant, stated against the block path rather than against a guessed number:
        // what a `crc` block of this width would credit over the same spans is what the search
        // must credit, less the one class constant. The guard does refuse some of these frames
        // even at w = 8 — `is_short_periodic` calls an 18-bit span periodic on two matching bit
        // pairs, so a 34-bit frame loses ~43 % — but the search must lose no MORE than the block.
        let mut block = CheckTally::default();
        for f in &frames {
            block.record(f, true, 8.0, 8);
        }
        assert!(
            c.differences as u64 + 1 >= block.independent(),
            "{len} bits: the search credits {} of 24, the crc block at w=8 credits {} — the \
             search must not be the weaker of the two. {c:?}",
            c.differences,
            block.independent()
        );
        assert!(c.score > 0.5, "{len} bits: {c:?}");
    }
}

#[test]
fn assist_repeated_frames_are_not_fresh_evidence() {
    // Alternating all-zero / all-one frames: two distinct frames, whatever the classes.
    let alt: Vec<Vec<u8>> = (0..40).map(|i| vec![(i % 2) as u8; 32]).collect();
    let r = search_codes(&alt, &CodeSearchConfig::default());
    assert!(r.codes.iter().all(|c| c.score < 0.2), "{:?}", r.codes);
    // Three random frames repeated: three distinct frames.
    let mut rng = Rng(0x4242);
    let three: Vec<Vec<u8>> = (0..3).map(|_| rng.bits(64)).collect();
    let rep: Vec<Vec<u8>> = (0..48).map(|i| three[i % 3].clone()).collect();
    let r = search_codes(&rep, &CodeSearchConfig::default());
    assert!(r.codes.iter().all(|c| c.score < 0.2), "{:?}", r.codes);
    // Distinct but degenerate frames (constant and short-period patterns): their differences
    // are structured, not random multiples of a generator.
    let periodic: Vec<Vec<u8>> = (0..24)
        .map(|i| {
            let period = 1 + i % 8;
            let phase = i / 8;
            (0..32)
                .map(|b| u8::from((b + phase) % period == 0))
                .collect()
        })
        .collect();
    let r = search_codes(&periodic, &CodeSearchConfig::default());
    assert!(r.codes.iter().all(|c| c.score < 0.2), "{:?}", r.codes);
}

#[test]
fn assist_random_frames_give_no_confident_fields() {
    for (n, len) in [(200usize, 112usize), (5, 64), (8, 48)] {
        for seed in 0..4u64 {
            let mut rng = Rng(0x5151 + seed);
            let frames: Vec<Vec<u8>> = (0..n).map(|_| rng.bits(len)).collect();
            let f = suggest_fields(&frames, &FieldsConfig::default());
            for s in &f.suggestions {
                let structural = !matches!(s.kind, FieldKind::HighEntropy | FieldKind::Mixed);
                assert!(
                    !structural || s.score < 0.5,
                    "{n}×{len}, seed {seed}: {s:?}"
                );
            }
        }
    }
    // Frames past the classified length answer nothing, and say why.
    let mut rng = Rng(0x5152);
    let huge: Vec<Vec<u8>> = (0..6).map(|_| rng.bits(MAX_FIELD_BITS + 8)).collect();
    let f = suggest_fields(&huge, &FieldsConfig::default());
    assert!(f.suggestions.is_empty());
    assert!(
        f.work.skipped.iter().any(|s| s.contains("longer than")),
        "{:?}",
        f.work
    );
}

#[test]
fn assist_alignment_is_metered() {
    let mut rng = Rng(0x5153);
    let frames: Vec<Vec<u8>> = (0..400)
        .map(|_| {
            let mut f = rng.bits(40);
            push(&mut f, 0x2DD4, 16);
            f.extend(rng.bits(64));
            f
        })
        .collect();
    let cfg = FieldsConfig {
        align: Some(SyncAlign {
            sync: (0..16).rev().map(|i| ((0x2DD4 >> i) & 1) as u8).collect(),
            max_errors: 1,
        }),
        budget: Budget { max_ops: 10_000 },
        ..FieldsConfig::default()
    };
    let f = suggest_fields(&frames, &cfg);
    assert!(f.work.partial && f.frames < 400, "{:?}", f.work);
    assert!(f.work.skipped.iter().any(|s| s.contains("alignment")));
}

/// Release timing of heavy calls at the default and the maximum work cap (op charges should be
/// ≈ 1 ns each): `cargo test --release -p hk-estimate --lib assist_bench -- --ignored --nocapture`.
#[test]
#[ignore = "timing bench, release builds"]
fn assist_bench_work_cap_timing() {
    use std::time::Instant;
    let mut rng = Rng(0x9999);
    let long: Vec<Vec<u8>> = (0..100).map(|_| rng.bits(4000)).collect();
    let short: Vec<Vec<u8>> = (0..2000).map(|_| rng.bits(112)).collect();
    let tiny: Vec<Vec<u8>> = (0..20_000).map(|_| rng.bits(20)).collect();
    let stream = rng.bits(400_000);
    let rds = rds_stream(&mut rng, 5, 3800);
    for (label, max_ops) in [("default", DEFAULT_MAX_OPS), ("max", MAX_OPS)] {
        let budget = Budget { max_ops };
        let wide = CodeSearchConfig {
            max_tail_bits: 256,
            max_classes: 16,
            budget,
            ..CodeSearchConfig::default()
        };
        let sync = SyncConfig {
            budget,
            codes: CodeSearchConfig {
                budget,
                ..CodeSearchConfig::default()
            },
            max_block_bits: 128,
            max_lag: 65_536,
            ..SyncConfig::default()
        };
        let fields = FieldsConfig {
            budget,
            codes: CodeSearchConfig {
                max_classes: 1,
                max_tail_bits: 256,
                budget,
                ..CodeSearchConfig::default()
            },
            ..FieldsConfig::default()
        };
        type Case<'a> = (&'a str, Box<dyn Fn() -> WorkReport + 'a>);
        let cases: Vec<Case> = vec![
            (
                "crc 100×4000 tails≤256",
                Box::new(|| search_codes(&long, &wide).work),
            ),
            (
                "crc 2000×112 tails≤256",
                Box::new(|| search_codes(&short, &wide).work),
            ),
            ("crc 20000×20", Box::new(|| search_codes(&tiny, &wide).work)),
            (
                "sync frames 2000×112",
                Box::new(|| hunt_sync_frames(&short, &sync).work),
            ),
            (
                "sync stream 400k noise",
                Box::new(|| analyze_stream(&stream, &sync).work),
            ),
            (
                "sync stream rds",
                Box::new(|| analyze_stream(&rds, &sync).work),
            ),
            (
                "fields 100×4000",
                Box::new(|| suggest_fields(&long, &fields).work),
            ),
            (
                "fields 20000×20",
                Box::new(|| suggest_fields(&tiny, &fields).work),
            ),
        ];
        for (name, run) in cases {
            let t0 = Instant::now();
            let w = run();
            let s = t0.elapsed().as_secs_f64();
            eprintln!(
                "BENCH {label:7} {name:26} {:.3} s  ops {:>10}  partial {:5}  {:.2} ns/op",
                s,
                w.ops,
                w.partial,
                s * 1e9 / w.ops.max(1) as f64
            );
        }
    }
}

#[test]
fn assist_work_cap_returns_a_partial_result() {
    let mut rng = Rng(0x5EED_0006);
    let bits = pocsag_stream(&mut rng, 2);
    let cfg = SyncConfig {
        budget: Budget { max_ops: 20_000 },
        ..SyncConfig::default()
    };
    let r = analyze_stream(&bits, &cfg);
    assert!(r.work.partial, "{:?}", r.work);
    assert!(!r.work.skipped.is_empty());
}

const MODE_S_G: u64 = 0x1FF_F409;

fn mode_s_search(seed: u64, nf: usize) -> CodeReport {
    let mut rng = Rng(0xABCD_0000 + seed * 7919);
    search_codes(&mode_s_frames(&mut rng, nf), &CodeSearchConfig::default())
}

#[test]
fn assist_crc_many_frames_never_confidently_wrong() {
    // T-105: CRC-24/Mode-S already has the factor (x+1). With 8 frames, 1 draw in 128 gives every
    // difference one more (x+1), and (x+1)·CRC-24 fits as the largest generator. It has a
    // repeated factor, so it must not win confidently.
    for nf in [8usize, 16] {
        let mut right = 0;
        for seed in 0..200u64 {
            let r = mode_s_search(seed, nf);
            let c = r.codes.first().expect("a code");
            if c.generator == MODE_S_G {
                right += 1;
                assert!(c.score > 0.8, "{nf} frames, seed {seed}: {c:?}");
            } else {
                assert!(c.score < 0.5, "{nf} frames, seed {seed}: {:?}", r.codes);
            }
        }
        assert!(right >= 197, "{nf} frames: {right}/200 right");
    }
    // Seed 34: every one of the 7 differences carries the extra (x+1). The squared generator
    // stays only as a low-scored alternative, named in the group.
    let r = mode_s_search(34, 8);
    let (c, alt) = (&r.codes[0], 0x200_1C1Bu64);
    assert_eq!(
        (c.generator, c.ambiguous_with.as_slice()),
        (MODE_S_G, &[alt][..]),
        "{:?}",
        r.codes
    );
    let a = r
        .codes
        .iter()
        .find(|x| x.generator == alt)
        .expect("alternative");
    assert!(a.score < 0.1 && a.ambiguous_with == [MODE_S_G], "{a:?}");
    assert!(
        a.reasons.iter().any(|s| s.contains("repeated factor")),
        "{a:?}"
    );
}

#[test]
fn assist_crc_few_frames_ambiguity_is_explicit() {
    // T-105: with 3–4 frames the generators fitting one hypothesis are near-ties. They are ranked
    // by posterior share (no longer smallest-divisor-first), name each other in
    // `ambiguous_with`, and a wrong top scores low.
    for (nf, min_right) in [(3usize, 150), (4, 190)] {
        let mut right = 0;
        for seed in 0..200u64 {
            let r = mode_s_search(seed, nf);
            for c in &r.codes {
                let mates: Vec<u64> = r
                    .codes
                    .iter()
                    .filter(|x| {
                        (x.start_bit, x.tail_bits, &x.bit_order, x.classes)
                            == (c.start_bit, c.tail_bits, &c.bit_order, c.classes)
                            && x.generator != c.generator
                    })
                    .map(|x| x.generator)
                    .collect();
                let mut named = c.ambiguous_with.clone();
                named.sort_unstable();
                let mut mates = mates;
                mates.sort_unstable();
                assert_eq!(named, mates, "{nf} frames, seed {seed}: {c:?}");
            }
            let c = r.codes.first().expect("a code");
            if c.generator == MODE_S_G {
                right += 1;
            } else {
                assert!(c.score < 0.4, "{nf} frames, seed {seed}: {:?}", r.codes);
            }
        }
        assert!(right >= min_right, "{nf} frames: {right}/200 right");
    }
}
