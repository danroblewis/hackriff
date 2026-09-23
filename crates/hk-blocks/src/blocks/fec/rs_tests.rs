//! `reed_solomon` tests (T-611). Known vectors are from Phil Karn's libfec — the CCSDS
//! reference implementation — as shipped in GNU Radio 3.10.12 (`libgnuradio-fec`):
//! `encode_rs_ccsds` (dual basis), `encode_rs_8` (CCSDS, conventional) and
//! `init_rs_char(symsize, gfpoly, fcr, prim, nroots)` + `encode_rs_char` for the others, a
//! shortened code encoded as the full code with its leading data symbols zero. Data are
//! `(i·a + b) mod 2^m`, `i` from 0 (see [`ramp`]), each vector from a fresh process.
//! Every vector was also checked independently (Python `reedsolo` field tables, syndromes at
//! `α^(prim·(fcr+i))`). Two defects of that GNU Radio build, found doing so: `decode_rs_ccsds`
//! returns −1 even for an error-free codeword, and `encode_rs_ccsds` rewrites its `data`
//! argument and corrupts later `encode_rs_8` calls in the same process — its own output is a
//! valid codeword (it converts, by libfec's `tal` matrix, to a valid conventional one).

use hk_model::CrcStatus;
use hk_recipe::PortType;
use serde_json::{Value, json};

use super::rs::{Decoder, DualBasis, Field, Sym};
use crate::block::Block;
use crate::blocks::framing::common::testutil::{Owned, build, run_frames, try_build};

/// One named code, its libfec parity for [`ramp`] data, and its block parameters.
struct Known {
    name: &'static str,
    params: Value,
    m: usize,
    n: usize,
    k: usize,
    /// `(a, b)` of the data ramp.
    ramp: (usize, usize),
    parity: &'static str,
}

fn known() -> Vec<Known> {
    let ccsds = |dual: bool| json!({"n": 255, "k": 223, "poly": "0x187", "fcr": 112, "prim": 11, "dual_basis": dual});
    let dvb = |n: usize, k: usize| json!({"n": n, "k": k, "poly": "0x11D", "fcr": 0});
    let p25 =
        |n: usize, k: usize| json!({"n": n, "k": k, "symbol_bits": 6, "poly": "0x43", "fcr": 1});
    vec![
        Known {
            name: "CCSDS RS(255,223) dual basis (encode_rs_ccsds)",
            params: ccsds(true),
            m: 8,
            n: 255,
            k: 223,
            ramp: (7, 3),
            parity: "0c29d565dd7fb9654ba23207f8d9962e04699784e7e28d63c017bad54b582bc4",
        },
        Known {
            name: "CCSDS RS(255,223) conventional (encode_rs_8)",
            params: ccsds(false),
            m: 8,
            n: 255,
            k: 223,
            ramp: (7, 3),
            parity: "3f56af8183b8ad235310d48f4ce7c60e458d1948b674923ab100c186f0bc1519",
        },
        Known {
            name: "RS(255,239) 0x11D fcr 0",
            params: dvb(255, 239),
            m: 8,
            n: 255,
            k: 239,
            ramp: (13, 5),
            parity: "fa9853a7674242289845d1a896176084",
        },
        Known {
            name: "DVB RS(204,188), shortened RS(255,239)",
            params: dvb(204, 188),
            m: 8,
            n: 204,
            k: 188,
            ramp: (29, 1),
            parity: "0d8315d26eeed71f95cf5965c9ae2aee",
        },
        Known {
            name: "P25 RS(24,12,13) over GF(64)",
            params: p25(24, 12),
            m: 6,
            n: 24,
            k: 12,
            ramp: (5, 1),
            parity: "38190b1735190d2b3b330331",
        },
        Known {
            name: "P25 RS(24,16,9) over GF(64)",
            params: p25(24, 16),
            m: 6,
            n: 24,
            k: 16,
            ramp: (5, 1),
            parity: "2e0c1e0c030b0219",
        },
        Known {
            name: "P25 RS(36,20,17) over GF(64)",
            params: p25(36, 20),
            m: 6,
            n: 36,
            k: 20,
            ramp: (5, 1),
            parity: "321b0718022d200620321c2c37272426",
        },
    ]
}

