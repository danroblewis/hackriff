//! Synthetic FLEX frames through the decoder (T-950): sync, FIW, BCH correct/detect, interleave
//! and pages, for every mode, with noise, a spectrally inverted path and a transmitter that drops
//! carrier after a few blocks — the shapes the explorer's live capture showed.
//!
//! The generator here is written from the same reading of the format as the decoder, so these
//! tests prove the decoder is self-consistent and robust; that the reading matches the air is
//! what `flex-pagers-930p8` (the SIGNAL-088 acceptance member) checks.

use hk_demod::flex::bch::encode;
use hk_demod::flex::frame::{self, SYNC_MARKER, checksum_ok, interleave};
use hk_demod::flex::{PageKind, decode};
use num_complex::Complex32;

const FS: f64 = 32_000.0;
const OUTER_HZ: f64 = 4_800.0;

/// A field word (BIW / vector) with a valid checksum over `payload`'s bits 4..21.
fn with_checksum(payload: u32) -> u32 {
    let p = payload & 0x1F_FFF0;
    let partial = (1..5).map(|i| (p >> (4 * i)) & 0xF).sum::<u32>() + (p >> 20 & 1);
    let d = p | ((0xF + 64 - partial) & 0xF);
    assert!(checksum_ok(d));
    d
}

/// A tiny deterministic RNG (xorshift) so the noise is reproducible.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
    fn gauss(&mut self) -> f64 {
        let (u, v) = (self.next().max(1e-12), self.next());
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }
}

/// One phase's eleven blocks: `data` first, then the idle alternation, all as codewords.
fn phase_words(data: &[u32]) -> Vec<u32> {
    (0..frame::BLOCKS * 8)
        .map(|i| {
            let d = data
                .get(i)
                .copied()
                .unwrap_or(if i % 2 == 0 { 0 } else { 0x1F_FFFF });
            encode(d)
        })
        .collect()
}

/// A page-carrying phase: a short-address alphanumeric page to `capcode` saying `text`.
fn page_phase(capcode: u32, text: &str) -> Vec<u32> {
    let chars: Vec<u32> = text.bytes().map(u32::from).collect();
    let mut msg = vec![0u32]; // header word: fragment 0
    for c in chars.chunks(3) {
        msg.push(c.iter().enumerate().fold(0, |a, (i, ch)| a | ch << (7 * i)));
    }
    let biw = with_checksum(2 << 10); // aoffset 1, voffset 2
    let viw = with_checksum(5 << 4 | 3 << 7 | (msg.len() as u32) << 14);
    let mut d = vec![biw, capcode + 0x8000, viw];
    d.extend(msg);
    phase_words(&d)
}

/// An empty phase: a BIW with no addresses, then idle.
fn empty_phase() -> Vec<u32> {
    phase_words(&[0x00_040B])
}

struct Scene {
    baud: u32,
    levels: u8,
    code: u16,
    blocks_sent: usize,
    snr_db: f64,
    invert: bool,
    fiw: (u8, u8),
}

