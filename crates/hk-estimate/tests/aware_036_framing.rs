//! AWARE-036 (unknown burst triage), C21 bit-framing inference on bit corpora (T-013).
//!
//! Frames mirror the T-023 `fsk_burst_train` sensor: `1010…` preamble, sync `2DD4`, 48-bit
//! payload (sensor id, seq, temperature, humidity, flags), CRC-16/CCITT-FALSE big endian,
//! wrapped in random receiver bits before and after. Variants: inverted polarity, LSB-first
//! bytes, PN9 whitening, bit errors, an encrypted-looking payload, a length-field protocol, a
//! sync that continues the preamble alternation, and negatives (random bits, a random check
//! field, repeated messages, too few bursts).

use hk_dsp::synth::Rng;
use hk_estimate::framing::bits::{BitOrder, hex, inverted, parse_bit_string, unpack};
use hk_estimate::framing::{
    CrcMethod, CrcParams, Endianness, FramingConfig, FramingReason, FramingStatus, PayloadClass,
    Polarity, SyncAnchor, Whitening, infer_framing,
};

const AWARE_036: &str = "AWARE-036";

fn ccitt_false(data: &[u8]) -> u16 {
    CrcParams {
        width: 16,
        poly: 0x1021,
        init: 0xFFFF,
        refin: false,
        refout: false,
        xorout: 0,
    }
    .compute(data) as u16
}

fn rand_bits(rng: &mut Rng, n: usize) -> Vec<u8> {
    (0..n).map(|_| (rng.next_u64() & 1) as u8).collect()
}

struct Corpus {
    bursts: Vec<Vec<u8>>,
    payloads: Vec<Vec<u8>>,
}

#[derive(Clone, Copy, Default)]
struct Opts {
    invert_all: bool,
    invert_odd: bool,
    lsb_first: bool,
    pn9: bool,
    random_payload: bool,
    random_crc: bool,
    repeat_payload: bool,
}

/// Sensor frames. Bits: [lead random][32 preamble][sync][payload][crc][tail random].
fn sensor_corpus(n: usize, seed: u64, o: Opts) -> Corpus {
    let mut rng = Rng::new(seed);
    let order = if o.lsb_first {
        BitOrder::LsbFirst
    } else {
        BitOrder::MsbFirst
    };
    let mut bursts = Vec::new();
    let mut payloads = Vec::new();
    let mut temp: i32 = 180;
    for k in 0..n {
        temp += (rng.next_u64() % 7) as i32 - 3;
        let payload: Vec<u8> = if o.random_payload {
            (0..8).map(|_| rng.next_u64() as u8).collect()
        } else {
            let seq = if o.repeat_payload { 0 } else { k as u64 };
            let v: u64 = (0x5A3Cu64 << 32)
                | ((seq & 0xFF) << 24)
                | ((temp as u64 & 0xFFF) << 12)
                | (47 << 4)
                | 1;
            v.to_be_bytes()[2..].to_vec()
        };
        let crc = if o.random_crc {
            rng.next_u64() as u16
        } else {
            ccitt_false(&payload)
        };
        let mut body = unpack(&payload, order);
        body.extend(unpack(&crc.to_be_bytes(), order));
        if o.pn9 {
            Whitening::Pn9Cc1101.apply(&mut body);
        }
        let lead = 5 + (rng.next_u64() % 20) as usize;
        let mut bits = rand_bits(&mut rng, lead);
        bits.extend((0..32).map(|i| ((i + 1) % 2) as u8));
        bits.extend(unpack(&[0x2D, 0xD4], order));
        bits.extend(body);
        let tail_len = 3 + (rng.next_u64() % 12) as usize;
        let tail = rand_bits(&mut rng, tail_len);
        bits.extend(tail);
        if o.invert_all || (o.invert_odd && k % 2 == 1) {
            bits = inverted(&bits);
        }
        bursts.push(bits);
        payloads.push(payload);
    }
    Corpus { bursts, payloads }
}