/// `(i·a + b) mod 2^m` for `i` in `0..k`.
fn ramp(k: usize, (a, b): (usize, usize), m: usize) -> Vec<Sym> {
    (0..k).map(|i| ((i * a + b) % (1 << m)) as Sym).collect()
}

fn unhex(s: &str) -> Vec<Sym> {
    (0..s.len())
        .step_by(2)
        .map(|i| Sym::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

impl Known {
    fn codeword(&self) -> Vec<Sym> {
        let mut c = ramp(self.k, self.ramp, self.m);
        c.extend(unhex(self.parity));
        assert_eq!(c.len(), self.n, "{}", self.name);
        c
    }
}

fn sym_bits(syms: &[Sym], m: usize) -> Vec<u8> {
    syms.iter()
        .flat_map(|&s| (0..m).rev().map(move |b| (s >> b) as u8 & 1))
        .collect()
}

fn frame(bits: &[u8]) -> Owned {
    Owned::from_bits(bits, 0, 0)
}

fn extra(b: &dyn Block, key: &str) -> f64 {
    b.status()
        .extra
        .iter()
        .find(|e| e.0 == key)
        .unwrap_or_else(|| panic!("{key}"))
        .1
}

fn with(mut base: Value, more: Value) -> Value {
    for (k, v) in more.as_object().unwrap() {
        base[k] = v.clone();
    }
    base
}

fn decoder(params: &Value) -> Decoder {
    let p = |k: &str| params[k].as_u64();
    let m = p("symbol_bits").unwrap_or(8) as u32;
    let poly = u64::from_str_radix(
        params["poly"].as_str().unwrap().trim_start_matches("0x"),
        16,
    )
    .unwrap();
    Decoder::new(
        Field::new(m, poly).unwrap(),
        p("n").unwrap() as usize,
        p("k").unwrap() as usize,
        p("fcr").unwrap() as usize,
        p("prim").unwrap_or(1) as usize,
    )
    .unwrap()
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() >> 11) as usize % n
    }
    /// `count` distinct indexes below `n`.
    fn positions(&mut self, n: usize, count: usize) -> Vec<usize> {
        let mut v: Vec<usize> = Vec::with_capacity(count);
        while v.len() < count {
            let p = self.below(n);
            if !v.contains(&p) {
                v.push(p);
            }
        }
        v
    }
}

/// Adds `count` symbol errors (random distinct positions, random non-zero magnitudes) to the
/// `m`-bit symbols; returns the channel bits flipped.
fn corrupt(c: &mut [Sym], m: usize, count: usize, rng: &mut Rng) -> u32 {
    let mut flipped = 0;
    for p in rng.positions(c.len(), count) {
        let e = (1 + rng.below((1 << m) - 1)) as Sym;
        c[p] ^= e;
        flipped += e.count_ones();
    }
    flipped
}

/// Every code the ticket names reproduces libfec's check symbols (the test encoder), and the
/// block accepts libfec's codeword as-is, strips it to the data, and corrects errors in it.
#[test]
fn libfec_known_vectors_for_every_named_code() {
    for kv in known() {
        let cw = kv.codeword();
        if kv.params["dual_basis"] != json!(true) {
            let dec = decoder(&kv.params);
            let mut parity = vec![0; kv.n - kv.k];
            dec.encode(&cw[..kv.k], &mut parity);
            assert_eq!(parity, cw[kv.k..], "{}: encoder", kv.name);
        }
        let mut b = build("reed_solomon", kv.params.clone(), PortType::Frames);
        let mut bad = cw.clone();
        let t = (kv.n - kv.k) / 2;
        let flipped = corrupt(&mut bad, kv.m, t, &mut Rng(0x5eed + kv.n as u64));
        let out = run_frames(
            b.as_mut(),
            &[frame(&sym_bits(&cw, kv.m)), frame(&sym_bits(&bad, kv.m))],
            2,
            false,
        );
        let data = sym_bits(&cw[..kv.k], kv.m);
        assert_eq!(out[0].info.check, CrcStatus::Valid, "{}", kv.name);
        assert_eq!(out[0].info.corrected_bits, 0, "{}", kv.name);
        assert_eq!(out[0].bits, data, "{}: stripped to the data", kv.name);
        assert_eq!(out[1].info.check, CrcStatus::Valid, "{}: t errors", kv.name);
        assert_eq!(out[1].info.corrected_bits, flipped, "{}", kv.name);
        assert_eq!(out[1].bits, data, "{}: corrected", kv.name);
        assert_eq!(extra(b.as_ref(), "corrected_symbols"), t as f64);
    }
}

