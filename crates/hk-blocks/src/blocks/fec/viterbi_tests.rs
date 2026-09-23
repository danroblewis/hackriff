//! T-610: `viterbi` and `viterbi_frames` (ADR-0011 §9.3). Every test builds the blocks through
//! the registry, so each goes red if the block is not registered.

use hk_recipe::PortType;
use serde_json::{Value, json};

use super::trellis::{Code, encode};
use crate::block::{Block, Io, PortInfo};
use crate::blocks::framing::common::P;
use crate::blocks::framing::common::testutil::{Owned, build, run_frames, try_build};
use crate::buffer::{ChunkFlags, ChunkMeta, Input, Output, PortSlice, PortVec};
use crate::status::{Lock, Status};

// ---- fixtures -------------------------------------------------------------------------------

/// CCSDS 131.0-B-4 §3.3 (figure 3-1): K = 7, G1 = 171, G2 = 133 (octal, newest input = MSB), G2
/// inverted, C1 sent first. In the `newest-lsb` form a recipe uses: 0x4F, 0x6D.
fn ccsds() -> Value {
    json!({ "constraint_length": 7, "polys": ["0x4F", "0x6D"], "invert": [false, true] })
}

fn with(base: Value, extra: Value) -> Value {
    let mut b = base.as_object().unwrap().clone();
    b.extend(extra.as_object().unwrap().clone());
    Value::Object(b)
}

fn code_of(params: &Value) -> Code {
    Code::from_params(P(params.as_object().unwrap())).unwrap()
}

/// xorshift64* + Box–Muller: deterministic data and noise.
struct Rng(u64);

impl Rng {
    fn u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn unit(&mut self) -> f64 {
        ((self.u64() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }
    fn bits(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| (self.u64() >> 40) as u8 & 1).collect()
    }
    fn gauss(&mut self) -> f64 {
        let (u, v) = (self.unit(), self.unit());
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }
}

/// BPSK over AWGN at `ebn0_db` for a code of `rate` (input bits per transmitted bit): soft
/// values, positive = 1 (unit amplitude; the scale is irrelevant to the decoder).
fn awgn(coded: &[u8], rate: f64, ebn0_db: f64, rng: &mut Rng) -> Vec<f32> {
    let ebn0 = 10f64.powf(ebn0_db / 10.0);
    let sigma = (1.0 / (2.0 * rate * ebn0)).sqrt();
    coded
        .iter()
        .map(|&b| (if b == 1 { 1.0 } else { -1.0 } + sigma * rng.gauss()) as f32)
        .collect()
}

fn soft_of(bits: &[u8]) -> Vec<f32> {
    bits.iter()
        .map(|&b| if b == 1 { 1.0 } else { -1.0 })
        .collect()
}

// ---- streaming harness ----------------------------------------------------------------------

/// An input stream: soft values or hard bits.
enum Stream {
    Soft(Vec<f32>),
    Bits(Vec<u8>),
}

impl Stream {
    fn len(&self) -> usize {
        match self {
            Stream::Soft(v) => v.len(),
            Stream::Bits(v) => v.len(),
        }
    }
    fn ty(&self) -> PortType {
        match self {
            Stream::Soft(_) => PortType::Soft,
            Stream::Bits(_) => PortType::Bits,
        }
    }
    fn slice(&self, a: usize, b: usize) -> PortSlice<'_> {
        match self {
            Stream::Soft(v) => PortSlice::Soft(&v[a..b]),
            Stream::Bits(v) => PortSlice::Bits(&v[a..b]),
        }
    }
}

/// What a streaming run produced.
#[derive(Debug, Default)]
struct Run {
    bits: Vec<u8>,
    /// Source index of every output bit, from its chunk's time map.
    source: Vec<f64>,
    /// Output bit indexes at which a chunk flagged DISCONTINUITY began.
    disc: Vec<usize>,
    status: Status,
}

const PER_ITEM: f64 = 10.0;

fn run(params: Value, input: &Stream, chunk: usize, end: bool) -> Run {
    let mut block = build("viterbi", params, input.ty());
    run_block(block.as_mut(), input, chunk, end)
}