#[test]
fn aware_036_framing_sensor_ccitt_false() {
    let c = sensor_corpus(20, 36, Opts::default());
    let r = infer_framing(&c.bursts, &FramingConfig::default());
    let m = &r.model;
    eprintln!("[{AWARE_036}] {}", serde_json::to_string(m).unwrap());
    assert_eq!(m.status, FramingStatus::Complete, "[{AWARE_036}] {m:?}");
    let pre = m.preamble.as_ref().unwrap();
    assert!((30..=33).contains(&pre.length_bits_median), "{pre:?}");
    let sync = m.sync.as_ref().unwrap();
    assert_eq!(sync.hex.as_deref(), Some("2DD4"));
    assert_eq!(sync.found_in, 20);
    // The alternation-break convention starts one bit early here (…10|0010…); the CRC span
    // re-aligns it.
    assert_eq!(sync.anchor, SyncAnchor::CrcAligned);
    let crc = m.crc.as_ref().unwrap();
    assert_eq!(crc.algorithm, "CRC-16/CCITT-FALSE");
    assert!(crc.catalogue);
    assert_eq!(crc.method, CrcMethod::FixedPositionDifferential);
    assert_eq!(crc.endianness, Endianness::Big);
    assert_eq!(crc.bit_order, BitOrder::MsbFirst);
    assert_eq!(crc.polarity, Polarity::Normal);
    assert_eq!(crc.whitening, None);
    assert_eq!((crc.span.start_bits, crc.span.covered_bits), (0, Some(48)));
    assert_eq!(crc.validate_ratio, 1.0);
    assert!(crc.false_alarm_bound < 1e-30, "{}", crc.false_alarm_bound);
    assert_eq!(m.frame.payload_bits, Some(48));
    assert_eq!(m.frame.frame_bits, Some(16 + 48 + 16));
    assert_eq!(
        m.payload.as_ref().unwrap().class,
        PayloadClass::Structured,
        "{:?}",
        m.payload
    );
    for (i, truth) in c.payloads.iter().enumerate() {
        let p = r.payload(i, &c.bursts[i]).unwrap();
        assert_eq!(p.crc_valid, Some(true));
        assert_eq!(&p.bytes, truth, "[{AWARE_036}] burst {i} payload");
    }
    // The model is structure only: its JSON never holds a payload.
    let json = serde_json::to_string(&r.model).unwrap().to_lowercase();
    for truth in &c.payloads {
        let h: String = truth.iter().map(|b| format!("{b:02x}")).collect();
        assert!(!json.contains(&h[2..]));
    }
}

#[test]
fn aware_036_framing_inverted_polarity_lsb_first_and_mixed() {
    for (name, o, polarity, order) in [
        (
            "inverted",
            Opts {
                invert_all: true,
                ..Default::default()
            },
            Polarity::Inverted,
            BitOrder::MsbFirst,
        ),
        (
            "lsb-first",
            Opts {
                lsb_first: true,
                ..Default::default()
            },
            Polarity::Normal,
            BitOrder::LsbFirst,
        ),
        (
            "mixed per-burst polarity",
            Opts {
                invert_odd: true,
                ..Default::default()
            },
            Polarity::Normal,
            BitOrder::MsbFirst,
        ),
        (
            "inverted lsb-first",
            Opts {
                invert_all: true,
                lsb_first: true,
                ..Default::default()
            },
            Polarity::Inverted,
            BitOrder::LsbFirst,
        ),
    ] {
        let c = sensor_corpus(16, 7, o);
        let r = infer_framing(&c.bursts, &FramingConfig::default());
        let m = &r.model;
        let crc = m
            .crc
            .as_ref()
            .unwrap_or_else(|| panic!("[{AWARE_036}] {name}: no CRC {m:?}"));
        eprintln!(
            "[{AWARE_036}] {name}: {} {:?} {:?} {:?}",
            crc.algorithm, crc.polarity, crc.bit_order, crc.endianness
        );
        // Under the preferred polarity the sync that led the search might be the complement
        // (mixed case: learned from the reference burst); the model reports it corrected.
        assert_eq!(crc.algorithm, "CRC-16/CCITT-FALSE", "{name}");
        assert_eq!(crc.bit_order, order, "{name}");
        assert_eq!(crc.endianness, Endianness::Big, "{name}");
        assert_eq!(
            m.sync.as_ref().unwrap().hex.as_deref(),
            Some("2DD4"),
            "{name}"
        );
        if name != "mixed per-burst polarity" {
            assert_eq!(crc.polarity, polarity, "{name}");
        }
        assert_eq!(crc.validated, 16, "{name}");
        for (i, truth) in c.payloads.iter().enumerate() {
            assert_eq!(
                &r.payload(i, &c.bursts[i]).unwrap().bytes,
                truth,
                "{name} {i}"
            );
        }
    }
}