/// The classic silent failure: the CCSDS dual-basis codeword decoded as conventional (or the
/// reverse) fails every time — it is not a codeword of the other representation.
#[test]
fn a_wrong_dual_basis_setting_fails_every_codeword() {
    let kvs = known();
    for (kv, other) in [(&kvs[0], &kvs[1]), (&kvs[1], &kvs[0])] {
        let mut b = build("reed_solomon", other.params.clone(), PortType::Frames);
        let out = run_frames(b.as_mut(), &[frame(&sym_bits(&kv.codeword(), 8))], 1, false);
        assert_eq!(out[0].info.check, CrcStatus::Invalid, "{}", kv.name);
        assert_eq!(out[0].info.corrected_bits, 0);
        assert_eq!(extra(b.as_ref(), "codewords_bad"), 1.0);
    }
}

/// CCSDS 131.0-B's Berlekamp representation, derived from its definition (bit i = Tr(z·β^i),
/// β = α^117), is exactly libfec's conversion matrix (`ccsds_tal.c`: `tal[]`, rows of
/// conventional bits 7..0).
#[test]
fn ccsds_dual_basis_is_the_trace_form_and_libfecs_matrix() {
    const TAL: [u8; 8] = [0x8d, 0xef, 0xec, 0x86, 0xfa, 0x99, 0xaf, 0x7b];
    let f = Field::new(8, 0x187).unwrap();
    let d = DualBasis::ccsds(&f).unwrap();
    for z in 0..=255u8 {
        let mut want = 0u8;
        for k in 0..8 {
            if z >> k & 1 == 1 {
                want ^= TAL[7 - k];
            }
        }
        assert_eq!(d.to_dual[z as usize], want, "z = {z:#04x}");
        assert_eq!(d.to_conv[want as usize], z);
    }
    // libfec's Tal1tab[1] (dual 0x01 → conventional) is the well-known 0xcc.
    assert_eq!(d.to_conv[1], 0xcc);
    assert!(DualBasis::ccsds(&Field::new(8, 0x11D).unwrap()).is_none());
}

/// The error-count boundary: every pattern of `t` symbol errors is corrected; `t + 1` on the
/// long codes is refused (frame invalid, codeword left exactly as received) and never
/// miscorrected over these trials — the bounded-distance miscorrection probability there is
/// ≈ 1/t! (RS(255,223): ~5·10⁻¹⁴).
#[test]
fn t_errors_are_corrected_and_t_plus_1_are_refused_not_miscorrected() {
    let mut rng = Rng(0x7_611);
    for kv in known().iter().take(4) {
        let t = (kv.n - kv.k) / 2;
        let cw = kv.codeword();
        let mut frames = Vec::new();
        let mut want = Vec::new();
        for trial in 0..60 {
            let errors = if trial % 2 == 0 { t } else { t + 1 };
            let mut bad = cw.clone();
            let flipped = corrupt(&mut bad, kv.m, errors, &mut rng);
            frames.push(frame(&sym_bits(&bad, kv.m)));
            want.push((errors, flipped, bad));
        }
        let mut b = build(
            "reed_solomon",
            with(kv.params.clone(), json!({"strip": false})),
            PortType::Frames,
        );
        let out = run_frames(b.as_mut(), &frames, 7, false);
        assert_eq!(out.len(), frames.len());
        for (o, (errors, flipped, bad)) in out.iter().zip(&want) {
            if *errors == t {
                assert_eq!(o.info.check, CrcStatus::Valid, "{}: t", kv.name);
                assert_eq!(o.info.corrected_bits, *flipped);
                assert_eq!(o.bits, sym_bits(&cw, kv.m));
            } else {
                assert_eq!(o.info.check, CrcStatus::Invalid, "{}: t + 1", kv.name);
                assert_eq!(o.info.corrected_bits, 0);
                assert_eq!(o.bits, sym_bits(bad, kv.m), "left as received");
            }
        }
        assert_eq!(b.status().error_rate, Some(0.5), "{}", kv.name);
        assert_eq!(extra(b.as_ref(), "codewords_bad"), 30.0);
        assert_eq!(extra(b.as_ref(), "corrected_symbols"), (30 * t) as f64);
    }
}

