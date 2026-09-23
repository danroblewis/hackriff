//! T-608: `descramble`, the LFSR de-whitener (ADR-0011 §9.1). Every test builds the block
//! through `Registry::builtin()`, so each goes red without it.
//!
//! - Known vectors, one per real parameterisation: the CCSDS 131.0-B pseudo-randomiser
//!   (SIGNAL-034/027/025/028), PN9 in serial and CC1101 byte mode (checked bit-for-bit against
//!   hk-estimate `framing::whitening` too), and the BLE data whitener seeded from the channel
//!   index (AWARE-025/026) — against a literal model of the core spec's register drawing.
//! - The ADR-0011 §1.6 pair: chunking invariance (bits and frames) and a synthetic signal
//!   (whitened frames behind a sync word; a G3RUH-style multiplicative scrambler).
//! - RESEARCH-012: `status()` ranks the right polynomial over a whitened burst.

use std::collections::BTreeMap;

use hk_blocks::{
    Block, BuildCtx, ChunkFlags, ChunkMeta, FrameBuf, FrameInfo, Input, Io, Lock, Output, PortInfo,
    PortSlice, PortVec, Registry, Status,
};
use hk_estimate::framing::whitening::Whitening;
use hk_recipe::{Params, PortType};
use serde_json::{Value, json};

// ------------------------------------------------------------------------------- harness

fn try_build(params: Value, ty: PortType) -> Result<Box<dyn Block>, String> {
    let maps = BTreeMap::new();
    let ctx = BuildCtx {
        field_maps: &maps,
        input_types: &[ty],
    };
    let p: Params = params.as_object().cloned().unwrap();
    Registry::builtin()
        .build("descramble", &p, &ctx)
        .map_err(|e| e.to_string())
}

fn build(params: &Value, ty: PortType) -> Box<dyn Block> {
    try_build(params.clone(), ty).unwrap_or_else(|e| panic!("descramble {params}: {e}"))
}

fn port(ty: PortType, max_items: usize) -> PortInfo {
    PortInfo {
        ty,
        rate_hz: 1e6,
        max_items,
        hold_items: 0,
    }
}

/// Runs `bits` through a fresh block in chunks of the given sizes (cycled); the first chunk
/// is flagged DISCONTINUITY. Returns the output and the final status.
fn run_bits_sized(params: &Value, bits: &[u8], sizes: &[usize]) -> (Vec<u8>, Status) {
    let mut b = build(params, PortType::Bits);
    let max = sizes.iter().copied().max().unwrap();
    let info = b.init(&[port(PortType::Bits, max)]).unwrap();
    assert_eq!(info[0].ty, PortType::Bits, "bits in, bits out");
    let mut outputs = vec![Output::for_port(&info[0])];
    let (mut out, mut at, mut k) = (Vec::new(), 0, 0);
    while at < bits.len() {
        let n = sizes[k % sizes.len()].min(bits.len() - at);
        outputs[0].begin_chunk();
        let meta = ChunkMeta {
            index: at as u64,
            source_index: at as f64,
            source_per_item: 1.0,
            flags: if k == 0 {
                ChunkFlags::DISCONTINUITY
            } else {
                ChunkFlags::NONE
            },
            ..ChunkMeta::start(1e6)
        };
        let inputs = [Input {
            meta,
            data: PortSlice::Bits(&bits[at..at + n]),
        }];
        b.process(&mut Io::new(&inputs, &mut outputs)).unwrap();
        assert_eq!(outputs[0].meta.flags, meta.flags, "flags propagate");
        let PortVec::Bits(v) = &outputs[0].data else {
            panic!("bits output")
        };
        assert_eq!(v.len(), n, "one bit out per bit in");
        out.extend_from_slice(v);
        at += n;
        k += 1;
    }
    (out, b.status())
}

fn run_bits(params: &Value, bits: &[u8]) -> Vec<u8> {
    run_bits_sized(params, bits, &[bits.len().max(1)]).0
}

/// A frame: unpacked bits and channel.
#[derive(Clone, Debug, PartialEq)]
struct F {
    bits: Vec<u8>,
    channel: u16,
}