fn run_block(block: &mut dyn Block, input: &Stream, chunk: usize, end: bool) -> Run {
    let info = PortInfo {
        ty: input.ty(),
        rate_hz: 9600.0,
        max_items: chunk,
        hold_items: 0,
    };
    let outs = block.init(&[info]).unwrap();
    assert_eq!(outs[0].ty, PortType::Bits);
    let mut outputs = vec![Output::for_port(&outs[0])];
    let mut r = Run::default();
    let n = input.len();
    let mut a = 0;
    while a < n {
        let b = (a + chunk).min(n);
        outputs[0].begin_chunk();
        let mut meta = ChunkMeta {
            index: a as u64,
            source_index: a as f64 * PER_ITEM,
            source_per_item: PER_ITEM,
            flags: if a == 0 {
                ChunkFlags::DISCONTINUITY
            } else {
                ChunkFlags::NONE
            },
            ..ChunkMeta::start(9600.0)
        };
        if end && b == n {
            meta.flags |= ChunkFlags::END;
        }
        let inputs = [Input {
            meta,
            data: input.slice(a, b),
        }];
        block.process(&mut Io::new(&inputs, &mut outputs)).unwrap();
        let out = &outputs[0];
        let PortVec::Bits(bits) = &out.data else {
            panic!("bits out")
        };
        assert!(bits.len() <= outs[0].max_items, "output within max_items");
        if out.meta.flags.contains(ChunkFlags::DISCONTINUITY) {
            r.disc.push(r.bits.len());
        }
        for (i, &bit) in bits.iter().enumerate() {
            r.bits.push(bit);
            r.source
                .push(out.meta.source_index + i as f64 * out.meta.source_per_item);
        }
        a = b;
    }
    r.status = block.status();
    r
}

fn extra(s: &Status, key: &str) -> Option<f64> {
    s.extra.iter().find(|(k, _)| *k == key).map(|(_, v)| v)
}

fn errors(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).filter(|(x, y)| x != y).count()
}

/// Offset of `window` in `data` (exact match), if any.
fn find(data: &[u8], window: &[u8]) -> Option<usize> {
    data.windows(window.len()).position(|w| w == window)
}

// ---- the code: the standard's own data ------------------------------------------------------

/// A shift-register encoder written straight from CCSDS 131.0-B figure 3-1 — octal taps with
/// the newest bit as the MSB, G2 inverted, C1 first — independent of `Code`'s tables.
fn ccsds_reference(bits: &[u8], invert_g2: bool) -> Vec<u8> {
    const G1: u32 = 0o171;
    const G2: u32 = 0o133;
    let mut reg = 0u32; // previous six inputs, most recent in bit 5
    let mut out = Vec::new();
    for &b in bits {
        let r = (u32::from(b) << 6) | reg;
        out.push(((r & G1).count_ones() & 1) as u8);
        out.push(((r & G2).count_ones() & 1) as u8 ^ u8::from(invert_g2));
        reg = r >> 1;
    }
    out
}

/// The CCSDS attached sync marker 0x1ACFFC1D (131.0-B §9).
fn asm_bits() -> Vec<u8> {
    (0..32)
        .rev()
        .map(|i| (0x1ACF_FC1Du32 >> i) as u8 & 1)
        .collect()
}

fn word_bits(w: u64, n: usize) -> Vec<u8> {
    (0..n).rev().map(|i| (w >> i) as u8 & 1).collect()
}

#[test]
fn ccsds_known_vectors_impulse_asm_and_the_lrpt_correlator_word() {
    let code = code_of(&ccsds());
    // Both notations of the standard's generators give the same code.
    let octal = code_of(&json!({ "constraint_length": 7, "polys": ["0x79", "0x5B"],
                                 "poly_order": "newest-msb", "invert": [false, true] }));
    assert_eq!(
        (code.next.clone(), code.out.clone()),
        (octal.next, octal.out)
    );
    assert_eq!((code.states, code.n, code.tail_steps), (64, 2, Some(6)));

    // Impulse response = the generator figure: G1 taps 1111001, G2 taps 1011011 inverted.
    let mut impulse = vec![1u8];
    impulse.extend([0; 6]);
    assert_eq!(
        encode(&code, &impulse, 0),
        [1, 0, 1, 1, 1, 0, 1, 0, 0, 1, 0, 0, 1, 0],
        "C1 = 1111001, C2 = !1011011, interleaved C1 C2"
    );
    // G2's inversion makes an all-zero input 0101…: the standard's point of inverting it.
    assert_eq!(encode(&code, &[0; 8], 0), [0, 1].repeat(8));

    // The ASM and a random block agree with the reference encoder built from the figure.
    let mut rng = Rng(7);
    for data in [asm_bits(), rng.bits(4096)] {
        assert_eq!(encode(&code, &data, 0), ccsds_reference(&data, true));
    }
    // External cross-check: Meteor-M LRPT (the same K = 7 pair, G2 *not* inverted) finds frames
    // by correlating against 0xFCA2B63DB00D9794 (meteor_decoder / medet), which is the
    // complement of the ASM encoded from state 0 — BPSK's 180° ambiguity. Cited from memory,
    // and it matched bit for bit: an independent check of the tap order.
    let lrpt = code_of(&json!({ "constraint_length": 7, "polys": ["0x4F", "0x6D"] }));
    let encoded_asm = encode(&lrpt, &asm_bits(), 0);
    assert_eq!(encoded_asm, ccsds_reference(&asm_bits(), false));
    assert_eq!(encoded_asm, word_bits(!0xFCA2_B63D_B00D_9794u64, 64));

    // And the block decodes the standard's code: ASM + data, noiseless, soft and hard.
    let mut data = asm_bits();
    data.extend(rng.bits(2000));
    let coded = encode(&code, &data, 0);
    for input in [Stream::Soft(soft_of(&coded)), Stream::Bits(coded.clone())] {
        let r = run(
            with(ccsds(), json!({ "align": "fixed" })),
            &input,
            256,
            true,
        );
        assert_eq!(r.bits, data);
    }
}

