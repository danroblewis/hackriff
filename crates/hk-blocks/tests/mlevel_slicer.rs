//! T-612: `mlevel_slicer`, the M-ary symbol decision (ADR-0011 §9.1). Every test builds the
//! block through `Registry::builtin()`, so each goes red without it.
//!
//! - Label vectors: Gray/natural 4-level, the C4FM dibit table (P25 / DMR / NXDN: SIGNAL-080,
//!   SIGNAL-082, SIGNAL-084), FLEX 4-level, the 2G ALE 8-ary tribits (SIGNAL-070), `bit_order`,
//!   `invert`, and `levels: 2` = `slicer`.
//! - The ADR-0011 §1.6 pair: chunking invariance, and a synthetic 4-level eye at several SNRs
//!   with `thresholds: auto` converging (and tracking deviation/offset drift, which a fixed
//!   threshold does not survive).
//! - An all-one-level input (and noise) does NOT read as a confident decision.

use std::collections::BTreeMap;

use hk_blocks::{
    Block, BuildCtx, ChunkFlags, ChunkMeta, Input, Io, Lock, Output, ParamUpdate, PortInfo,
    PortSlice, PortVec, Registry, Status,
};
use hk_recipe::{Params, PortType};
use serde_json::{Value, json};

// ------------------------------------------------------------------------------- harness

fn params(v: &Value) -> Params {
    v.as_object().cloned().unwrap()
}

fn try_build_named(name: &str, p: &Value) -> Result<Box<dyn Block>, String> {
    let maps = BTreeMap::new();
    let ctx = BuildCtx {
        field_maps: &maps,
        input_types: &[PortType::Soft],
    };
    Registry::builtin()
        .build(name, &params(p), &ctx)
        .map_err(|e| e.to_string())
}

fn try_build(p: &Value) -> Result<Box<dyn Block>, String> {
    try_build_named("mlevel_slicer", p)
}

fn soft_port(max_items: usize) -> PortInfo {
    PortInfo {
        ty: PortType::Soft,
        rate_hz: 4_800.0,
        max_items,
        hold_items: 0,
    }
}

/// A block driven chunk by chunk.
struct Run {
    b: Box<dyn Block>,
    out: Output,
    at: usize,
    k: usize,
}

impl Run {
    fn new(name: &str, p: &Value, max: usize) -> Self {
        let mut b = try_build_named(name, p).unwrap_or_else(|e| panic!("{name} {p}: {e}"));
        let info = b.init(&[soft_port(max)]).unwrap();
        assert_eq!(info[0].ty, PortType::Bits, "soft in, bits out");
        let k = (info[0].rate_hz / 4_800.0).round() as usize;
        assert_eq!(info[0].max_items, max * k, "k bits per symbol of capacity");
        Self {
            b,
            out: Output::for_port(&info[0]),
            at: 0,
            k,
        }
    }

    fn feed(&mut self, x: &[f32], flags: ChunkFlags) -> Vec<u8> {
        self.out.begin_chunk();
        let meta = ChunkMeta {
            index: self.at as u64,
            source_index: 10.0 * self.at as f64,
            source_per_item: 10.0,
            flags,
            ..ChunkMeta::start(4_800.0)
        };
        let inputs = [Input {
            meta,
            data: PortSlice::Soft(x),
        }];
        self.b
            .process(&mut Io::new(&inputs, std::slice::from_mut(&mut self.out)))
            .unwrap();
        assert_eq!(self.out.meta.flags, flags, "flags propagate");
        assert_eq!(
            self.out.meta.source_per_item,
            10.0 / self.k as f64,
            "source_per_item is the symbol's ÷ k"
        );
        assert_eq!(self.out.meta.source_index, 10.0 * self.at as f64);
        self.at += x.len();
        let PortVec::Bits(v) = &self.out.data else {
            panic!("bits output")
        };
        assert_eq!(v.len(), x.len() * self.k, "k bits per symbol");
        v.clone()
    }
}

/// `x` through a fresh block in chunks of the given sizes (cycled; the first is flagged
/// DISCONTINUITY). Returns the bits and the final status.
fn run_sized(p: &Value, x: &[f32], sizes: &[usize]) -> (Vec<u8>, Status) {
    let max = sizes.iter().copied().max().unwrap();
    let mut r = Run::new("mlevel_slicer", p, max);
    let (mut out, mut at, mut c) = (Vec::new(), 0, 0);
    while at < x.len() {
        let n = sizes[c % sizes.len()].min(x.len() - at);
        let flags = if c == 0 {
            ChunkFlags::DISCONTINUITY
        } else {
            ChunkFlags::NONE
        };
        out.extend(r.feed(&x[at..at + n], flags));
        at += n;
        c += 1;
    }
    (out, r.b.status())
}