/// Symbol frequencies for one frame, then phase-continuous FSK baseband with AWGN.
fn synth(s: &Scene) -> Vec<Complex32> {
    let mut freqs: Vec<(f64, f64)> = Vec::new(); // (freq Hz, duration s)
    let hb = 1.0 / 1600.0;
    // Carrier before the frame, then 32 bits of bit sync.
    freqs.push((OUTER_HZ, 0.05));
    for k in 0..32 {
        freqs.push((if k % 2 == 0 { OUTER_HZ } else { -OUTER_HZ }, hb));
    }
    // Header, marker polarity: 1 = lower frequency.
    let window: u128 = (u128::from(s.code) << 80)
        | (u128::from(SYNC_MARKER) << 48)
        | (u128::from(!s.code) << 32)
        | u128::from((!SYNC_MARKER >> 16) & 0xFFFF) << 16;
    for k in 0..80 {
        let one = window >> (95 - k) & 1 == 1;
        freqs.push((if one { -OUTER_HZ } else { OUTER_HZ }, hb));
    }
    // FIW, data polarity: 1 = higher frequency.
    let fiw_data = with_checksum(u32::from(s.fiw.0) << 4 | u32::from(s.fiw.1) << 8);
    let fiw = encode(fiw_data);
    for j in 0..32 {
        freqs.push((
            if fiw >> j & 1 == 1 {
                OUTER_HZ
            } else {
                -OUTER_HZ
            },
            hb,
        ));
    }
    // Sync-2: 25 ms of alternation at the data rate.
    let db = 1.0 / f64::from(s.baud);
    for k in 0..(0.025 * f64::from(s.baud)) as usize {
        freqs.push((if k % 2 == 0 { OUTER_HZ } else { -OUTER_HZ }, db));
    }
    // Data.
    let a = interleave(&page_phase(1_234_567, "PAGE"));
    let b = interleave(&empty_phase());
    let c = interleave(&page_phase(765_432, "HELLO"));
    let d = interleave(&empty_phase());
    let level = |bit_a: u8, bit_b: u8| -> f64 {
        match (bit_a, bit_b, s.levels) {
            (1, _, 2) => OUTER_HZ,
            (0, _, 2) => -OUTER_HZ,
            (1, 0, _) => OUTER_HZ,
            (1, 1, _) => OUTER_HZ / 3.0,
            (0, 1, _) => -OUTER_HZ / 3.0,
            _ => -OUTER_HZ,
        }
    };
    let n = s.blocks_sent * frame::BLOCK_BITS;
    for i in 0..n {
        if s.baud == 1600 {
            freqs.push((level(a[i], b[i]), db));
        } else {
            freqs.push((level(a[i], b[i]), db));
            freqs.push((level(c[i], d[i]), db));
        }
    }
    let on_air: f64 = freqs.iter().map(|f| f.1).sum();
    let total = on_air + 0.3;
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let noise = 10f64.powf(-s.snr_db / 20.0) / std::f64::consts::SQRT_2;
    let mut out = Vec::with_capacity((total * FS) as usize);
    let (mut phase, mut t, mut idx, mut acc) = (0.0f64, 0.0f64, 0usize, 0.0f64);
    let lead = 0.1; // silence before the carrier
    for n in 0..(total * FS + lead * FS) as usize {
        let time = n as f64 / FS;
        let amp = if time < lead || time - lead >= on_air {
            0.0
        } else {
            let tt = time - lead;
            while idx < freqs.len() && acc + freqs[idx].1 <= tt {
                acc += freqs[idx].1;
                idx += 1;
            }
            let f = freqs.get(idx).map_or(0.0, |f| f.0);
            phase += std::f64::consts::TAU * f / FS;
            1.0
        };
        t += 1.0 / FS;
        let q = if s.invert { -phase } else { phase };
        out.push(Complex32::new(
            (amp * q.cos() + noise * rng.gauss()) as f32,
            (amp * q.sin() + noise * rng.gauss()) as f32,
        ));
    }
    let _ = t;
    out
}

fn scene(code: u16, baud: u32, levels: u8) -> Scene {
    Scene {
        baud,
        levels,
        code,
        blocks_sent: 3,
        snr_db: 18.0,
        invert: false,
        fiw: (7, 42),
    }
}