/// The short P25 codes: `t` errors always corrected; past `t` a bounded-distance decoder can
/// land on a different codeword, and does so here at a rate bounded by the code's volume —
/// which is why P25 layers a CRC or Golay check after it. Whatever it returns as valid is a
/// codeword (never a half-corrected word), and whatever it refuses is untouched.
#[test]
fn p25_codes_correct_t_and_past_t_refuse_or_land_on_a_codeword() {
    let mut rng = Rng(0x25);
    for kv in known().iter().skip(4) {
        let t = (kv.n - kv.k) / 2;
        let cw = kv.codeword();
        let mut dec = decoder(&kv.params);
        for _ in 0..200 {
            let mut bad = cw.clone();
            corrupt(&mut bad, kv.m, t, &mut rng);
            assert_eq!(dec.decode(&mut bad), Some(t), "{}", kv.name);
            assert_eq!(bad, cw);
        }
        let mut miscorrected = 0;
        let trials = 2_000;
        for _ in 0..trials {
            let mut bad = cw.clone();
            corrupt(&mut bad, kv.m, t + 1, &mut rng);
            let received = bad.clone();
            match dec.decode(&mut bad) {
                None => assert_eq!(bad, received, "{}: refused untouched", kv.name),
                Some(e) => {
                    assert!(e <= t);
                    assert_ne!(bad, cw);
                    let mut again = bad.clone();
                    assert_eq!(dec.decode(&mut again), Some(0), "a codeword");
                    miscorrected += 1;
                }
            }
        }
        // Shortened GF(64) codes: the fraction of the space within t of a codeword, times the
        // share of locator roots that fall inside the n sent positions — measured ≲ 1 %.
        assert!(
            miscorrected * 20 < trials,
            "{}: {miscorrected} / {trials} miscorrected",
            kv.name
        );
    }
}

fn ccsds_block(depth: usize, seed: u64) -> (Vec<Sym>, Vec<Sym>) {
    // `depth` interleaved dual-basis codewords of random data: returns (on-air block, data).
    let f = Field::new(8, 0x187).unwrap();
    let dual = DualBasis::ccsds(&f).unwrap();
    let dec = decoder(&known()[1].params);
    let mut rng = Rng(seed);
    let mut block = vec![0; depth * 255];
    for c in 0..depth {
        let data: Vec<Sym> = (0..223).map(|_| rng.below(256) as Sym).collect();
        let conv: Vec<Sym> = data
            .iter()
            .map(|&s| Sym::from(dual.to_conv[s as usize]))
            .collect();
        let mut parity = vec![0; 32];
        dec.encode(&conv, &mut parity);
        for j in 0..255 {
            block[j * depth + c] = if j < 223 {
                data[j]
            } else {
                Sym::from(dual.to_dual[parity[j - 223] as usize])
            };
        }
    }
    let data = block[..depth * 223].to_vec();
    (block, data)
}