fn run(p: &Value, x: &[f32]) -> (Vec<u8>, Status) {
    run_sized(p, x, &[x.len().max(1)])
}

/// Groups `bits` into k-bit labels, MSB first.
fn labels(bits: &[u8], k: usize) -> Vec<u8> {
    bits.chunks(k)
        .map(|c| c.iter().fold(0, |a, &b| a << 1 | b))
        .collect()
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
    fn below(&mut self, n: usize) -> usize {
        ((self.next() * n as f64) as usize).min(n - 1)
    }
    fn gauss(&mut self) -> f64 {
        let (u, v) = (self.next().max(1e-300), self.next());
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }
}

fn extra(st: &Status, key: &str) -> f64 {
    st.extra
        .iter()
        .find(|(k, _)| *k == key)
        .unwrap_or_else(|| panic!("status extra {key}"))
        .1
}

fn gray(i: usize) -> u8 {
    (i ^ (i >> 1)) as u8
}

/// A C4FM-like symbol stream: level `i` → `centre + dev·(2i − (M−1))` plus Gaussian noise
/// `sigma`, the level spacing being `2·dev`.
fn mlevel(rng: &mut Rng, idx: &[usize], m: usize, centre: f64, dev: f64, sigma: f64) -> Vec<f32> {
    idx.iter()
        .map(|&i| (centre + dev * (2.0 * i as f64 - (m - 1) as f64) + sigma * rng.gauss()) as f32)
        .collect()
}

// ----------------------------------------------------------------------------- catalogue

/// The implemented block publishes the pinned ADR-0011 §9.1 row, parameters pinned.
#[test]
fn mlevel_slicer_is_registered_with_pinned_parameters() {
    let d = Registry::builtin()
        .get("mlevel_slicer")
        .expect("mlevel_slicer is registered")
        .descriptor()
        .clone();
    assert!(d.params_pinned);
    assert_eq!(
        hk_recipe::Catalogue::descriptor(&hk_blocks::catalogue::planned(), "mlevel_slicer"),
        Some(&d)
    );
    for key in [
        "levels",
        "thresholds",
        "fixed_levels",
        "window",
        "mapping",
        "table",
        "bit_order",
        "invert",
    ] {
        assert!(d.params.iter().any(|p| p.name == key), "{key} pinned");
    }
}

#[test]
fn bad_parameters_are_refused() {
    for (p, why) in [
        (json!({ "levels": 3 }), "power of two"),
        (
            json!({ "levels": 4, "thresholds": "fixed" }),
            "fixed_levels",
        ),
        (
            json!({ "levels": 4, "thresholds": "fixed", "fixed_levels": [0.0, -1.0, 1.0] }),
            "ascending",
        ),
        (
            json!({ "levels": 4, "thresholds": "fixed", "fixed_levels": [-1.0, 1.0] }),
            "levels − 1",
        ),
        (
            json!({ "levels": 4, "fixed_levels": [-1.0, 0.0, 1.0] }),
            "only with",
        ),
        (json!({ "levels": 4, "mapping": "table" }), "needs table"),
        (
            json!({ "levels": 4, "mapping": "table", "table": [0, 1, 1, 2] }),
            "permutation",
        ),
        (
            json!({ "levels": 4, "mapping": "table", "table": [0, 1, 2] }),
            "one label per level",
        ),
        (
            json!({ "levels": 4, "table": [0, 1, 2, 3] }),
            "only with mapping table",
        ),
    ] {
        let e = try_build(&p)
            .err()
            .unwrap_or_else(|| panic!("{p} accepted"));
        assert!(e.contains(why), "{p}: {e}");
    }
}

// ------------------------------------------------------------------------- label vectors

const FIXED4: [f32; 3] = [-2.0, 0.0, 2.0];
const C4FM: [f32; 4] = [-3.0, -1.0, 1.0, 3.0];

fn fixed4(extra: Value) -> Value {
    let mut p = json!({ "levels": 4, "thresholds": "fixed", "fixed_levels": FIXED4 });
    p.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    p
}