#[test]
fn aware_036_framing_pn9_whitening_and_bit_errors() {
    let c = sensor_corpus(
        18,
        11,
        Opts {
            pn9: true,
            ..Default::default()
        },
    );
    let r = infer_framing(&c.bursts, &FramingConfig::default());
    let crc = r.model.crc.as_ref().expect("CRC under PN9");
    assert_eq!(crc.algorithm, "CRC-16/CCITT-FALSE");
    assert_eq!(crc.whitening, Some(Whitening::Pn9Cc1101));
    assert_eq!(
        r.model.whitening.as_ref().map(|w| w.name.as_str()),
        Some("pn9-cc1101")
    );
    for (i, truth) in c.payloads.iter().enumerate() {
        assert_eq!(&r.payload(i, &c.bursts[i]).unwrap().bytes, truth);
    }

    // Two bursts with a payload bit error: claimed at 18/20, those frames invalid.
    let mut c = sensor_corpus(20, 12, Opts::default());
    for k in [3usize, 9] {
        let n = c.bursts[k].len();
        c.bursts[k][n - 30] ^= 1;
    }
    let r = infer_framing(&c.bursts, &FramingConfig::default());
    let crc = r.model.crc.as_ref().expect("CRC with errors");
    assert_eq!(crc.algorithm, "CRC-16/CCITT-FALSE");
    assert_eq!(crc.validated, 18);
    assert!((crc.validate_ratio - 0.9).abs() < 1e-9);
    assert_eq!(r.frames[3].crc_valid, Some(false));
    assert_eq!(r.frames[4].crc_valid, Some(true));
}

#[test]
fn aware_036_framing_encrypted_payload_is_labelled() {
    let c = sensor_corpus(
        20,
        13,
        Opts {
            random_payload: true,
            ..Default::default()
        },
    );
    let r = infer_framing(&c.bursts, &FramingConfig::default());
    let m = &r.model;
    assert_eq!(
        m.crc.as_ref().map(|c| c.algorithm.as_str()),
        Some("CRC-16/CCITT-FALSE")
    );
    let p = m.payload.as_ref().unwrap();
    eprintln!("[{AWARE_036}] encrypted-looking: {p:?}");
    assert_eq!(p.class, PayloadClass::EncryptedOrScrambled);
    assert!(p.stopped);
    assert!(m.reasons.contains(&FramingReason::EncryptedOrScrambled));
}