fn run_frames(params: &Value, frames: &[F], per_chunk: usize) -> (Vec<F>, Status) {
    let mut b = build(params, PortType::Frames);
    let info = b.init(&[port(PortType::Frames, per_chunk)]).unwrap();
    assert_eq!(info[0].ty, PortType::Frames, "frames in, frames out");
    let mut outputs = vec![Output::for_port(&info[0])];
    let mut out = Vec::new();
    for (k, c) in frames.chunks(per_chunk).enumerate() {
        let mut buf = FrameBuf::with_capacity(c.len(), 64);
        for (i, f) in c.iter().enumerate() {
            buf.push_bits(
                &f.bits,
                FrameInfo::new((k * per_chunk + i) as u64, 0, f.channel),
            );
        }
        outputs[0].begin_chunk();
        let meta = ChunkMeta {
            flags: if k == 0 {
                ChunkFlags::DISCONTINUITY
            } else {
                ChunkFlags::NONE
            },
            ..ChunkMeta::start(10.0)
        };
        let inputs = [Input {
            meta,
            data: PortSlice::Frames(&buf),
        }];
        b.process(&mut Io::new(&inputs, &mut outputs)).unwrap();
        let PortVec::Frames(got) = &outputs[0].data else {
            panic!("frames output")
        };
        for f in got.iter() {
            assert_eq!(f.info.channel, c[out.len() % per_chunk.max(1)].channel);
            out.push(F {
                bits: (0..f.info.bit_len).map(|i| f.bit(i).unwrap()).collect(),
                channel: f.info.channel,
            });
        }
    }
    assert_eq!(out.len(), frames.len(), "one frame out per frame in");
    (out, b.status())
}

fn pack_msb(bits: &[u8]) -> Vec<u8> {
    bits.chunks(8)
        .map(|c| c.iter().enumerate().fold(0, |a, (k, b)| a | b << (7 - k)))
        .collect()
}

fn pack_lsb(bits: &[u8]) -> Vec<u8> {
    bits.chunks(8)
        .map(|c| c.iter().enumerate().fold(0, |a, (k, b)| a | b << k))
        .collect()
}

fn unpack_msb(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .flat_map(|&b| (0..8).rev().map(move |k| (b >> k) & 1))
        .collect()
}

fn noise(n: usize, seed: u64) -> Vec<u8> {
    let mut s = seed | 1;
    (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 33) as u8 & 1
        })
        .collect()
}

fn xor(a: &[u8], b: &[u8]) -> Vec<u8> {
    a.iter().zip(b).map(|(x, y)| x ^ y).collect()
}

// --------------------------------------------------------------------- parameterisations

fn ccsds() -> Value {
    json!({ "mode": "additive", "poly": "0x1A9", "init": "0xFF" })
}

fn pn9_serial() -> Value {
    json!({ "mode": "additive", "poly": "0x221", "init": "0x1FF" })
}

fn pn9_cc1101() -> Value {
    json!({ "mode": "additive", "poly": "0x221", "init": "0x1FF", "bit_order": "byte-lsb" })
}

fn ble() -> Value {
    json!({ "mode": "additive", "poly": "0x91", "init": "0x40", "register": "galois",
            "seed_channel": true })
}

fn g3ruh() -> Value {
    // 1 + x^12 + x^17 as a delay polynomial is x^17 + x^5 + 1 in characteristic form.
    json!({ "mode": "multiplicative", "poly": "0x20021" })
}

/// The BLE whitener exactly as the core spec (Vol 6 Part B §3.2) draws it: positions 0..6,
/// position 0 = 1 and positions 1..6 = the channel index MSB first; each clock the output is
/// position 6, which shifts into position 0 and is XORed into the input of position 4.
fn ble_spec_mask(channel: u8, n: usize) -> Vec<u8> {
    let mut pos = [0u8; 7];
    pos[0] = 1;
    for (i, p) in pos[1..].iter_mut().enumerate() {
        *p = (channel >> (5 - i)) & 1;
    }
    (0..n)
        .map(|_| {
            let out = pos[6];
            let mut next = [0u8; 7];
            next[0] = out;
            next[1..].copy_from_slice(&pos[..6]);
            next[4] = pos[3] ^ out;
            pos = next;
            out
        })
        .collect()
}

// ------------------------------------------------------------------------- known vectors