// ---- streaming ------------------------------------------------------------------------------

#[test]
fn stream_output_is_invariant_to_chunking_including_a_realignment() {
    let mut rng = Rng(11);
    let data = rng.bits(3000);
    let code = code_of(&ccsds());
    // Start mid-pair so `auto` has to realign, at 3 dB so the metrics are noisy.
    let coded = encode(&code, &data, 0);
    let soft = awgn(&coded[1..], 0.5, 3.0, &mut rng);
    let input = Stream::Soft(soft);
    let whole = run(ccsds(), &input, input.len(), true);
    assert_eq!(whole.disc, [0], "only the stream start");
    assert_eq!(
        extra(&whole.status, "realignments"),
        Some(1.0),
        "one realignment"
    );
    for chunk in [1, 7, 37, 1000] {
        let r = run(ccsds(), &input, chunk, true);
        assert_eq!(r.bits, whole.bits, "bits, chunk {chunk}");
        assert_eq!(r.disc, whole.disc, "discontinuities, chunk {chunk}");
        assert_eq!(r.status, whole.status, "status, chunk {chunk}");
        // Each chunk's map starts exact; across the switch a linear map is off by < 1 item.
        for (i, (a, b)) in r.source.iter().zip(&whole.source).enumerate() {
            assert!(
                (a - b).abs() <= PER_ITEM,
                "time map of bit {i}, chunk {chunk}"
            );
        }
    }
    // After the realignment the output is the data.
    let at = find(&data, &whole.bits[400..464]).expect("realigned output is the data");
    assert_eq!(errors(&whole.bits[400..], &data[at..]), 0);

    // Without a realignment (fixed phase) the time map is exact whatever the chunking, and a
    // hard-decision punctured stream decodes to the same bits.
    let fixed = with(ccsds(), json!({ "align": "fixed" }));
    let input = Stream::Soft(awgn(&coded, 0.5, 3.0, &mut rng));
    let whole = run(fixed.clone(), &input, input.len(), true);
    let punctured = with(ccsds(), json!({ "puncture": ["101", "110"] }));
    let pcode = code_of(&punctured);
    let hard = awgn(&encode(&pcode, &data, 0)[2..], 0.75, 6.0, &mut rng);
    let hard = Stream::Bits(hard.iter().map(|&v| u8::from(v > 0.0)).collect());
    let whole_hard = run(punctured.clone(), &hard, hard.len(), true);
    for chunk in [1, 7, 37, 1000] {
        let r = run(fixed.clone(), &input, chunk, true);
        assert_eq!(r.bits, whole.bits, "fixed bits, chunk {chunk}");
        assert_eq!(r.source, whole.source, "fixed time map, chunk {chunk}");
        let r = run(punctured.clone(), &hard, chunk, true);
        assert_eq!(
            r.bits, whole_hard.bits,
            "punctured hard bits, chunk {chunk}"
        );
        assert_eq!(
            r.status, whole_hard.status,
            "punctured hard status, chunk {chunk}"
        );
    }
}

#[test]
fn auto_alignment_finds_the_pair_phase_and_fixed_does_not() {
    let mut rng = Rng(3);
    let data = rng.bits(4000);
    let code = code_of(&ccsds());
    let coded = encode(&code, &data, 0);
    let soft = Stream::Soft(awgn(&coded[1..], 0.5, 4.0, &mut rng));
    let auto = run(ccsds(), &soft, 500, true);
    assert_eq!(auto.status.lock, Lock::Locked);
    assert_eq!(extra(&auto.status, "phase"), Some(1.0));
    assert_eq!(extra(&auto.status, "realignments"), Some(1.0));
    assert_eq!(auto.disc, [0], "a realignment is not a discontinuity");
    // Continuous in time: one bit per step of air time, none skipped, and the switch repeats
    // the half-step the two phases overlap (the input is one coded item short, so the last
    // step is incomplete: 3999 steps + 1 repeated).
    assert_eq!(auto.bits.len(), data.len());
    let at = find(&data, &auto.bits[400..464]).unwrap();
    assert_eq!(
        at, 400,
        "the realigned phase resumed where the old one stopped"
    );
    assert_eq!(errors(&auto.bits[400..], &data[400..auto.bits.len()]), 0);
    // The time map follows the phase: data bit d starts at input item 2d − 1 (the input lacks
    // coded item 0), to within one item inside the chunk where the switch happened.
    for d in 400..auto.bits.len() {
        let want = (2.0 * d as f64 - 1.0) * PER_ITEM;
        assert!(
            (auto.source[d] - want).abs() <= PER_ITEM,
            "bit {d}: {} vs {want}",
            auto.source[d]
        );
    }
    // `error_rate` estimates the channel (pre-decoding) bit error rate by re-encoding: at 4 dB,
    // rate 1/2, Es/N0 = 1 dB and uncoded BPSK errs Q(√(2 Es/N0)) ≈ 0.056.
    let raw = qfunc((2.0 * 0.5 * 10f64.powf(0.4)).sqrt());
    let est = f64::from(auto.status.error_rate.unwrap());
    assert!((est - raw).abs() < 0.015, "channel BER {est} vs {raw}");

    let fixed = run(with(ccsds(), json!({ "align": "fixed" })), &soft, 500, true);
    assert_eq!(fixed.status.lock, Lock::None);
    let e = errors(&fixed.bits[100..], &data[100..]);
    assert!(e > fixed.bits.len() / 5, "wrong phase decodes garbage: {e}");
    let wrong = f64::from(fixed.status.error_rate.unwrap());
    assert!(wrong > 2.0 * raw, "wrong phase: channel estimate {wrong}");
}