/// Gray: adjacent levels differ in one bit, lowest level first: −3 → 00, −1 → 01, +1 → 11,
/// +3 → 10 (FLEX's 4-level mapping). Natural is the level index.
#[test]
fn gray_and_natural_mapping_vectors() {
    let (b, _) = run(&fixed4(json!({})), &C4FM);
    assert_eq!(b, [0, 0, 0, 1, 1, 1, 1, 0], "gray, MSB first");
    let (b, _) = run(&fixed4(json!({ "mapping": "natural" })), &C4FM);
    assert_eq!(b, [0, 0, 0, 1, 1, 0, 1, 1], "natural");
    let (b, _) = run(&fixed4(json!({ "bit_order": "lsb" })), &C4FM);
    assert_eq!(b, [0, 0, 1, 0, 1, 1, 0, 1], "gray, LSB first");
    let (b, _) = run(&fixed4(json!({ "invert": true })), &C4FM);
    assert_eq!(labels(&b, 2), [2, 3, 1, 0], "invert mirrors the levels");
    // Every adjacent pair differs in exactly one bit, at every M.
    for m in [2usize, 4, 8, 16] {
        let x: Vec<f32> = (0..m).map(|i| i as f32).collect();
        let t: Vec<f32> = (1..m).map(|i| i as f32 - 0.5).collect();
        let p = json!({ "levels": m, "thresholds": "fixed", "fixed_levels": t });
        let (b, _) = run(&p, &x);
        let l = labels(&b, m.trailing_zeros() as usize);
        assert_eq!(l, (0..m).map(gray).collect::<Vec<_>>(), "M = {m}");
        assert!(l.windows(2).all(|w| (w[0] ^ w[1]).count_ones() == 1));
    }
}

/// SIGNAL-080 / SIGNAL-082 / SIGNAL-084: the C4FM dibit table P25 (TIA-102.BAAA), DMR (ETSI
/// TS 102 361-1 §10.2) and NXDN share: +3 → 01, +1 → 00, −1 → 10, −3 → 11.
#[test]
fn c4fm_dibit_table() {
    let p = fixed4(json!({ "mapping": "table", "table": [3, 2, 0, 1] }));
    let (b, _) = run(&p, &[3.0, 1.0, -1.0, -3.0]);
    assert_eq!(b, [0, 1, 0, 0, 1, 0, 1, 1]);
}

/// SIGNAL-070: 2G ALE (MIL-STD-188-141) 8-ary FSK, tones 750…2500 Hz in 250 Hz steps carry
/// Gray-coded tribits 000 001 011 010 110 111 101 100, lowest tone first. A discriminator
/// output in Hz goes straight in.
#[test]
fn ale_8ary_tribits() {
    let tones: Vec<f32> = (0..8).map(|i| 750.0 + 250.0 * i as f32).collect();
    let t: Vec<f32> = (1..8).map(|i| 625.0 + 250.0 * i as f32).collect();
    let (b, _) = run(
        &json!({ "levels": 8, "thresholds": "fixed", "fixed_levels": t }),
        &tones,
    );
    assert_eq!(
        labels(&b, 3),
        [0b000, 0b001, 0b011, 0b010, 0b110, 0b111, 0b101, 0b100]
    );
    // The same, estimated rather than told: auto on a noisy random tribit stream.
    let mut rng = Rng(8);
    let idx: Vec<usize> = (0..4_000).map(|_| rng.below(8)).collect();
    let x: Vec<f32> = idx
        .iter()
        .map(|&i| (750.0 + 250.0 * i as f64 + 18.0 * rng.gauss()) as f32)
        .collect();
    let (b, st) = run(&json!({ "levels": 8 }), &x);
    let got = labels(&b, 3);
    let errs = (256..idx.len()).filter(|&n| got[n] != gray(idx[n])).count();
    assert_eq!(errs, 0, "8-ary auto decisions after the first window");
    assert_eq!(st.lock, Lock::Locked);
}

/// `levels: 2` is `slicer` (threshold 0, invert included).
#[test]
fn two_levels_is_the_binary_slicer() {
    let mut rng = Rng(2);
    let x: Vec<f32> = (0..500).map(|_| (rng.next() * 2.0 - 1.0) as f32).collect();
    for invert in [false, true] {
        let mut ml = Run::new(
            "mlevel_slicer",
            &json!({ "levels": 2, "thresholds": "fixed", "fixed_levels": [0.0], "invert": invert }),
            x.len(),
        );
        let mut sl = Run::new("slicer", &json!({ "invert": invert }), x.len());
        assert_eq!(
            ml.feed(&x, ChunkFlags::DISCONTINUITY),
            sl.feed(&x, ChunkFlags::DISCONTINUITY),
            "invert {invert}"
        );
    }
}

// --------------------------------------------------------------------- auto on a real eye