/// SIGNAL-034/027/025/028 (CCSDS 131.0-B §10, the pseudo-randomiser every CCSDS family
/// shares: cubesats, Meteor-M LRPT, HRIT/LRIT, HRPT): `x^8+x^7+x^5+x^3+1`, all ones, period
/// 255, sequence `FF 48 0E C0 9A 0D 70 BC …`. Zeros in → the sequence out, per frame after
/// the 32-bit ASM, which passes through untouched.
#[test]
fn ccsds_pseudo_randomiser_known_vector() {
    let mask = run_bits(&ccsds(), &[0; 255 * 2]);
    assert_eq!(
        pack_msb(&mask[..64]),
        [0xFF, 0x48, 0x0E, 0xC0, 0x9A, 0x0D, 0x70, 0xBC]
    );
    assert_eq!(mask[..255], mask[255..], "period 255");
    assert_eq!(mask[..255].iter().filter(|&&b| b == 1).count(), 128);

    let asm = unpack_msb(&[0x1A, 0xCF, 0xFC, 0x1D]);
    let frame = F {
        bits: [asm.clone(), vec![0; 64]].concat(),
        channel: 0,
    };
    let params = json!({ "mode": "additive", "poly": "0x1A9", "init": "0xFF", "offset_bits": 32 });
    let (out, _) = run_frames(&params, &[frame.clone(), frame], 2);
    for f in &out {
        assert_eq!(f.bits[..32], asm[..], "the ASM is not randomised");
        assert_eq!(
            pack_msb(&f.bits[32..]),
            [0xFF, 0x48, 0x0E, 0xC0, 0x9A, 0x0D, 0x70, 0xBC],
            "the sequence restarts after every ASM"
        );
    }
}

/// PN9 `x^9+x^5+1`, seed `0x1FF`: TI DN509's CC1101 byte-mode sequence starts `FF E1 1D 9A`
/// (MSB-first bytes), and IEEE 802.15.4g's serial one is the same register LSB of each byte
/// first. Both agree bit for bit with hk-estimate's `framing::whitening` kernel.
#[test]
fn pn9_serial_and_cc1101_known_vectors() {
    let n = 511 * 8;
    let cc = run_bits(&pn9_cc1101(), &vec![0; n]);
    assert_eq!(pack_msb(&cc[..32]), [0xFF, 0xE1, 0x1D, 0x9A]);
    assert_eq!(cc, Whitening::Pn9Cc1101.sequence(n));

    let serial = run_bits(&pn9_serial(), &vec![0; n]);
    assert_eq!(pack_lsb(&serial[..32]), [0xFF, 0xE1, 0x1D, 0x9A]);
    assert_eq!(serial, Whitening::Pn9Serial.sequence(n));
    assert_eq!(serial[..511], serial[511..1022], "period 511");
    assert_ne!(serial, cc, "byte mode is a different parameterisation");
}

/// AWARE-025/026 (BLE advertising, and ASTM F3411 Remote ID over it): `x^7+x^4+1` seeded
/// from the channel index. Channel 37's sequence starts `8D D2 57 A1` (LSB-first, as BLE
/// sends bytes); every one of the 40 channels matches the spec's own register drawing; and
/// frames from different channels in one chunk each get their own channel's sequence.
#[test]
fn ble_whitener_seeded_from_the_channel_known_vector() {
    let zeros = |ch| F {
        bits: vec![0; 32],
        channel: ch,
    };
    let (out, _) = run_frames(&ble(), &[zeros(37)], 1);
    assert_eq!(pack_lsb(&out[0].bits), [0x8D, 0xD2, 0x57, 0xA1]);

    let frames: Vec<F> = (0..40)
        .map(|ch| F {
            bits: vec![0; 300],
            channel: ch,
        })
        .collect();
    let (out, _) = run_frames(&ble(), &frames, 7);
    for (ch, f) in out.iter().enumerate() {
        assert_eq!(f.bits, ble_spec_mask(ch as u8, 300), "channel {ch}");
    }
    assert_ne!(out[37].bits, out[38].bits);

    // A fixed seed, no channel: init is the register the spec draws (0x40 | 37 = 0x65).
    let fixed = json!({ "mode": "additive", "poly": "0x91", "init": "0x65", "register": "galois" });
    assert_eq!(run_bits(&fixed, &[0; 300]), ble_spec_mask(37, 300));
}

// -------------------------------------------------------------- ADR-0011 §1.6: chunking

