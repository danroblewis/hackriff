//! `Block::evidence` through `run_window` (ADR-0015 §2.1, §3.1; T-853 = MAUTO M-2).
//!
//! Each case runs a recipe over a window it built, once with a signal and once with the
//! block's null, and asserts on the `hk_model::synth::Evidence` records — stage, metric, group,
//! support and (for analytic metrics) bits — never on block internals.

use hk_blocks::{
    Evidence, GroupId, MetricId, PortSlice, PortVec, Registry, Stage, WindowError, run_window,
};
use hk_recipe::Recipe;
use num_complex::Complex32;
use serde_json::{Value, json};

fn recipe(input: Value, nodes: Value) -> Recipe {
    let last = nodes.as_array().unwrap().last().unwrap()["id"].clone();
    serde_json::from_value(json!({
        "schema": "hackriff.recipe", "schema_version": 2,
        "id": "t853", "version": 1, "name": "evidence",
        "input": input,
        "nodes": nodes,
        "outputs": [ { "id": "tail", "kind": "stage", "from": last } ],
        "output_policy": { "content_class": "metadata-only" }
    }))
    .expect("recipe parses")
}

/// A reproducible generator (xorshift64*), uniform and Gaussian.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn uniform(&mut self) -> f64 {
        ((self.next() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }
    fn gauss(&mut self) -> f64 {
        (-2.0 * self.uniform().ln()).sqrt() * (std::f64::consts::TAU * self.uniform()).cos()
    }
    fn bit(&mut self) -> u8 {
        (self.next() >> 63) as u8
    }
}

fn find(ev: &[Evidence], metric: MetricId) -> Evidence {
    *ev.iter()
        .find(|e| e.metric == metric)
        .unwrap_or_else(|| panic!("no {metric:?} in {ev:?}"))
}

fn node_evidence(run: &hk_blocks::WindowRun, id: &str) -> Vec<Evidence> {
    let n = run.nodes.iter().find(|n| n.id == id).expect("node");
    assert!(n.evidence.len() <= 4, "≤ 4 entries per block (§2.1)");
    n.evidence.iter().copied().collect()
}

/// A 2-FSK burst (±`dev` Hz, `sps` samples per symbol) of random symbols plus complex noise.
fn fsk(n: usize, fs: f64, dev: f64, sps: usize, noise: f64, rng: &mut Rng) -> Vec<Complex32> {
    let mut ph = 0.0f64;
    let mut sym = 1.0;
    (0..n)
        .map(|i| {
            if i % sps == 0 {
                sym = if rng.bit() == 1 { 1.0 } else { -1.0 };
            }
            ph += std::f64::consts::TAU * sym * dev / fs;
            let z = Complex32::new(ph.cos() as f32, ph.sin() as f32);
            z + Complex32::new((noise * rng.gauss()) as f32, (noise * rng.gauss()) as f32)
        })
        .collect()
}

fn noise(n: usize, rng: &mut Rng) -> Vec<Complex32> {
    (0..n)
        .map(|_| Complex32::new(rng.gauss() as f32, rng.gauss() as f32))
        .collect()
}

fn fsk_ladder() -> Recipe {
    recipe(
        json!({ "port": "iq", "sample_rate_hz": 48000, "bandwidth_hz": 12000 }),
        json!([
            { "id": "chan", "block": "lowpass", "params": { "cutoff_hz": 6000, "transition_hz": 3000 } },
            { "id": "fsk", "block": "fsk_demod" },
            { "id": "clock", "block": "clock_recovery",
              "params": { "symbol_rate_bd": 4800, "pulse": "nrz", "algorithm": "gardner" } },
            { "id": "slice", "block": "slicer" }
        ]),
    )
}