/// A synthetic 4-level eye at several SNRs, with an arbitrary scale and DC offset (a
/// discriminator's units are Hz, not ±3): `auto` converges to the true levels, decides at the
/// Gaussian error rate, and its eye opening (`quality`, `lock`) tracks the SNR — open eyes
/// lock, a closing eye is reported as closing.
#[test]
fn auto_thresholds_converge_on_a_synthetic_4_level_eye() {
    let (centre, dev) = (-310.0, 600.0); // spacing 1200
    let spacing = 2.0 * dev;
    // (spacing / sigma, max symbol error rate, locked)
    for (ratio, max_ser, locked) in [
        (16.0, 0.0, true),
        (10.0, 1e-3, true),
        (7.0, 1.2e-2, true),
        (4.0, 8e-2, false),
    ] {
        let mut rng = Rng(ratio as u64);
        let n = 20_000;
        let idx: Vec<usize> = (0..n).map(|_| rng.below(4)).collect();
        let x = mlevel(&mut rng, &idx, 4, centre, dev, spacing / ratio);
        let (b, st) = run(&json!({ "levels": 4 }), &x);
        let got = labels(&b, 2);
        let errs = (256..n).filter(|&i| got[i] != gray(idx[i])).count();
        let ser = errs as f64 / (n - 256) as f64;
        assert!(ser <= max_ser, "s/σ {ratio}: SER {ser} > {max_ser}");
        let s = extra(&st, "level_spacing");
        let c = extra(&st, "level_centre");
        // Decision-directed fitting biases the spacing outward as the eye closes.
        let tol = if locked { 0.03 } else { 0.08 };
        assert!(
            (s - spacing).abs() < tol * spacing,
            "s/σ {ratio}: spacing {s}"
        );
        assert!(
            (c - centre).abs() < tol * spacing,
            "s/σ {ratio}: centre {c}"
        );
        assert_eq!(st.lock == Lock::Locked, locked, "s/σ {ratio}: {st:?}");
    }
}

/// Deviation and offset drift over the stream (the C4FM case the ticket names): `auto`
/// tracks it with no errors, while thresholds fixed at the start's levels break.
#[test]
fn auto_tracks_deviation_and_offset_drift_that_fixed_thresholds_do_not() {
    let mut rng = Rng(77);
    let n = 30_000;
    let idx: Vec<usize> = (0..n).map(|_| rng.below(4)).collect();
    let x: Vec<f32> = idx
        .iter()
        .enumerate()
        .map(|(k, &i)| {
            let f = k as f64 / n as f64;
            let dev = 1.0 * (1.0 - f) + 1.6 * f; // deviation +60 %
            let centre = 0.9 * f; // DC walks by almost a level
            (centre + dev * (2.0 * i as f64 - 3.0) + 0.06 * rng.gauss()) as f32
        })
        .collect();
    let (b, st) = run(&json!({ "levels": 4 }), &x);
    let got = labels(&b, 2);
    let errs = (256..n).filter(|&k| got[k] != gray(idx[k])).count();
    assert_eq!(errs, 0, "auto tracks the drift");
    assert_eq!(st.lock, Lock::Locked);
    let (b, _) = run(&fixed4(json!({})), &x);
    let got = labels(&b, 2);
    let errs = (256..n).filter(|&k| got[k] != gray(idx[k])).count();
    assert!(errs > n / 20, "fixed thresholds break under drift: {errs}");
}

/// P25's frame sync is ±3 only: a window with only the outer levels still fits the right
/// spacing (the equal-spacing constraint), so the inner levels that follow decide correctly.
#[test]
fn outer_levels_only_still_fit_all_four() {
    let mut rng = Rng(25);
    let mut idx: Vec<usize> = (0..400).map(|_| 3 * rng.below(2)).collect();
    idx.extend((0..2_000).map(|_| rng.below(4)));
    let x = mlevel(&mut rng, &idx, 4, 0.2, 1.0, 0.08);
    let (b, _) = run(&json!({ "levels": 4, "window": 128 }), &x);
    let got = labels(&b, 2);
    let bad: Vec<(usize, usize, u8, f32)> = (128..idx.len())
        .filter(|&k| got[k] != gray(idx[k]))
        .map(|k| (k, idx[k], got[k], x[k]))
        .collect();
    assert!(bad.is_empty(), "{bad:?}");
}

// ----------------------------------------------------------------------- not confident