/// CCSDS 131.0-B §3.5 punctured rates, received from an arbitrary offset in the period.
#[test]
fn punctured_ccsds_rates_decode_from_any_phase() {
    let patterns: [(&str, &str, f64); 4] = [
        ("10", "11", 2.0 / 3.0),
        ("101", "110", 3.0 / 4.0),
        ("10101", "11010", 5.0 / 6.0),
        ("1000101", "1111010", 7.0 / 8.0),
    ];
    for (c1, c2, rate) in patterns {
        let params = with(
            ccsds(),
            json!({ "puncture": [c1, c2], "traceback_bits": 128 }),
        );
        let code = code_of(&params);
        assert!((code.rate() - rate).abs() < 1e-9);
        let mut rng = Rng(5);
        let data = rng.bits(6000);
        let coded = encode(&code, &data, 0);
        for skip in [0, code.kept_per_period() - 1] {
            let soft = Stream::Soft(awgn(&coded[skip..], rate, 8.0, &mut rng));
            let r = run(params.clone(), &soft, 333, true);
            let seg = &r.bits[..];
            let at = find(&data, &seg[300..364])
                .unwrap_or_else(|| panic!("{c1}/{c2} skip {skip}: no data in the output"));
            let e = errors(&seg[300..], &data[at..]);
            assert!(e == 0, "{c1}/{c2} skip {skip}: {e} errors");
            assert_eq!(r.status.lock, Lock::Locked, "{c1}/{c2} skip {skip}");
        }
    }
}

/// The truncated union bound for K = 7 (171, 133), rate 1/2, soft decision:
/// `Pb ≤ Σ_d B_d Q(√(2 d R Eb/N0))`, d = 10…18, with the code's bit-weight spectrum
/// B_d = 36, 211, 1404, 11633, 77433 (Odenwalder; Proakis table 8.2-x).
fn union_bound(ebn0_db: f64) -> f64 {
    let ebn0 = 10f64.powf(ebn0_db / 10.0);
    [
        (10, 36.0),
        (12, 211.0),
        (14, 1404.0),
        (16, 11633.0),
        (18, 77433.0),
    ]
    .iter()
    .map(|&(d, b)| b * qfunc((f64::from(d) * ebn0).sqrt()))
    .sum()
}

/// Gaussian tail: Q(x) = erfc(x / √2) / 2. With R = 1/2 the union-bound argument
/// √(2 d R Eb/N0) is √(d Eb/N0).
fn qfunc(x: f64) -> f64 {
    0.5 * erfc(x / std::f64::consts::SQRT_2)
}

/// erfc by Numerical Recipes' Chebyshev fit (|error| < 1.2e-7 relative).
fn erfc(x: f64) -> f64 {
    let z = x.abs();
    let t = 1.0 / (1.0 + 0.5 * z);
    let r = t
        * (-z * z - 1.265_512_23
            + t * (1.000_023_68
                + t * (0.374_091_96
                    + t * (0.096_784_18
                        + t * (-0.186_288_06
                            + t * (0.278_868_07
                                + t * (-1.135_203_98
                                    + t * (1.488_515_87
                                        + t * (-0.822_152_23 + t * 0.170_872_77)))))))))
            .exp();
    if x >= 0.0 { r } else { 2.0 - r }
}