#[test]
fn aware_036_framing_negatives_never_claim_a_crc() {
    let cfg = FramingConfig::default();
    // Pure random bits.
    let mut rng = Rng::new(99);
    let noise: Vec<Vec<u8>> = (0..20).map(|_| rand_bits(&mut rng, 400)).collect();
    let r = infer_framing(&noise, &cfg);
    assert!(r.model.crc.is_none());
    assert!(r.model.sync.is_none(), "{:?}", r.model.sync);
    assert_ne!(r.model.status, FramingStatus::Complete);

    // Preamble + sync + structured payload but a random check field.
    let c = sensor_corpus(
        24,
        5,
        Opts {
            random_crc: true,
            ..Default::default()
        },
    );
    let r = infer_framing(&c.bursts, &cfg);
    eprintln!(
        "[{AWARE_036}] random check field: candidate {:?}",
        r.model.crc_candidate
    );
    assert!(r.model.crc.is_none(), "{:?}", r.model.crc);
    // Without a CRC the word follows the alternation-break convention: one bit before 2DD4.
    let s = r.model.sync.as_ref().unwrap();
    assert_eq!(s.bits, "0001011011101010");
    assert_eq!((s.found_in, s.anchor), (24, SyncAnchor::AlternationBreak));
    assert_eq!(r.model.status, FramingStatus::SyncOnly);
    assert!(r.model.reasons.contains(&FramingReason::NoCrcValidated));

    // Random payload and random check field (random bits after a real sync).
    let c = sensor_corpus(
        24,
        6,
        Opts {
            random_crc: true,
            random_payload: true,
            ..Default::default()
        },
    );
    assert!(infer_framing(&c.bursts, &cfg).model.crc.is_none());

    // The same message repeated: a CRC cannot be told from repetition.
    let c = sensor_corpus(
        12,
        8,
        Opts {
            repeat_payload: true,
            ..Default::default()
        },
    );
    let r = infer_framing(&c.bursts, &cfg);
    // Temperature still varies in this corpus; force true repetition.
    let same: Vec<Vec<u8>> = (0..12).map(|_| c.bursts[0].clone()).collect();
    let r2 = infer_framing(&same, &cfg);
    assert!(r2.model.crc.is_none(), "{:?}", r2.model.crc);
    assert!(
        r2.model
            .reasons
            .contains(&FramingReason::InsufficientMessageVariety),
        "{:?}",
        r2.model.reasons
    );
    let _ = r;

    // Two bursts: insufficient corpus, no claim.
    let c = sensor_corpus(2, 9, Opts::default());
    let r = infer_framing(&c.bursts, &cfg);
    assert!(r.model.crc.is_none());
    assert!(r.model.reasons.contains(&FramingReason::InsufficientCorpus));
}

#[test]
fn aware_036_framing_sync_continuing_the_preamble_and_length_field() {
    // 0101 preamble, sync 0000110001011111 (as the 915 MHz FHSS truth), length byte, payload
    // of 4..=20 bytes, CRC-16/KERMIT little endian over length + payload (802.15.4-like).
    let sync = parse_bit_string("0000110001011111").unwrap();
    let kermit = CrcParams {
        width: 16,
        poly: 0x1021,
        init: 0,
        refin: true,
        refout: true,
        xorout: 0,
    };
    let mut rng = Rng::new(21);
    let mut bursts = Vec::new();
    for _ in 0..20 {
        let len = 4 + (rng.next_u64() % 17) as usize;
        let mut frame = vec![len as u8 + 2];
        frame.extend((0..len).map(|_| rng.next_u64() as u8));
        let crc = kermit.compute(&frame) as u16;
        frame.extend(crc.to_le_bytes());
        let mut bits = rand_bits(&mut rng, 7);
        bits.extend(std::iter::repeat_n([0u8, 1], 24).flatten());
        bits.extend(&sync);
        bits.extend(unpack(&frame, BitOrder::MsbFirst));
        let tail = rand_bits(&mut rng, 4);
        bits.extend(tail);
        bursts.push(bits);
    }
    let r = infer_framing(&bursts, &FramingConfig::default());
    let m = &r.model;
    eprintln!(
        "[{AWARE_036}] length-field model: {}",
        serde_json::to_string(m).unwrap()
    );
    let s = m.sync.as_ref().unwrap();
    assert_eq!(s.bits, "0000110001011111");
    assert_eq!(s.hex.as_deref(), Some("0C5F"));
    assert_eq!(s.found_in, 20);
    let crc = m.crc.as_ref().expect("CRC via length field");
    assert_eq!(crc.algorithm, "CRC-16/KERMIT");
    assert_eq!(crc.method, CrcMethod::LengthField);
    assert_eq!(crc.endianness, Endianness::Little);
    assert_eq!(crc.validated, 20);
    let lf = m.length_field.as_ref().unwrap();
    assert_eq!((lf.offset_bits, lf.width_bits), (0, 8));
    assert_eq!(
        hex(&sync, BitOrder::MsbFirst).as_deref(),
        Some("0C5F"),
        "helper"
    );
}