#[test]
fn the_fsk_ladder_reports_s0_to_s3_and_signal_beats_noise_at_every_stage() {
    let registry = Registry::builtin();
    let r = fsk_ladder();
    let mut rng = Rng(0x7853);
    let sig = fsk(48_000, 48_000.0, 2_400.0, 10, 0.05, &mut rng);
    let nul = noise(48_000, &mut rng);
    let a = run_window(&r, &registry, 48_000.0, PortSlice::Iq(&sig), 4096).unwrap();
    let b = run_window(&r, &registry, 48_000.0, PortSlice::Iq(&nul), 4096).unwrap();

    let s0 = |run| find(&node_evidence(run, "chan"), MetricId::Snr);
    let (sa, sb) = (s0(&a), s0(&b));
    assert_eq!(sa.stage, Stage::S0);
    assert!(
        sa.raw > sb.raw + 3.0,
        "in-band excess {} vs {}",
        sa.raw,
        sb.raw
    );
    assert!(
        sb.raw.abs() < 0.5,
        "white noise passes exactly its noise gain: {}",
        sb.raw
    );
    assert_eq!(sa.n, 48_000);

    let s1 = |run| find(&node_evidence(run, "fsk"), MetricId::Bimodality);
    let (fa, fb) = (s1(&a), s1(&b));
    assert_eq!((fa.stage, fa.group), (Stage::S1, GroupId::DemodShape));
    assert!(
        fa.raw > 0.5 && fb.raw < 0.3,
        "bimodality {} vs {}",
        fa.raw,
        fb.raw
    );

    let ca = node_evidence(&a, "clock");
    let cb = node_evidence(&b, "clock");
    let (ea, eb) = (find(&ca, MetricId::EyeOpen), find(&cb, MetricId::EyeOpen));
    assert_eq!((ea.stage, ea.group), (Stage::S2, GroupId::Eye));
    assert!(ea.raw > eb.raw, "eye {} vs {}", ea.raw, eb.raw);
    let (ta, tb) = (
        find(&ca, MetricId::TimingVar),
        find(&cb, MetricId::TimingVar),
    );
    assert_eq!(ta.group, GroupId::SoftQuality);
    assert!(
        ta.raw < tb.raw,
        "timing variance is evidence when small: {} vs {}",
        ta.raw,
        tb.raw
    );
    assert!(
        (4700..=4900).contains(&ea.n),
        "support is symbols: {}",
        ea.n
    );

    let s3 = find(&node_evidence(&a, "slice"), MetricId::BitStructure);
    assert_eq!((s3.stage, s3.group), (Stage::S3, GroupId::BitShape));

    // Calibrated metrics leave bits to hk-synth's table: 0.0, the NoTable answer.
    for run in [&a, &b] {
        for n in &run.nodes {
            for e in n.evidence.iter() {
                assert!(!e.metric.is_analytic());
                assert_eq!(e.bits, 0.0, "{} {:?}", n.id, e.metric);
            }
        }
    }
    // The window's items reached the recipe output.
    let (id, bits) = &a.outputs[0];
    assert_eq!(id, "tail");
    assert!(matches!(bits, PortVec::Bits(b) if b.len() == ea.n as usize));
}

/// CRC-16/XMODEM (poly 0x1021, init 0) over MSB-first bits.
fn crc16(bits: &[u8]) -> u16 {
    let mut r = 0u16;
    for &b in bits {
        let top = ((r >> 15) as u8) ^ b;
        r <<= 1;
        if top == 1 {
            r ^= 0x1021;
        }
    }
    r
}

const SYNC: u16 = 0x2dd4;

/// `frames` frames of sync + 48 payload bits + CRC-16, separated by random gap bits.
fn framed(frames: usize, rng: &mut Rng) -> Vec<u8> {
    let mut out = Vec::new();
    for _ in 0..frames {
        for _ in 0..40 {
            out.push(rng.bit());
        }
        out.extend((0..16).rev().map(|k| ((SYNC >> k) & 1) as u8));
        let payload: Vec<u8> = (0..48).map(|_| rng.bit()).collect();
        let c = crc16(&payload);
        out.extend_from_slice(&payload);
        out.extend((0..16).rev().map(|k| ((c >> k) & 1) as u8));
    }
    out
}

fn framing_recipe() -> Recipe {
    recipe(
        json!({ "port": "bits" }),
        json!([
            { "id": "sync", "block": "sync_search",
              "params": { "mode": "sync-word", "sync_word": "0x2DD4", "sync_bits": 16, "frame_bits": 64 } },
            { "id": "check", "block": "crc", "params": { "width": 16, "poly": "0x1021" } }
        ]),
    )
}

#[test]
fn sync_and_check_evidence_are_analytic_and_zero_on_random_bits() {
    let registry = Registry::builtin();
    let r = framing_recipe();
    let mut rng = Rng(0x2dd4);
    let bits = framed(12, &mut rng);
    let run = run_window(&r, &registry, 1200.0, PortSlice::Bits(&bits), 256).unwrap();

    let sync = find(&node_evidence(&run, "sync"), MetricId::SyncExcess);
    assert_eq!(sync.stage, Stage::S4);
    assert!(sync.raw >= 12.0, "every sync found: {}", sync.raw);
    assert!(
        sync.bits > 10.0,
        "12 syncs of a 16-bit word in ~1.4k positions: {}",
        sync.bits
    );

    let check = find(&node_evidence(&run, "check"), MetricId::CheckDistinctValid);
    assert_eq!(check.stage, Stage::S5);
    assert_eq!(check.raw, 12.0, "12 distinct clean frames");
    // Every tested frame valid: 16 bits each, less the few chance syncs in the gaps.
    assert!(
        check.bits > 16.0 * 10.0,
        "{} over {} tested",
        check.bits,
        check.n
    );

    // The null: random bits sync by chance and never pass the check.
    let random: Vec<u8> = (0..bits.len()).map(|_| rng.bit()).collect();
    let run = run_window(&r, &registry, 1200.0, PortSlice::Bits(&random), 256).unwrap();
    let sync = node_evidence(&run, "sync");
    assert!(sync.iter().all(|e| e.bits == 0.0), "{sync:?}");
    let check = node_evidence(&run, "check");
    assert!(check.iter().all(|e| e.bits == 0.0), "{check:?}");
}