/// Bit error rate of `viterbi` (fixed phase) on CCSDS-coded BPSK at `ebn0_db`, soft or hard.
fn ber(ebn0_db: f64, bits: usize, hard: bool, seed: u64) -> f64 {
    let mut rng = Rng(seed);
    let data = rng.bits(bits);
    let code = code_of(&ccsds());
    let soft = awgn(&encode(&code, &data, 0), 0.5, ebn0_db, &mut rng);
    let input = if hard {
        Stream::Bits(soft.iter().map(|&v| u8::from(v > 0.0)).collect())
    } else {
        Stream::Soft(soft)
    };
    let r = run(
        with(ccsds(), json!({ "align": "fixed" })),
        &input,
        4096,
        true,
    );
    assert_eq!(r.bits.len(), data.len());
    assert_eq!(
        extra(&r.status, "hard_decision"),
        Some(if hard { 1.0 } else { 0.0 }),
        "hard-decision input is reported as such"
    );
    errors(&r.bits, &data) as f64 / bits as f64
}

/// The BER-vs-Eb/N0 curve of the CCSDS code, soft decision, against the union bound, and the
/// hard-decision penalty. Stated tolerances:
/// - at 3.0 and 3.5 dB the measured BER lies within [bound / 10, bound × 1.5] (the bound is an
///   upper bound, within a few tenths of a dB of the true curve here; measured on this seed set:
///   0.73× and 0.67×);
/// - the curve falls monotonically, and at 3.5 dB is over two decades under uncoded BPSK;
/// - hard decision costs about 2 dB: hard at 5 dB is within 4× of soft at 3 dB (measured 1.1×),
///   and hard at 3 dB is over 10× worse than soft at 3 dB (measured 80×).
///
/// Sample sizes keep ≥ ~20 bit errors per soft point without a release build (≈ 20 s
/// unoptimised). Reference measurements at larger sizes (not run here): 4 dB soft 6.0e-6 over
/// 10⁶ bits against a bound of 1.8e-5.
#[test]
fn ber_curve_matches_the_union_bound_and_hard_decision_costs_about_2_db() {
    let points = [(2.0, 100_000), (3.0, 200_000), (3.5, 300_000)];
    let mut soft = Vec::new();
    for (i, &(db, n)) in points.iter().enumerate() {
        let b = ber(db, n, false, 100 + i as u64);
        eprintln!(
            "soft {db} dB: BER {b:.2e} (union bound {:.2e})",
            union_bound(db)
        );
        if let Some(&last) = soft.last() {
            assert!(
                b < last,
                "BER falls with Eb/N0: {db} dB {b:.2e} vs {last:.2e}"
            );
        }
        soft.push(b);
        if db >= 3.0 {
            let ub = union_bound(db);
            assert!(
                b <= ub * 1.5 && b >= ub / 10.0,
                "{db} dB: BER {b:.2e} outside [{:.2e}, {:.2e}]",
                ub / 10.0,
                ub * 1.5
            );
        }
    }
    let uncoded = qfunc((2.0 * 10f64.powf(0.35)).sqrt());
    assert!(soft[2] < uncoded / 100.0, "coding gain at 3.5 dB");
    let hard3 = ber(3.0, 50_000, true, 200);
    let hard5 = ber(5.0, 200_000, true, 201);
    eprintln!("hard 3 dB: {hard3:.2e}; hard 5 dB: {hard5:.2e}");
    assert!(
        hard3 > 10.0 * soft[1],
        "hard 3 dB {hard3:.2e} vs soft {:.2e}",
        soft[1]
    );
    assert!(
        hard5 < 4.0 * soft[1] && hard5 > soft[1] / 4.0,
        "hard 5 dB {hard5:.2e} vs soft 3 dB {:.2e}",
        soft[1]
    );
}

#[test]
fn a_restart_drops_the_undecided_tail_and_starts_like_a_fresh_block() {
    let mut rng = Rng(9);
    let data = rng.bits(1000);
    let coded = soft_of(&encode(&code_of(&ccsds()), &data, 0));
    let params = with(ccsds(), json!({ "align": "fixed" }));
    let mut block = build("viterbi", params.clone(), PortType::Soft);
    let info = PortInfo {
        ty: PortType::Soft,
        rate_hz: 9600.0,
        max_items: 2000,
        hold_items: 0,
    };
    let outs = block.init(&[info]).unwrap();
    let mut outputs = vec![Output::for_port(&outs[0])];
    let feed = |b: &mut dyn Block, outputs: &mut Vec<Output>, flags| {
        outputs[0].begin_chunk();
        let inputs = [Input {
            meta: ChunkMeta {
                flags,
                ..ChunkMeta::start(9600.0)
            },
            data: PortSlice::Soft(&coded),
        }];
        b.process(&mut Io::new(&inputs, outputs)).unwrap();
        let PortVec::Bits(bits) = &outputs[0].data else {
            panic!()
        };
        (bits.clone(), outputs[0].meta.flags)
    };
    let (first, _) = feed(block.as_mut(), &mut outputs, ChunkFlags::DISCONTINUITY);
    // Depth 64 → block 32: 1000 steps decide 32 × (⌊(1000 − 96) / 32⌋ + 1) = 928 bits.
    assert_eq!(first.len(), 928);
    assert_eq!(first, data[..928]);
    let (again, flags) = feed(block.as_mut(), &mut outputs, ChunkFlags::DISCONTINUITY);
    assert!(flags.contains(ChunkFlags::DISCONTINUITY), "propagated");
    assert_eq!(again, first, "a restart is a fresh decoder");
    assert_eq!(extra(&block.status(), "dropped_bits"), Some(72.0));
}