/// CCSDS interleaving I = 4: a burst of 4t = 64 consecutive on-air symbols is 16 per codeword,
/// corrected; 65 puts 17 into one codeword, refused. The stripped output is the 892 data bytes
/// in on-air order (the transfer frame).
#[test]
fn ccsds_depth_4_corrects_a_64_symbol_burst_and_refuses_65() {
    let params = with(known()[0].params.clone(), json!({"depth": 4}));
    let (block, data) = ccsds_block(4, 3);
    let burst = |len: usize| {
        let mut b = block.clone();
        for s in &mut b[500..500 + len] {
            *s ^= 0xA5;
        }
        frame(&sym_bits(&b, 8))
    };
    let mut b = build("reed_solomon", params, PortType::Frames);
    let out = run_frames(b.as_mut(), &[burst(64), burst(65)], 1, false);
    assert_eq!(out[0].info.check, CrcStatus::Valid);
    assert_eq!(out[0].info.corrected_bits, 64 * 4);
    assert_eq!(out[0].bits, sym_bits(&data, 8));
    assert_eq!(out[0].bits.len(), 892 * 8);
    assert_eq!(out[1].info.check, CrcStatus::Invalid);
    // Three codewords of the second frame still decoded (16 errors each), one refused.
    assert_eq!(extra(b.as_ref(), "codewords_corrected"), 7.0);
    assert_eq!(extra(b.as_ref(), "codewords_bad"), 1.0);
}

/// Output and status are the same however the frames are chunked.
#[test]
fn output_is_invariant_to_chunking() {
    let params = with(known()[0].params.clone(), json!({"depth": 2}));
    let mut rng = Rng(9);
    let frames: Vec<Owned> = (0..23)
        .map(|i| {
            let (mut block, _) = ccsds_block(2, 100 + i);
            let errors = [0, 5, 16, 32, 33][i as usize % 5];
            corrupt(&mut block, 8, errors, &mut rng);
            frame(&sym_bits(&block, 8))
        })
        .collect();
    let run = |per_chunk: usize| {
        let mut b = build("reed_solomon", params.clone(), PortType::Frames);
        let out = run_frames(b.as_mut(), &frames, per_chunk, true);
        let s = b.status();
        (out, s.error_rate, s.items_in, s.items_out, s.extra)
    };
    let whole = run(frames.len());
    assert!(whole.0.iter().any(|f| f.info.check == CrcStatus::Invalid));
    assert!(whole.0.iter().any(|f| f.info.corrected_bits > 0));
    for per_chunk in [1, 2, 5, 7] {
        assert_eq!(run(per_chunk), whole, "{per_chunk} frames per chunk");
    }
}

/// `span` leaves its prefix and suffix alone, a partial trailing block passes through, and a
/// frame without one whole block is invalid (`frames_short`). `drop_invalid` is hot.
#[test]
fn span_partial_blocks_short_frames_and_hot_drop_invalid() {
    let kv = &known()[5]; // P25 RS(24,16,9): 144-bit blocks
    let cw = kv.codeword();
    let mut bad = cw.clone();
    corrupt(&mut bad, 6, 4, &mut Rng(4));
    let prefix = [1u8, 0, 1, 1, 0];
    let suffix = [0u8, 1, 1];
    let mut bits = prefix.to_vec();
    bits.extend(sym_bits(&bad, 6));
    bits.extend(sym_bits(&bad, 6));
    bits.extend([1u8; 20]); // partial third block
    bits.extend(suffix);
    let params = with(
        kv.params.clone(),
        json!({"span": {"start_bit": 5, "end_trim_bits": 3}}),
    );
    let mut b = build("reed_solomon", params.clone(), PortType::Frames);
    let short = frame(&bits[..100]);
    let out = run_frames(b.as_mut(), &[frame(&bits), short.clone()], 2, false);
    let mut want = prefix.to_vec();
    want.extend(sym_bits(&cw[..16], 6));
    want.extend(sym_bits(&cw[..16], 6));
    want.extend([1u8; 20]);
    want.extend(suffix);
    assert_eq!(out[0].info.check, CrcStatus::Valid);
    assert_eq!(out[0].bits, want);
    assert_eq!(out[1].info.check, CrcStatus::Invalid);
    assert_eq!(out[1].bits, short.bits, "a short frame passes unchanged");
    assert_eq!(extra(b.as_ref(), "frames_short"), 1.0);

    let dropping = with(params.clone(), json!({"drop_invalid": true}));
    let p = dropping.as_object().cloned().unwrap();
    let ctx_maps = std::collections::BTreeMap::new();
    let ctx = crate::registry::BuildCtx {
        field_maps: &ctx_maps,
        input_types: &[PortType::Frames],
    };
    assert_eq!(
        b.update_params(&p, &ctx).unwrap(),
        crate::block::ParamUpdate::Applied
    );
    let out = run_frames(b.as_mut(), &[short.clone(), frame(&bits)], 2, false);
    assert_eq!(out.len(), 1, "the short frame is dropped");
    assert_eq!(out[0].info.check, CrcStatus::Valid);
}