fn check_scene(s: &Scene) {
    let x = synth(s);
    let r = decode(&x, FS).unwrap();
    assert_eq!(r.frames.len(), 1, "one frame: {:?}", r.frames);
    let f = &r.frames[0];
    assert_eq!(f.inverted, s.invert);
    assert_eq!(f.code, s.code);
    let fiw = f.fiw.expect("the FIW checks");
    assert!(fiw.checksum_ok);
    assert_eq!((fiw.cycle, fiw.frame), s.fiw);
    assert_eq!(f.blocks_on_air, s.blocks_sent, "{:?}", f.hypotheses);
    let m = f.measured.expect("a hypothesis checked");
    assert_eq!(
        (m.baud, m.levels),
        (s.baud, s.levels),
        "measured the sent mode: {:?}",
        f.hypotheses
    );
    assert_eq!(f.mode_agrees(), Some(true));
    let phases = if s.baud == 1600 { 1 } else { 2 } * if s.levels == 4 { 2 } else { 1 };
    assert_eq!(m.live_phases, phases);
    assert_eq!(m.words, phases * s.blocks_sent * 8);
    assert_eq!(
        m.valid_words, m.words,
        "every on-air word checks: {:?}",
        f.phases
    );
    assert_eq!(m.clean_words, m.words, "and cleanly: {:?}", f.phases);
    assert!(f.levels_decisive, "{:?}", f.hypotheses);
    assert!(
        (f.deviation_hz - OUTER_HZ).abs() < 0.15 * OUTER_HZ,
        "deviation {}",
        f.deviation_hz
    );
    let pages: Vec<_> = f.phases.iter().flat_map(|p| &p.pages).collect();
    let a = pages.iter().find(|p| p.phase == 'A').expect("phase A page");
    assert_eq!(a.capcode, 1_234_567);
    assert_eq!(a.kind, PageKind::Alphanumeric);
    assert_eq!(a.text.as_deref(), Some("PAGE"));
    if s.baud == 3200 {
        let c = pages.iter().find(|p| p.phase == 'C').expect("phase C page");
        assert_eq!((c.capcode, c.text.as_deref()), (765_432, Some("HELLO")));
    }
    if s.levels == 4 {
        assert!(
            f.inner_fraction > 0.02,
            "inner pair used: {}",
            f.inner_fraction
        );
    } else {
        assert!(
            f.inner_fraction < 0.02,
            "no inner pair: {}",
            f.inner_fraction
        );
    }
}

#[test]
fn every_mode_is_decoded_and_its_rate_and_levels_are_measured() {
    check_scene(&scene(0x870C, 1600, 2));
    check_scene(&scene(0xB068, 1600, 4));
    check_scene(&scene(0x7B18, 3200, 2));
    check_scene(&scene(0xDEA0, 3200, 4));
}

#[test]
fn a_spectrally_inverted_path_is_decoded() {
    check_scene(&Scene {
        invert: true,
        ..scene(0xDEA0, 3200, 4)
    });
}

#[test]
fn a_full_frame_on_the_air_is_eleven_blocks() {
    check_scene(&Scene {
        blocks_sent: 11,
        ..scene(0xB068, 1600, 4)
    });
}

/// The measurement, not the declaration, decides: a header that declares 4-level over data sent
/// 2-level is measured 2-level and flagged as disagreeing.
#[test]
fn a_declaration_the_data_contradicts_is_measured_and_flagged() {
    let s = Scene {
        code: 0xB068, // declares 1600 Bd 4-level
        ..scene(0xB068, 1600, 2)
    };
    let r = decode(&synth(&s), FS).unwrap();
    let f = &r.frames[0];
    let m = f.measured.unwrap();
    assert_eq!((m.baud, m.levels), (1600, 2), "{:?}", f.hypotheses);
    assert_eq!(f.mode_agrees(), Some(false));
}

#[test]
fn noise_alone_yields_no_frame() {
    let mut rng = Rng(12345);
    let x: Vec<Complex32> = (0..(2.0 * FS) as usize)
        .map(|_| Complex32::new(rng.gauss() as f32, rng.gauss() as f32))
        .collect();
    assert!(decode(&x, FS).unwrap().frames.is_empty());
}

#[test]
fn a_rate_too_low_for_3200_bd_is_refused() {
    assert!(decode(&[Complex32::new(0.0, 0.0); 100], 16_000.0).is_err());
}