// ---- table-defined trellis ------------------------------------------------------------------

/// The CCSDS code written as a `trellis` table decodes exactly as the `polys` form.
fn ccsds_as_table() -> Value {
    let code = code_of(&ccsds());
    json!({ "trellis": { "input_bits": 1, "output_bits": 2, "next_state": code.next,
                         "output": code.out } })
}

#[test]
fn a_trellis_table_is_the_same_decoder_as_its_polynomials() {
    let mut rng = Rng(21);
    let data = rng.bits(3000);
    let code = code_of(&ccsds());
    let soft = Stream::Soft(awgn(&encode(&code, &data, 0), 0.5, 2.5, &mut rng));
    let a = run(ccsds(), &soft, 100, true);
    let b = run(ccsds_as_table(), &soft, 100, true);
    assert_eq!(a.bits, b.bits);
    assert_eq!(a.status.error_rate, b.status.error_rate);
}

/// A 4-state dibit trellis shaped like P25 Phase 1's rate-1/2 code (state = last dibit, a 4-bit
/// constellation point per step, terminated by one zero dibit): the transition table is the one
/// DSD/OP25 carry, cited from memory and NOT checked against TIA-102.BAAA here — the test is of
/// the table form (k = 2 input bits per step), not of P25.
fn p25_like() -> Value {
    const POINT: [u16; 16] = [0, 15, 12, 3, 4, 11, 8, 7, 13, 2, 1, 14, 9, 6, 5, 10];
    let next: Vec<u16> = (0..16).map(|i| i & 3).collect();
    json!({ "trellis": { "input_bits": 2, "output_bits": 4, "next_state": next,
                         "output": POINT }, "termination": "terminated" })
}

// ---- per frame ------------------------------------------------------------------------------

fn frame(bits: &[u8]) -> Owned {
    Owned::from_bits(bits, 0, 0)
}

fn flip(bits: &mut [u8], at: &[usize]) {
    for &i in at {
        bits[i] ^= 1;
    }
}

#[test]
fn frames_terminated_truncated_and_tail_biting_each_correct_errors() {
    let code = code_of(&ccsds());
    let mut rng = Rng(31);
    let data = rng.bits(200);

    // Terminated: six zero tail bits, stripped from the output; a 16-bit header passes through.
    let mut tailed = data.clone();
    tailed.extend([0; 6]);
    let header = rng.bits(16);
    let mut bits = header.clone();
    bits.extend(encode(&code, &tailed, 0));
    let trailer = rng.bits(5);
    bits.extend(&trailer);
    flip(&mut bits, &[20, 60, 61, 150, 300]);
    let params = with(
        ccsds(),
        json!({ "termination": "terminated",
                "span": { "start_bit": 16, "end_trim_bits": 5 } }),
    );
    let mut b = build("viterbi_frames", params, PortType::Frames);
    let out = run_frames(b.as_mut(), &[frame(&bits)], 1, true);
    let mut want = header.clone();
    want.extend(&data);
    want.extend(&trailer);
    assert_eq!(out[0].bits, want);
    assert_eq!(out[0].info.corrected_bits, 5);
    assert_eq!(
        out[0].info.check,
        hk_model::CrcStatus::Unknown,
        "a check block decides"
    );
    assert_eq!(extra(&b.status(), "hard_decision"), Some(1.0));

    // Truncated: starts in 0, ends anywhere; every step is data.
    let mut bits = encode(&code, &data, 0);
    flip(&mut bits, &[10, 101, 250]);
    let mut b = build(
        "viterbi_frames",
        with(ccsds(), json!({ "termination": "truncated" })),
        PortType::Frames,
    );
    let out = run_frames(b.as_mut(), &[frame(&bits)], 1, true);
    assert_eq!(out[0].bits.len(), 200);
    assert_eq!(errors(&out[0].bits[..190], &data[..190]), 0);

    // Tail-biting: the encoder starts in the state its last six bits leave it in.
    let start = data[194..]
        .iter()
        .fold(0usize, |s, &b| ((s << 1) | usize::from(b)) & 63);
    let mut bits = encode(&code, &data, start);
    flip(&mut bits, &[0, 3, 200, 399]);
    let mut b = build(
        "viterbi_frames",
        with(ccsds(), json!({ "termination": "tail-biting" })),
        PortType::Frames,
    );
    let out = run_frames(b.as_mut(), &[frame(&bits)], 1, true);
    assert_eq!(out[0].bits, data);
    assert_eq!(out[0].info.corrected_bits, 4);
}