#[test]
fn a_repeated_frame_counts_once() {
    let registry = Registry::builtin();
    let r = framing_recipe();
    let mut rng = Rng(9);
    let one = framed(1, &mut rng);
    let bits: Vec<u8> = one.iter().copied().cycle().take(one.len() * 8).collect();
    let run = run_window(&r, &registry, 1200.0, PortSlice::Bits(&bits), 97).unwrap();
    let check = find(&node_evidence(&run, "check"), MetricId::CheckDistinctValid);
    assert_eq!(check.raw, 1.0, "a beacon repeating one payload is one fact");
    assert!(check.bits <= 16.0, "{}", check.bits);
}

#[test]
fn manchester_line_violations_separate_coded_chips_from_random_ones() {
    let registry = Registry::builtin();
    let r = recipe(
        json!({ "port": "bits" }),
        json!([{ "id": "man", "block": "manchester", "params": { "align": "fixed" } }]),
    );
    let mut rng = Rng(5);
    let coded: Vec<u8> = (0..2000)
        .flat_map(|_| {
            let b = rng.bit();
            [b, 1 - b]
        })
        .collect();
    let random: Vec<u8> = (0..4000).map(|_| rng.bit()).collect();
    let a = run_window(&r, &registry, 2400.0, PortSlice::Bits(&coded), 512).unwrap();
    let b = run_window(&r, &registry, 2400.0, PortSlice::Bits(&random), 512).unwrap();
    let (va, vb) = (
        find(&node_evidence(&a, "man"), MetricId::LineViolations),
        find(&node_evidence(&b, "man"), MetricId::LineViolations),
    );
    assert_eq!((va.stage, va.group), (Stage::S3, GroupId::BitShape));
    assert!(
        va.raw < 0.01 && (vb.raw - 0.5).abs() < 0.05,
        "{} vs {}",
        va.raw,
        vb.raw
    );
    assert_eq!(va.n, 2000);
    // Both S3 metrics share one declared group (ρ 0.706, ADR-0015 §13.1).
    let s = find(&node_evidence(&a, "man"), MetricId::BitStructure);
    assert_eq!(s.group, GroupId::BitShape);
}

#[test]
fn psk_demod_reports_its_s1_evm() {
    // ADR-0015 §10 M-14: psk_demod's S1 metric is owed by M-2.
    let registry = Registry::builtin();
    let r = recipe(
        json!({ "port": "iq", "sample_rate_hz": 19200, "bandwidth_hz": 4800 }),
        json!([{ "id": "psk", "block": "psk_demod",
                 "params": { "modulation": "bpsk", "symbol_rate_bd": 2400 } }]),
    );
    let mut rng = Rng(77);
    let sps = 8;
    let mut sym = 1.0f32;
    let sig: Vec<Complex32> = (0..38_400)
        .map(|i| {
            if i % sps == 0 {
                sym = if rng.bit() == 1 { 1.0 } else { -1.0 };
            }
            Complex32::new(sym, 0.0)
                + Complex32::new(0.02 * rng.gauss() as f32, 0.02 * rng.gauss() as f32)
        })
        .collect();
    let nul = noise(38_400, &mut rng);
    let a = run_window(&r, &registry, 19_200.0, PortSlice::Iq(&sig), 4096).unwrap();
    let b = run_window(&r, &registry, 19_200.0, PortSlice::Iq(&nul), 4096).unwrap();
    let (ea, eb) = (
        find(&node_evidence(&a, "psk"), MetricId::Evm),
        find(&node_evidence(&b, "psk"), MetricId::Evm),
    );
    assert_eq!(ea.stage, Stage::S1);
    assert!(
        ea.raw < eb.raw,
        "EVM is evidence when small: {} vs {}",
        ea.raw,
        eb.raw
    );
    assert!(ea.n > 1000, "support is symbols: {}", ea.n);
}

#[test]
fn a_block_without_evidence_reports_nothing_and_fails_nothing() {
    // `Block::evidence` is optional (ADR-0015 §2.1): the default emits no entry.
    let registry = Registry::builtin();
    let r = recipe(
        json!({ "port": "bits" }),
        json!([{ "id": "id", "block": "identity" }]),
    );
    let bits = [1u8, 0, 1];
    let run = run_window(&r, &registry, 1.0, PortSlice::Bits(&bits), 2).unwrap();
    assert!(run.nodes[0].evidence.is_empty());
    assert_eq!(run.nodes[0].block_version(), "identity@1");
}

#[test]
fn a_wrong_input_type_and_an_invalid_recipe_are_refused() {
    let registry = Registry::builtin();
    let r = framing_recipe();
    let iq = [Complex32::new(0.0, 0.0)];
    assert_eq!(
        run_window(&r, &registry, 1.0, PortSlice::Iq(&iq), 1).unwrap_err(),
        WindowError::InputType
    );
    let mut bad = framing_recipe();
    bad.nodes[0].block = "no_such_block".into();
    assert!(matches!(
        run_window(&bad, &registry, 1.0, PortSlice::Bits(&[0]), 1),
        Err(WindowError::Invalid(_))
    ));
}