/// Parameters a code cannot have are refused at build.
#[test]
fn code_parameters_are_checked() {
    let base = json!({"n": 255, "k": 223, "poly": "0x187", "fcr": 112, "prim": 11});
    for (why, bad) in [
        (
            "AES's 0x11B is irreducible, not primitive",
            json!({"poly": "0x11B"}),
        ),
        ("degree above m", json!({"poly": "0x1187"})),
        ("k = n", json!({"k": 255})),
        ("n above 2^m − 1", json!({"n": 256})),
        ("prim shares 3 with 255", json!({"prim": 3})),
        (
            "dual basis outside the CCSDS field",
            json!({"poly": "0x11D", "prim": 1, "dual_basis": true}),
        ),
        ("depth 0", json!({"depth": 0})),
    ] {
        assert!(
            try_build("reed_solomon", with(base.clone(), bad), PortType::Frames).is_err(),
            "{why}"
        );
    }
    // Both polynomial forms name the same field.
    let short_form = with(base.clone(), json!({"poly": "0x87"}));
    assert!(try_build("reed_solomon", short_form, PortType::Frames).is_ok());
}

/// SIGNAL-034 (CCSDS telemetry; the same ladder as LRPT, HRIT/LRIT and HRPT): the whole
/// CCSDS 131.0-B receive order on synthesised IQ — BPSK with an RRC pulse, a carrier offset,
/// an unknown phase and a fractional timing offset — `psk_demod → viterbi → sync_search (ASM)
/// → descramble (0x1A9) → reed_solomon (dual basis)`. Two frames carry a burst of flipped
/// coded bits that the Viterbi decoder cannot absorb, so those frames reach RS with byte
/// errors; RS returns every frame's 223 data bytes exact and says which it corrected. Red
/// without RS: the de-randomised frames of the burst frames are wrong.
#[test]
fn signal_034_ccsds_tm_frames_decode_through_psk_viterbi_asm_derandomise_and_rs() {
    use super::trellis::{Code, encode};
    use crate::block::PortInfo;
    use crate::blocks::framing::common::P;
    use crate::blocks::iq::testkit::{Chain, Lcg, build as build_iq};
    use crate::buffer::PortVec;
    use num_complex::Complex32;
    use std::f64::consts::{PI, TAU};

    const FS: f64 = 48_000.0;
    const RSYM: f64 = 4_800.0;
    const SPS: f64 = FS / RSYM;
    const FRAMES: usize = 4;
    const BLOCK_BITS: usize = 255 * 8;
    let pn = json!({"mode": "additive", "poly": "0x1A9", "init": "0xFF"});

    // The CCSDS pseudo-randomiser (131.0-B §10) starts FF 48 0E C0.
    let mut scr = build("descramble", pn.clone(), PortType::Frames);
    let zeros = run_frames(scr.as_mut(), &[frame(&[0u8; 32])], 1, false);
    assert_eq!(zeros[0].bits, sym_bits(&[0xFF, 0x48, 0x0E, 0xC0], 8));

    let mut blocks = Vec::new();
    let mut datas = Vec::new();
    for f in 0..FRAMES {
        let (block, data) = ccsds_block(1, 340 + f as u64);
        blocks.push(frame(&sym_bits(&block, 8)));
        datas.push(sym_bits(&data, 8));
    }
    let randomised = run_frames(scr.as_mut(), &blocks, FRAMES, false);

    let mut rng = Lcg::new(611);
    let mut stream: Vec<u8> = (0..300).map(|_| rng.bit()).collect();
    let asm: Vec<u8> = (0..32)
        .rev()
        .map(|i| (0x1ACF_FC1Du32 >> i) as u8 & 1)
        .collect();
    let mut starts = Vec::new();
    for r in &randomised {
        stream.extend(&asm);
        starts.push(stream.len());
        stream.extend(&r.bits);
    }
    stream.extend((0..200).map(|_| rng.bit()));
    let conv = json!({"constraint_length": 7, "polys": ["0x4F", "0x6D"], "invert": [false, true]});
    let code = Code::from_params(P(conv.as_object().unwrap())).unwrap();
    let mut coded = encode(&code, &stream, 0);
    // Bursts of flipped coded bits (rate 1/2: coded index 2 × data index), mid frames 1 and 2.
    let burst_frames = [1usize, 2];
    for (&f, len) in burst_frames.iter().zip([12usize, 18]) {
        let at = 2 * (starts[f] + 900);
        for b in &mut coded[at..at + len] {
            *b ^= 1;
        }
    }

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
    let (tau, cfo, phase, ebn0_db) = (0.37, 350.0, 1.1, 6.0);
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
    let mut c = Chain::new(
        vec![
            build_iq(
                "psk_demod",
                json!({"modulation": "bpsk", "symbol_rate_bd": RSYM}),
                PortType::Iq,
            ),
            build_iq("viterbi", conv, PortType::Soft),
            build_iq(
                "sync_search",
                json!({"mode": "sync-word", "sync_word": "0x1ACFFC1D", "sync_bits": 32,
                       "max_errors": 3, "frame_bits": BLOCK_BITS, "polarity": "either"}),
                PortType::Bits,
            ),
            build_iq("descramble", pn, PortType::Frames),
            build_iq("reed_solomon", known()[0].params.clone(), PortType::Frames),
        ],
        info,
    );
    c.run(&PortVec::Iq(x), 1_000);
    let unpack = |b: &[u8], bits: usize| -> Vec<u8> {
        (0..bits).map(|i| (b[i / 8] >> (7 - i % 8)) & 1).collect()
    };
    let got: Vec<(Vec<u8>, CrcStatus, u32)> = c
        .out(4, 0)
        .frames
        .iter()
        .map(|(b, i)| (unpack(b, 223 * 8), i.check, i.corrected_bits))
        .collect();
    for (bits, check, _) in &got {
        assert_eq!(*check, CrcStatus::Valid);
        assert!(datas.contains(bits), "every valid frame is a sent frame");
    }
    for &f in &burst_frames {
        let hit = got
            .iter()
            .find(|g| g.0 == datas[f])
            .expect("burst frame decoded");
        assert!(hit.2 > 0, "frame {f}: RS corrected its burst");
    }
    assert!(got.len() >= FRAMES - 1, "{} of {FRAMES} frames", got.len());

    // Red without RS: before it, the burst frames' code blocks are wrong.
    let pre = &c.out(3, 0).frames;
    let clean: Vec<Vec<u8>> = blocks.iter().map(|b| b.bits.clone()).collect();
    let pre_exact = pre
        .iter()
        .filter(|(b, _)| clean.contains(&unpack(b, BLOCK_BITS)))
        .count();
    assert!(
        pre_exact + burst_frames.len() <= pre.len(),
        "the burst frames arrive at RS with errors ({pre_exact} of {} exact)",
        pre.len()
    );
}