#[test]
fn frames_decode_a_dibit_trellis_table_and_refuse_short_frames() {
    let params = p25_like();
    let code = code_of(&params);
    assert_eq!(
        (code.states, code.input_bits, code.tail_steps),
        (4, 2, Some(1))
    );
    let mut rng = Rng(41);
    let data = rng.bits(96); // 48 dibits
    let mut tailed = data.clone();
    tailed.extend([0, 0]);
    let mut bits = encode(&code, &tailed, 0); // 49 steps × 4 = 196 bits
    assert_eq!(bits.len(), 196);
    flip(&mut bits, &[7, 90]);
    let mut b = build("viterbi_frames", params, PortType::Frames);
    let short = frame(&bits[..4]);
    let out = run_frames(b.as_mut(), &[frame(&bits), short], 2, true);
    assert_eq!(out.len(), 1, "a frame of only the tail is refused");
    assert_eq!(out[0].bits, data);
    assert_eq!(out[0].info.corrected_bits, 2);
    assert_eq!(extra(&b.status(), "frames_refused"), Some(1.0));
}

#[test]
fn punctured_frames_restart_the_period_at_each_frame() {
    let params = with(
        ccsds(),
        json!({ "puncture": ["101", "110"], "termination": "terminated" }),
    );
    let code = code_of(&params);
    let mut b = build("viterbi_frames", params, PortType::Frames);
    let mut rng = Rng(51);
    let frames: Vec<(Vec<u8>, Owned)> = (0..3)
        .map(|i| {
            let data = rng.bits(90 + i);
            let mut tailed = data.clone();
            tailed.extend([0; 6]);
            let mut bits = encode(&code, &tailed, 0);
            flip(&mut bits, &[30]);
            (data, frame(&bits))
        })
        .collect();
    let input: Vec<Owned> = frames.iter().map(|(_, f)| f.clone()).collect();
    let out = run_frames(b.as_mut(), &input, 2, true);
    for ((data, _), o) in frames.iter().zip(&out) {
        assert_eq!(&o.bits, data);
    }
}

#[test]
fn code_parameters_are_checked() {
    for (params, want) in [
        (json!({}), "no code"),
        (with(ccsds(), ccsds_as_table()), "not both"),
        (json!({ "polys": ["0x4F", "0x6D"] }), "constraint_length"),
        (
            json!({ "constraint_length": 3, "polys": ["0x4F", "0x6D"] }),
            "wider",
        ),
        (
            with(ccsds(), json!({ "invert": [true, false, true] })),
            "invert",
        ),
        (
            with(ccsds(), json!({ "puncture": ["10", "1"] })),
            "puncture",
        ),
        (
            with(ccsds(), json!({ "puncture": ["10", "01x"] })),
            "puncture",
        ),
        (
            with(ccsds(), json!({ "puncture": ["10", "10"] })),
            "transmits nothing",
        ),
        (
            json!({ "trellis": { "input_bits": 1, "output_bits": 2, "next_state": [0, 1, 0, 2],
                                 "output": [0, 3, 1, 2] } }),
            "not a state",
        ),
    ] {
        let e = try_build("viterbi", params.clone(), PortType::Soft)
            .err()
            .unwrap_or_else(|| panic!("{params} builds"));
        assert!(e.contains(want), "{params}: {e}");
    }
    let e = try_build(
        "viterbi_frames",
        json!({ "trellis": { "input_bits": 1, "output_bits": 2, "next_state": [1, 0, 1, 0],
                             "output": [0, 3, 1, 2] }, "termination": "terminated" }),
        PortType::Frames,
    )
    .err()
    .unwrap();
    assert!(e.contains("state 0"), "{e}");
}

// ---- SIGNAL-034: the composed chain --------------------------------------------------------