/// Output is identical however the input is split: bit chunks of 1 bit to all at once, and
/// frame chunks of 1..all frames, for every parameterisation (including byte mode, whose
/// 8-bit groups straddle chunk edges, and the multiplicative register).
#[test]
fn chunking_invariance() {
    let bits = noise(5_000, 0x608);
    for params in [ccsds(), pn9_serial(), pn9_cc1101(), ble(), g3ruh()] {
        let whole = run_bits(&params, &bits);
        for sizes in [&[1][..], &[3], &[7, 1, 64], &[13, 250], &[4_999]] {
            let (got, _) = run_bits_sized(&params, &bits, sizes);
            assert_eq!(got, whole, "{params} split {sizes:?}");
        }
        let frames: Vec<F> = (0..12)
            .map(|k| F {
                bits: noise(40 + 37 * k, k as u64),
                channel: (k % 3) as u16 + 37,
            })
            .collect();
        for reset in ["per-frame", "free-running"] {
            let mut p = params.clone();
            p["reset"] = json!(reset);
            p["offset_bits"] = json!(16);
            let (whole, _) = run_frames(&p, &frames, frames.len());
            for per_chunk in [1, 2, 5] {
                assert_eq!(
                    run_frames(&p, &frames, per_chunk).0,
                    whole,
                    "{p} x{per_chunk}"
                );
            }
        }
    }
}

// ------------------------------------------------------ ADR-0011 §1.6: synthetic signals

/// A synthetic whitened burst train, as `sync_search` would hand it over: each frame is a
/// sync word then a payload whitened from a restarted register. Descrambling returns every
/// payload exactly, for each parameterisation; `free-running` (wrong for these frames) does
/// not — the reset rule is load-bearing.
#[test]
fn synthetic_whitened_frames_are_recovered() {
    let sync = unpack_msb(&[0x8E, 0x89, 0xBE, 0xD6]);
    for (params, mask_of) in [
        (
            ccsds(),
            Box::new(|_: u16, n| run_bits(&ccsds(), &vec![0; n]))
                as Box<dyn Fn(u16, usize) -> Vec<u8>>,
        ),
        (
            pn9_cc1101(),
            Box::new(|_, n| Whitening::Pn9Cc1101.sequence(n)),
        ),
        (ble(), Box::new(|ch, n| ble_spec_mask(ch as u8, n))),
    ] {
        let payloads: Vec<Vec<u8>> = (0..6).map(|k| noise(64 + 24 * k, 99 + k as u64)).collect();
        let frames: Vec<F> = payloads
            .iter()
            .enumerate()
            .map(|(k, p)| {
                let ch = 37 + (k % 3) as u16;
                F {
                    bits: [sync.clone(), xor(p, &mask_of(ch, p.len()))].concat(),
                    channel: ch,
                }
            })
            .collect();
        let mut p = params.clone();
        p["offset_bits"] = json!(32);
        let (out, _) = run_frames(&p, &frames, 4);
        for (f, want) in out.iter().zip(&payloads) {
            assert_eq!(f.bits[..32], sync[..], "{params}: sync untouched");
            assert_eq!(&f.bits[32..], &want[..], "{params}: payload recovered");
        }
        // With seed_channel a channel change reseeds anyway, and these frames alternate
        // channels; the reset rule is exercised by the fixed-seed parameterisations.
        if params.get("seed_channel").is_none() {
            p["reset"] = json!("free-running");
            let (free, _) = run_frames(&p, &frames, 4);
            assert_ne!(free[1].bits, out[1].bits, "{params}: reset rule matters");
        }
    }
}

/// The multiplicative (self-synchronising) mode against an independent G3RUH-style scrambler
/// `y[k] = x[k] ⊕ y[k−12] ⊕ y[k−17]`: exact recovery from a matching (zero) start, recovery
/// within 17 bits from an unknown start, and one channel bit error costs exactly three output
/// errors (the polynomial's weight) — the property that tells a multiplicative scrambler from
/// an additive one.
#[test]
fn synthetic_multiplicative_scrambler_self_synchronises() {
    let x = noise(3_000, 7);
    let mut y = vec![0u8; x.len()];
    for k in 0..x.len() {
        let d = |n: usize| if k >= n { y[k - n] } else { 0 };
        y[k] = x[k] ^ d(12) ^ d(17);
    }
    assert_eq!(run_bits(&g3ruh(), &y), x, "exact from the zero start");

    let mut seeded = g3ruh();
    seeded["init"] = json!("0x1ABCD");
    let got = run_bits(&seeded, &y);
    assert_eq!(got[17..], x[17..], "self-synchronised after n = 17 bits");

    let mut hit = y.clone();
    hit[1_000] ^= 1;
    let got = run_bits(&g3ruh(), &hit);
    let errs: Vec<usize> = (0..x.len()).filter(|&k| got[k] != x[k]).collect();
    assert_eq!(errs, [1_000, 1_012, 1_017]);
}