/// One constant level — exact, with noise, or noise with no level structure at all — is not a
/// confident M-level decision: `quality` stays ~0 and `lock` never reads `locked`.
#[test]
fn an_all_one_level_input_is_not_confident() {
    let mut rng = Rng(1);
    let n = 5_000;
    let inputs: [(&str, Vec<f32>); 4] = [
        ("constant", vec![1.25; n]),
        (
            "constant + noise",
            (0..n).map(|_| (1.25 + 0.05 * rng.gauss()) as f32).collect(),
        ),
        (
            "gaussian noise",
            (0..n).map(|_| rng.gauss() as f32).collect(),
        ),
        ("uniform noise", (0..n).map(|_| rng.next() as f32).collect()),
    ];
    for m in [4usize, 8] {
        for (what, x) in &inputs {
            let (_, st) = run(&json!({ "levels": m }), x);
            assert_ne!(st.lock, Lock::Locked, "M {m}, {what}: {st:?}");
            assert!(
                st.quality.unwrap() < 0.05,
                "M {m}, {what}: quality {:?}",
                st.quality
            );
        }
    }
    // Fixed thresholds decide the same constant confidently-looking bits, but the status still
    // says the eye is shut.
    let (_, st) = run(&fixed4(json!({})), &inputs[1].1);
    assert_ne!(st.lock, Lock::Locked);
    // And the same block does lock on a real 4-level stream (the control).
    let idx: Vec<usize> = (0..n).map(|_| rng.below(4)).collect();
    let x = mlevel(&mut rng, &idx, 4, 1.25, 0.5, 0.05);
    let (_, st) = run(&json!({ "levels": 4 }), &x);
    assert_eq!(st.lock, Lock::Locked);
    assert!(st.quality.unwrap() > 0.5);
}

// ------------------------------------------------------------------ chunking and hot params

/// Output is independent of chunking (ADR-0011 §1.6), including across a mid-stream restart.
#[test]
fn chunking_invariance() {
    let mut rng = Rng(9);
    let n = 6_000;
    let idx: Vec<usize> = (0..n).map(|_| rng.below(4)).collect();
    let x = mlevel(&mut rng, &idx, 4, 0.3, 1.1, 0.25);
    for p in [
        json!({ "levels": 4 }),
        json!({ "levels": 4, "window": 100, "mapping": "table", "table": [3, 2, 0, 1] }),
        json!({ "levels": 8, "bit_order": "lsb", "invert": true }),
    ] {
        let (whole, st_whole) = run(&p, &x);
        for sizes in [&[1usize][..], &[7, 1, 300, 64], &[1_000, 3]] {
            let (chunked, st) = run_sized(&p, &x, sizes);
            assert_eq!(chunked, whole, "{p} in chunks {sizes:?}");
            assert_eq!(st.quality, st_whole.quality, "{p} in chunks {sizes:?}");
        }
    }
}

#[test]
fn a_restart_is_a_fresh_block_and_hot_params_apply_in_place() {
    let mut rng = Rng(4);
    let idx: Vec<usize> = (0..1_000).map(|_| rng.below(4)).collect();
    let x = mlevel(&mut rng, &idx, 4, 5.0, 2.0, 0.1);
    let y = mlevel(&mut rng, &idx, 4, -7.0, 0.5, 0.02);
    let p = json!({ "levels": 4 });
    let mut r = Run::new("mlevel_slicer", &p, 1_000);
    r.feed(&x, ChunkFlags::DISCONTINUITY);
    let after = r.feed(&y, ChunkFlags::DISCONTINUITY);
    let mut fresh = Run::new("mlevel_slicer", &p, 1_000);
    assert_eq!(after, fresh.feed(&y, ChunkFlags::DISCONTINUITY));

    // invert is hot: applied in place, the fit kept.
    let maps = BTreeMap::new();
    let ctx = BuildCtx {
        field_maps: &maps,
        input_types: &[PortType::Soft],
    };
    let hot = json!({ "levels": 4, "invert": true });
    assert_eq!(
        r.b.update_params(&params(&hot), &ctx).unwrap(),
        ParamUpdate::Applied
    );
    let inv = r.feed(&y, ChunkFlags::NONE);
    let want: Vec<u8> = idx.iter().map(|&i| gray(3 - i)).collect();
    assert_eq!(labels(&inv, 2), want);
    // levels / thresholds / window are cold.
    for cold in [json!({ "levels": 8 }), json!({ "levels": 4, "window": 64 })] {
        assert_eq!(
            r.b.update_params(&params(&cold), &ctx).unwrap(),
            ParamUpdate::Rebuild,
            "{cold}"
        );
    }
    // A bad hot update is refused, not half-applied.
    let bad = json!({ "levels": 4, "mapping": "table", "table": [0, 0, 1, 2] });
    assert!(r.b.update_params(&params(&bad), &ctx).is_err());
}