/// SIGNAL-034 (CCSDS cubesat telemetry), SPACE-081: continuously convolutionally coded BPSK
/// telemetry frames — ASM 0x1ACFFC1D + payload, CCSDS 131.0-B order — synthesised as RRC-shaped
/// IQ with a carrier offset, an unknown phase and a fractional timing offset, then recovered
/// blind by `psk_demod → viterbi → sync_search (ASM)`. The receiver is told the symbol rate and
/// the code, never the phase, the pair alignment or where a frame starts: BPSK's 180° ambiguity
/// comes out of `psk_demod` as inverted soft bits, which the code maps to inverted data (both
/// generators have odd weight), and `sync_search`'s `polarity: either` undoes it.
#[test]
fn signal_034_ccsds_frames_recover_through_psk_viterbi_and_the_asm() {
    use crate::blocks::iq::testkit::{Chain, Lcg, build as build_iq};
    use num_complex::Complex32;
    use std::f64::consts::{PI, TAU};

    const FS: f64 = 48_000.0;
    const RS: f64 = 4_800.0; // coded symbols per second = 2 × the 2400 bit/s payload
    const SPS: f64 = FS / RS;
    const PAYLOAD: usize = 1024;
    const FRAMES: usize = 5;
    let ebn0_db = 5.0; // Es/N0 = 2 dB per coded symbol: uncoded BPSK would err 3.8e-2
    let rrc = |t: f64, a: f64| -> f64 {
        if t.abs() < 1e-9 {
            return 1.0 - a + 4.0 * a / PI;
        }
        if (t.abs() - 1.0 / (4.0 * a)).abs() < 1e-9 {
            let x = PI / (4.0 * a);
            return a / 2f64.sqrt() * ((1.0 + 2.0 / PI) * x.sin() + (1.0 - 2.0 / PI) * x.cos());
        }
        let num = (PI * t * (1.0 - a)).sin() + 4.0 * a * t * (PI * t * (1.0 + a)).cos();
        num / (PI * t * (1.0 - (4.0 * a * t).powi(2)))
    };

    let mut rng = Lcg::new(34);
    let mut data: Vec<u8> = (0..300).map(|_| rng.bit()).collect(); // idle fill ahead
    let mut payloads = Vec::new();
    for _ in 0..FRAMES {
        data.extend(asm_bits());
        let p: Vec<u8> = (0..PAYLOAD).map(|_| rng.bit()).collect();
        data.extend(&p);
        payloads.push(p);
    }
    data.extend((0..200).map(|_| rng.bit()));
    let coded = encode(&code_of(&ccsds()), &data, 0);
    let (tau, cfo, phase) = (0.37, 350.0, 1.1);
    let n = ((coded.len() as f64 + 1.0) * SPS) as usize;
    let mut x = vec![Complex32::new(0.0, 0.0); n];
    for (s, v) in x.iter_mut().enumerate() {
        let t = s as f64 / SPS - tau;
        let lo = (t - 8.0).floor().max(0.0) as usize;
        let hi = ((t + 8.0).ceil().max(0.0) as usize).min(coded.len());
        let re: f64 = (lo..hi)
            .map(|k| (2.0 * f64::from(coded[k]) - 1.0) * rrc(t - k as f64, 0.35))
            .sum();
        *v = Complex32::new(re as f32, 0.0);
    }
    let power = x.iter().map(|z| f64::from(z.norm_sqr())).sum::<f64>() / n as f64;
    let es_n0 = 10f64.powf((ebn0_db + 10.0 * 0.5f64.log10()) / 10.0);
    for (s, v) in x.iter_mut().enumerate() {
        let ph = TAU * cfo * s as f64 / FS + phase;
        *v = *v * Complex32::from_polar(1.0, ph as f32) + rng.cnoise(power * SPS / es_n0);
    }

    let info = PortInfo {
        ty: PortType::Iq,
        rate_hz: FS,
        max_items: 1_000,
        hold_items: 0,
    };
    let chain = |with_fec: bool| {
        let mut blocks = vec![build_iq(
            "psk_demod",
            json!({ "modulation": "bpsk", "symbol_rate_bd": RS }),
            PortType::Iq,
        )];
        if with_fec {
            blocks.push(build_iq("viterbi", ccsds(), PortType::Soft));
        } else {
            blocks.push(build_iq("slicer", json!({}), PortType::Soft));
        }
        blocks.push(build_iq(
            "sync_search",
            json!({ "mode": "sync-word", "sync_word": "0x1ACFFC1D", "sync_bits": 32,
                    "max_errors": 3, "frame_bits": PAYLOAD, "polarity": "either" }),
            PortType::Bits,
        ));
        let mut c = Chain::new(blocks, info);
        c.run(&PortVec::Iq(x.clone()), 1_000);
        c
    };
    let c = chain(true);
    let frames = &c.out(2, 0).frames;
    let unpack = |b: &[u8]| -> Vec<u8> {
        (0..PAYLOAD)
            .map(|i| (b[i / 8] >> (7 - i % 8)) & 1)
            .collect()
    };
    let got: Vec<Vec<u8>> = frames.iter().map(|(b, _)| unpack(b)).collect();
    // Every frame after acquisition comes out exact (the first may fall in the acquisition).
    let exact = payloads.iter().filter(|p| got.contains(p)).count();
    assert!(
        exact >= FRAMES - 1,
        "{exact} of {FRAMES} frames exact; got {}",
        got.len()
    );
    assert!(
        got.iter().all(|g| payloads.contains(g)),
        "no frame is a near miss"
    );
    let v = c.block(1).status();
    assert_eq!(v.lock, Lock::Locked, "viterbi locked its pair phase");
    assert_eq!(
        extra(&v, "hard_decision"),
        Some(0.0),
        "soft decision all the way"
    );

    // Red without the decoder: the coded stream never contains the ASM.
    let c = chain(false);
    assert!(
        c.out(2, 0).frames.is_empty(),
        "no ASM in the undecoded stream"
    );
}