/// A DISCONTINUITY restarts the additive register (the stream lost its phase).
#[test]
fn discontinuity_restarts_the_register() {
    let mask = run_bits(&ccsds(), &[0; 100]);
    let mut b = build(&ccsds(), PortType::Bits);
    let info = b.init(&[port(PortType::Bits, 100)]).unwrap();
    let mut outputs = vec![Output::for_port(&info[0])];
    let mut chunks = Vec::new();
    for flags in [ChunkFlags::DISCONTINUITY, ChunkFlags::DISCONTINUITY] {
        outputs[0].begin_chunk();
        let zeros = [0u8; 100];
        let inputs = [Input {
            meta: ChunkMeta {
                flags,
                ..ChunkMeta::start(1e6)
            },
            data: PortSlice::Bits(&zeros),
        }];
        b.process(&mut Io::new(&inputs, &mut outputs)).unwrap();
        let PortVec::Bits(v) = &outputs[0].data else {
            panic!()
        };
        chunks.push(v.clone());
    }
    assert_eq!(chunks[0], mask);
    assert_eq!(chunks[1], mask);
}

// ------------------------------------------------------- RESEARCH-012: identification

/// RESEARCH-012 (whitening identification): over a burst that carries whitened constant fill
/// (preamble, padding, idle), `status()` locks and reports a near-zero `error_rate` only for
/// the polynomial that whitened it — whatever `init` the searcher guessed — and reads a coin
/// toss for the wrong polynomials and for unwhitened random data. So a search can rank
/// candidates by it.
#[test]
fn status_identifies_the_whitening_polynomial() {
    let fill = [
        run_bits(&pn9_serial(), &[0; 2_000]),
        run_bits(&pn9_serial(), &[1; 2_000])[..1_000].to_vec(),
    ]
    .concat();
    let score = |poly: &str, init: &str, bits: &[u8]| {
        let p = json!({ "mode": "additive", "poly": poly, "init": init });
        run_bits_sized(&p, bits, &[333]).1
    };
    let right = score("0x221", "0x5A", &fill);
    assert_eq!(right.lock, Lock::Locked);
    assert!(right.error_rate.unwrap() < 0.02, "{right:?}");
    assert!(right.quality.unwrap() > 0.95);
    for wrong in ["0x1A9", "0x91", "0x211", "0x20021"] {
        let s = score(wrong, "0x1", &fill);
        assert_eq!(s.lock, Lock::Searching, "{wrong}");
        assert!(s.error_rate.unwrap() > 0.3, "{wrong}: {s:?}");
    }
    let random = score("0x221", "0x1FF", &noise(3_000, 3));
    assert_eq!(random.lock, Lock::Searching);
    assert!(random.error_rate.unwrap() > 0.3);

    // Byte mode scores in mask order, so CC1101-whitened fill identifies too.
    let cc_fill = run_bits(&pn9_cc1101(), &[0; 2_048]);
    let s = run_bits_sized(&pn9_cc1101(), &cc_fill, &[100]).1;
    assert_eq!(s.lock, Lock::Locked);
    assert!(s.error_rate.unwrap() < 0.02);
}

// ---------------------------------------------------------------------- parameter rules

#[test]
fn inconsistent_parameters_are_refused() {
    for bad in [
        json!({ "mode": "additive", "poly": "0x1A9", "init": "0x0" }),
        json!({ "mode": "additive", "poly": "0x1A8" }),
        json!({ "mode": "additive", "poly": "0x1" }),
        json!({ "mode": "multiplicative", "poly": "0x91", "register": "galois" }),
        json!({ "mode": "multiplicative", "poly": "0x221", "bit_order": "byte-lsb" }),
        json!({ "mode": "multiplicative", "poly": "0x221", "seed_channel": true }),
        json!({ "poly": "0x221" }),
    ] {
        assert!(try_build(bad.clone(), PortType::Bits).is_err(), "{bad}");
    }
    // init defaults to all ones (additive): CCSDS without an explicit init.
    let default = json!({ "mode": "additive", "poly": "0x1A9" });
    assert_eq!(run_bits(&default, &[0; 64]), run_bits(&ccsds(), &[0; 64]));
}
