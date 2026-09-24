//! `psk_demod` (T-609) tests. Signals are synthesised here from the textbook constellations
//! (not through liquid), so a test cannot pass by the transmitter and receiver sharing a
//! convention by accident. Every test builds the block through the registry, so each one goes
//! red when `psk_demod` is not registered.

use std::f64::consts::{PI, TAU};

use hk_recipe::PortType;
use num_complex::{Complex32, Complex64};
use serde_json::{Value, json};

use super::super::testkit::*;
use crate::block::{BlockError, PortInfo};
use crate::buffer::{ChunkFlags, PortVec};
use crate::registry::{BuildCtx, Registry};
use crate::status::Lock;

const FS: f64 = 48_000.0;
const RS: f64 = 4_800.0;
const SPS: f64 = FS / RS;

#[derive(Clone, Copy)]
enum Shape {
    Rrc(f64),
    HalfSine,
}

/// One synthetic transmission.
#[derive(Clone, Copy)]
struct Tx {
    modulation: &'static str,
    symbols: usize,
    es_n0_db: f64,
    cfo_hz: f64,
    phase: f64,
    /// Timing offset, fraction of a symbol.
    tau: f64,
    shape: Shape,
    seed: u64,
}

impl Tx {
    fn new(modulation: &'static str, es_n0_db: f64) -> Self {
        Self {
            modulation,
            symbols: 6_000,
            es_n0_db,
            cfo_hz: 600.0,
            phase: 1.1,
            tau: 0.37,
            shape: Shape::Rrc(0.35),
            seed: 7,
        }
    }
}

fn bits_per_symbol(m: &str) -> usize {
    match m {
        "bpsk" | "dbpsk" => 1,
        "8psk" | "d8psk" => 3,
        _ => 2,
    }
}

fn rrc(t: f64, a: f64) -> f64 {
    if t.abs() < 1e-9 {
        return 1.0 - a + 4.0 * a / PI;
    }
    if (t.abs() - 1.0 / (4.0 * a)).abs() < 1e-9 {
        let x = PI / (4.0 * a);
        return a / 2f64.sqrt() * ((1.0 + 2.0 / PI) * x.sin() + (1.0 - 2.0 / PI) * x.cos());
    }
    let num = (PI * t * (1.0 - a)).sin() + 4.0 * a * t * (PI * t * (1.0 + a)).cos();
    num / (PI * t * (1.0 - (4.0 * a * t).powi(2)))
}

fn pulse(shape: Shape, u: f64) -> f64 {
    match shape {
        Shape::Rrc(a) => {
            if u.abs() > 8.0 {
                0.0
            } else {
                rrc(u, a)
            }
        }
        Shape::HalfSine => {
            if u.abs() < 0.5 {
                (PI * u).cos()
            } else {
                0.0
            }
        }
    }
}

/// Transmits random bits: returns (bits, samples at `FS`). Symbol `n`'s pulse is centred at
/// source sample `(n + tau) · SPS`.
fn transmit(tx: Tx) -> (Vec<u8>, Vec<Complex32>) {
    let mut rng = Lcg::new(tx.seed);
    let kb = bits_per_symbol(tx.modulation);
    let m = 1usize << kb;
    let bits: Vec<u8> = (0..tx.symbols * kb).map(|_| rng.bit()).collect();
    // Label → constellation index (inverse Gray).
    let index = |label: usize| {
        let mut i = label;
        let mut s = label >> 1;
        while s > 0 {
            i ^= s;
            s >>= 1;
        }
        i
    };
    let n_samples = ((tx.symbols as f64 + 1.0) * SPS) as usize;
    // Symbol values on the I and Q rails (OQPSK) or as complex points.
    let mut pts = Vec::with_capacity(tx.symbols);
    let mut acc = 0.0f64;
    let diff_phi0 = if tx.modulation == "pi4-dqpsk" {
        PI / 4.0
    } else {
        0.0
    };
    for n in 0..tx.symbols {
        let label = bits[n * kb..(n + 1) * kb]
            .iter()
            .fold(0usize, |a, &b| a << 1 | usize::from(b));
        let i = index(label) as f64;
        let p = match tx.modulation {
            "oqpsk" => Complex32::new(
                (1.0 - 2.0 * f64::from(bits[2 * n])) as f32,
                (1.0 - 2.0 * f64::from(bits[2 * n + 1])) as f32,
            ),
            "dbpsk" | "dqpsk" | "pi4-dqpsk" | "d8psk" => {
                acc += diff_phi0 + TAU * i / m as f64;
                Complex32::from_polar(1.0, acc as f32)
            }
            _ => Complex32::from_polar(1.0, (PI / 4.0 + TAU * i / m as f64) as f32),
        };
        pts.push(p);
    }
    let offset_q = if tx.modulation == "oqpsk" { 0.5 } else { 0.0 };
    let mut x = vec![Complex32::new(0.0, 0.0); n_samples];
    for (s, v) in x.iter_mut().enumerate() {
        let t = s as f64 / SPS - tx.tau;
        let lo = (t - 9.0).floor().max(0.0) as usize;
        let hi = ((t + 9.0).ceil() as usize).min(tx.symbols);
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (n, p) in pts.iter().enumerate().take(hi).skip(lo) {
            re += f64::from(p.re) * pulse(tx.shape, t - n as f64);
            im += f64::from(p.im) * pulse(tx.shape, t - n as f64 - offset_q);
        }
        *v = Complex32::new(re as f32, im as f32);
    }
    let power = x.iter().map(|z| f64::from(z.norm_sqr())).sum::<f64>() / x.len() as f64;
    let noise = power * SPS / 10f64.powf(tx.es_n0_db / 10.0);
    for (s, v) in x.iter_mut().enumerate() {
        let ph = TAU * tx.cfo_hz * s as f64 / FS + tx.phase;
        *v = *v * Complex32::from_polar(1.0, ph as f32) + rng.cnoise(noise);
    }
    (bits, x)
}

fn with(base: Value, extra: Value) -> Value {
    let mut p = base;
    p.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    p
}

fn run(params: &Value, x: &[Complex32], chunk: usize) -> Chain {
    let info = PortInfo {
        ty: PortType::Iq,
        rate_hz: FS,
        max_items: chunk,
        hold_items: 0,
    };
    let mut c = Chain::new(vec![build("psk_demod", params.clone(), PortType::Iq)], info);
    c.run(&PortVec::Iq(x.to_vec()), chunk);
    c
}

/// Bit errors of `got` against `truth` after `skip` truth bits, at the best alignment
/// `got[i + lag] ~ truth[i]` (the tracker's group delay is unknown to the test, and a tracker
/// that starts emitting only once its filter is full leads rather than lags). Returns (errors,
/// compared, lag in bits).
fn ber(truth: &[u8], got: &[u8], skip: usize) -> (usize, usize, isize) {
    let mut best = (usize::MAX, 0, 0);
    for lag in -400isize..400 {
        let lo = skip.max((-lag).max(0) as usize);
        let hi = truth.len().min((got.len() as isize - lag).max(0) as usize);
        if hi <= lo + 512 {
            continue;
        }
        let at = |i: usize| got[(i as isize + lag) as usize];
        let probe = (lo..lo + 512).filter(|&i| truth[i] != at(i)).count();
        if probe * 4 > 512 {
            continue;
        }
        let errs = (lo..hi).filter(|&i| truth[i] != at(i)).count();
        if errs < best.0 {
            best = (errs, hi - lo, lag);
        }
    }
    best
}

fn hard(c: &Chain) -> Vec<u8> {
    c.out(0, 0)
        .real
        .iter()
        .map(|&v| u8::from(v > 0.0))
        .collect()
}

/// Rotations a blind receiver must try for `m`'s phase ambiguity.
fn rotations(modulation: &str) -> Vec<i64> {
    match modulation {
        "bpsk" => vec![0, 180],
        "qpsk" | "oqpsk" => vec![0, 90, 180, 270],
        "8psk" => (0..8).map(|i| i * 45).collect(),
        _ => vec![0],
    }
}

/// Best (BER, lag, rotation) over the phase ambiguity, as the refinement loop would search it.
fn best_ber(params: &Value, tx: Tx) -> (f64, isize, i64, Chain) {
    let (truth, x) = transmit(tx);
    let skip = 2_500 * bits_per_symbol(tx.modulation);
    let mut best: Option<(f64, isize, i64, Chain)> = None;
    for rot in rotations(tx.modulation) {
        let p = with(params.clone(), json!({"rotation_deg": rot}));
        let c = run(&p, &x, 1_000);
        let (e, n, lag) = ber(&truth, &hard(&c), skip);
        let rate = if n == 0 { 1.0 } else { e as f64 / n as f64 };
        if best.as_ref().is_none_or(|b| rate < b.0) {
            best = Some((rate, lag, rot, c));
        }
    }
    best.unwrap()
}

/// SIGNAL-034/033/024/027/025/028/019/004/054/069, SPACE-081: every constellation the family
/// list needs, recovered blind through a 600 Hz carrier offset (0.125 × the symbol rate: 25×
/// liquid's unaided 8PSK pull-in), a 1.1 rad phase, a 0.37-symbol timing offset and a
/// non-integer resampling ratio (10 → 3 samples/symbol), at the stated Es/N0.
#[test]
fn every_constellation_recovers_blind_at_stated_snr_cfo_and_timing_offset() {
    let cases: [(&str, f64, f64); 7] = [
        ("bpsk", 9.0, 1e-3),
        ("dbpsk", 11.0, 1e-3),
        ("qpsk", 13.0, 1e-3),
        ("dqpsk", 15.0, 2e-3),
        ("pi4-dqpsk", 15.0, 2e-3),
        ("8psk", 19.0, 2e-3),
        ("d8psk", 21.0, 3e-3),
    ];
    let mut report = Vec::new();
    for (m, snr, limit) in cases {
        let params = json!({"modulation": m, "symbol_rate_bd": RS});
        let (rate, _, rot, c) = best_ber(&params, Tx::new(m, snr));
        let s = c.block(0).status();
        report.push(format!("{m} @ {snr} dB: BER {rate:.2e} (rotation {rot})"));
        assert!(rate <= limit, "{m} at Es/N0 {snr} dB: BER {rate} > {limit}");
        assert_eq!(s.lock, Lock::Locked, "{m}: not locked at the end");
        let extra: Vec<_> = s.extra.iter().collect();
        assert!(extra.contains(&("symbol_rate_bd", RS)), "{extra:?}");
        assert!(
            extra.contains(&("bits_per_symbol", bits_per_symbol(m) as f64)),
            "{extra:?}"
        );
        let off = extra.iter().find(|(k, _)| *k == "offset_hz").unwrap().1;
        assert!((off - 600.0).abs() < 30.0, "{m}: offset estimate {off} Hz");
    }
    eprintln!("{report:#?}");
}

/// SIGNAL-054 (802.15.4): O-QPSK with half-sine pulses, and O-QPSK with RRC pulses. OQPSK is the
/// native path (liquid has no OQPSK modem): offset rails, Gardner per rail, offset-aware Costas.
#[test]
fn oqpsk_recovers_blind_with_half_sine_and_rrc_pulses() {
    for (shape, pulse, snr) in [
        (Shape::HalfSine, "half-sine", 12.0),
        (Shape::Rrc(0.35), "rrc", 13.0),
    ] {
        let params = json!({"modulation": "oqpsk", "symbol_rate_bd": RS, "pulse": pulse});
        let tx = Tx {
            shape,
            ..Tx::new("oqpsk", snr)
        };
        let (rate, _, rot, c) = best_ber(&params, tx);
        assert!(
            rate <= 1e-3,
            "oqpsk {pulse} at {snr} dB: BER {rate} (rotation {rot})"
        );
        assert_eq!(c.block(0).status().lock, Lock::Locked, "oqpsk {pulse}");
    }
}

/// The soft contract (ADR-0011 §9.2): k items per symbol at k × the symbol rate, and the value
/// is a reliability — larger-magnitude items are wrong less often than small ones.
#[test]
fn soft_output_is_k_items_per_symbol_and_magnitude_is_reliability() {
    for (m, kb, snr) in [
        ("bpsk", 1usize, 4.0),
        ("qpsk", 2, 7.0),
        ("8psk", 3, 13.0),
        ("oqpsk", 2, 7.0),
    ] {
        let params = json!({"modulation": m, "symbol_rate_bd": RS});
        let tx = Tx {
            symbols: 6_000,
            ..Tx::new(m, snr)
        };
        let (truth, x) = transmit(tx);
        let c = run(&params, &x, 1_000);
        let out = c.out(0, 0);
        let syms = c.out(0, 1).iq.len();
        assert_eq!(out.real.len(), kb * syms, "{m}: items per symbol");
        assert_eq!(out.metas[0].rate_hz, kb as f64 * RS, "{m}: item rate");
        let sps_src = out.metas[0].source_per_item * kb as f64;
        assert!((sps_src - SPS).abs() < 1e-9, "{m}: source per symbol");
        // Reliability: split by |soft| into halves, the confident half has fewer errors (at the
        // best rotation — search it on hard bits first).
        let (_, lag, rot, c) = best_ber(&params, tx);
        let got = hard(&c);
        let soft = &c.out(0, 0).real;
        let mut mags: Vec<f32> = soft.iter().map(|v| v.abs()).collect();
        mags.sort_by(f32::total_cmp);
        let median = mags[mags.len() / 2];
        let (mut lo, mut hi) = ((0usize, 0usize), (0usize, 0usize));
        let hi_end = truth.len().min((soft.len() as isize - lag) as usize);
        for (i, &bit) in truth.iter().enumerate().take(hi_end).skip(2_500 * kb) {
            let j = (i as isize + lag) as usize;
            let wrong = usize::from(bit != got[j]);
            if soft[j].abs() < median {
                lo = (lo.0 + wrong, lo.1 + 1);
            } else {
                hi = (hi.0 + wrong, hi.1 + 1);
            }
        }
        assert!(
            lo.0 > 0,
            "{m} at {snr} dB should make some errors (rotation {rot})"
        );
        assert!(
            hi.0 * 4 < lo.0,
            "{m}: confident half {hi:?} vs unsure half {lo:?}"
        );
    }
}

/// ADR-0011 §1.6: identical output however the input is chunked, on every port — the liquid
/// path (resampler, acquisition window, tracker segments) and the native OQPSK path, each run
/// long enough for its acquisition to land, plus a signal that starts late, so a
/// re-acquisition happens mid-stream.
#[test]
fn output_is_chunking_invariant_on_both_paths() {
    for (m, extra, symbols, snr, late) in [
        ("qpsk", json!({}), 1_500, 15.0, false),
        ("d8psk", json!({}), 2_200, 22.0, false),
        ("oqpsk", json!({"pulse": "half-sine"}), 1_500, 15.0, false),
        ("oqpsk", json!({}), 1_500, 15.0, false),
        ("qpsk", json!({}), 1_500, 15.0, true),
    ] {
        let tx = Tx {
            symbols,
            ..Tx::new(m, snr)
        };
        let (_, mut x) = transmit(tx);
        if late {
            x = late_start(&x, 2_000);
        }
        let params = with(json!({"modulation": m, "symbol_rate_bd": RS}), extra);
        let c = assert_chunk_invariant(
            || vec![build("psk_demod", params.clone(), PortType::Iq)],
            PortType::Iq,
            FS,
            &PortVec::Iq(x.clone()),
            &[4_096, 1_000, 37, 1],
        );
        assert!(c.out(0, 0).real.len() > 1_000, "{m}");
        assert_eq!(c.block(0).status().lock, Lock::Locked, "{m} late {late}");
    }
}

/// `x` preceded by `symbols` of noise at the signal's own noise level (a burst that starts
/// after the stream did).
fn late_start(x: &[Complex32], symbols: usize) -> Vec<Complex32> {
    let tail = &x[x.len() - 2_000..];
    let floor = tail.iter().map(|z| f64::from(z.norm_sqr())).sum::<f64>() / tail.len() as f64;
    let mut rng = Lcg::new(99);
    let mut out: Vec<Complex32> = (0..(symbols as f64 * SPS) as usize)
        .map(|_| rng.cnoise(floor * 0.05))
        .collect();
    out.extend_from_slice(x);
    out
}

/// Re-acquisition: when the stream starts on noise, the first acquisition window holds no
/// carrier. The block stays `searching`, takes a fresh window once `REACQ_SYMBOLS` pass
/// without lock, and then decodes the burst — the same bits as when it starts on the signal.
#[test]
fn a_burst_that_starts_after_the_acquisition_window_is_reacquired() {
    let tx = Tx::new("qpsk", 13.0);
    let (truth, x) = transmit(tx);
    let x = late_start(&x, 3_000);
    let params = json!({"modulation": "qpsk", "symbol_rate_bd": RS});
    let mut best = (1.0f64, 0);
    let mut c = None;
    for rot in rotations("qpsk") {
        let p = with(params.clone(), json!({"rotation_deg": rot}));
        let run = run(&p, &x, 1_000);
        // Bits after the burst's first 3 000 symbols (acquisition plus lock) only.
        let got = hard(&run);
        let skip_items = 2 * (3_000 + 3_000);
        let tail: Vec<u8> = got.get(skip_items..).unwrap_or_default().to_vec();
        let (e, n, _) = ber(&truth[2 * 3_000..], &tail, 0);
        let rate = if n == 0 { 1.0 } else { e as f64 / n as f64 };
        if rate < best.0 {
            best = (rate, rot);
            c = Some(run);
        }
    }
    let c = c.expect("some rotation decodes");
    assert!(best.0 <= 1e-3, "BER {} after the late start", best.0);
    let s = c.block(0).status();
    assert_eq!(s.lock, Lock::Locked);
    let off = s.extra.iter().find(|(k, _)| *k == "offset_hz").unwrap().1;
    assert!((off - 600.0).abs() < 10.0, "re-acquired offset {off}");
}

/// The refinement loop and the MAUTO search read `status().lock`: `searching` on noise and
/// during acquisition, `locked` once the constellation is clean, and back to `searching` —
/// with every loop dropped — after a DISCONTINUITY.
#[test]
fn status_reads_searching_then_locked_and_resets_on_discontinuity() {
    let params = json!({"modulation": "qpsk", "symbol_rate_bd": RS});
    let info = PortInfo {
        ty: PortType::Iq,
        rate_hz: FS,
        max_items: 2_000,
        hold_items: 0,
    };
    // Noise only: never locked.
    let mut rng = Lcg::new(3);
    let noise: Vec<Complex32> = (0..40_000).map(|_| rng.cnoise(1.0)).collect();
    let mut c = Chain::new(vec![build("psk_demod", params.clone(), PortType::Iq)], info);
    for k in (0..noise.len()).step_by(2_000) {
        c.feed(
            slice(&PortVec::Iq(noise.clone()), k, k + 2_000),
            ChunkFlags::NONE,
        );
        assert_eq!(c.block(0).status().lock, Lock::Searching, "noise at {k}");
    }
    // Signal: searching first, locked later.
    let (_, x) = transmit(Tx::new("qpsk", 15.0));
    let sig = PortVec::Iq(x);
    let mut c = Chain::new(vec![build("psk_demod", params, PortType::Iq)], info);
    c.feed(slice(&sig, 0, 2_000), ChunkFlags::NONE);
    assert_eq!(c.block(0).status().lock, Lock::Searching, "at start");
    let mut locked_at = None;
    for k in (2_000..30_000).step_by(2_000) {
        c.feed(slice(&sig, k, k + 2_000), ChunkFlags::NONE);
        if locked_at.is_none() && c.block(0).status().lock == Lock::Locked {
            locked_at = Some(k);
        }
    }
    let locked_at = locked_at.expect("never locked");
    assert!(locked_at < 20_000, "locked only at sample {locked_at}");
    let q = c.block(0).status().quality.unwrap();
    assert!(q > 0.6, "quality {q}");
    // A gap: everything restarts, so the next chunk reads searching again…
    c.feed(slice(&sig, 30_000, 32_000), ChunkFlags::DISCONTINUITY);
    let s = c.block(0).status();
    assert_eq!(s.lock, Lock::Searching, "after DISCONTINUITY");
    assert!(
        c.out(0, 0)
            .metas
            .last()
            .unwrap()
            .flags
            .contains(ChunkFlags::DISCONTINUITY),
        "the flag goes out on the output"
    );
    // …and it re-acquires.
    for k in (32_000..48_000).step_by(2_000) {
        c.feed(slice(&sig, k, k + 2_000), ChunkFlags::NONE);
    }
    assert_eq!(c.block(0).status().lock, Lock::Locked, "re-acquired");
}

/// The coarse carrier stage is what acquires a large offset. With it off (`max_offset_hz: 0`)
/// liquid's loop alone does not pull in 8PSK at 0.125 × the symbol rate; with it on, it does.
/// This is the measured T-607 gap, closed.
#[test]
fn coarse_carrier_stage_acquires_what_the_tracker_alone_cannot() {
    let tx = Tx::new("8psk", 19.0);
    let on = json!({"modulation": "8psk", "symbol_rate_bd": RS});
    let off = with(on.clone(), json!({"max_offset_hz": 0}));
    let (with_stage, ..) = best_ber(&on, tx);
    let (without, ..) = best_ber(&off, tx);
    assert!(
        with_stage <= 2e-3,
        "with the coarse stage: BER {with_stage}"
    );
    assert!(
        without > 0.1,
        "without it: BER {without} (liquid pulled in unaided?)"
    );
}

/// Every item carries absolute capture time: the stamped source index of each output symbol
/// sits within a quarter symbol of the transmitted symbol's centre, on both paths.
#[test]
fn time_map_stamps_each_symbol_at_its_source_centre() {
    for m in ["qpsk", "oqpsk"] {
        let tx = Tx {
            symbols: 6_000,
            ..Tx::new(m, 15.0)
        };
        let params = json!({"modulation": m, "symbol_rate_bd": RS});
        let (_, lag, _, c) = best_ber(&params, tx);
        let kb = 2isize;
        // QPSK pairs bits within a symbol. OQPSK may lock a rail late (a 90° slip is a one-bit
        // slip), and then its "I" strobe truthfully sits on the transmitter's Q rail, half a
        // symbol on: bit b's rail is centred at (b/2 + tau) symbols either way.
        if m == "qpsk" {
            assert_eq!(lag % kb, 0, "{m}: bit lag {lag} is not symbol-aligned");
        }
        let out = c.out(0, 0);
        let mut worst = 0.0f64;
        let mut first = 0isize;
        for (meta, &len) in out.metas.iter().zip(&out.lens) {
            // First output item of this chunk is truth bit `b = first - lag`.
            let b = first - lag;
            if b >= 2_500 * kb {
                let sym = if m == "qpsk" {
                    (b / kb) as f64
                } else {
                    b as f64 / kb as f64
                };
                let want = (sym + tx.tau) * SPS;
                worst = worst.max((meta.source_index - want).abs());
            }
            first += len as isize;
        }
        assert!(
            worst < 0.25 * SPS,
            "{m}: time map off by {worst} samples ({SPS} per symbol)"
        );
    }
}

fn build_err(p: Value) -> BlockError {
    let maps = std::collections::BTreeMap::new();
    let ctx = BuildCtx {
        field_maps: &maps,
        input_types: &[PortType::Iq],
    };
    match Registry::builtin().build("psk_demod", &params(p), &ctx) {
        Err(e) => e,
        Ok(_) => panic!("should not build"),
    }
}

#[test]
fn parameters_are_checked_and_the_ambiguity_knobs_are_hot() {
    // Rotation must be a multiple of 360/M; differential modes have none to resolve.
    assert!(matches!(
        build_err(json!({"modulation": "qpsk", "symbol_rate_bd": RS, "rotation_deg": 45})),
        BlockError::Params(_)
    ));
    assert!(matches!(
        build_err(json!({"modulation": "d8psk", "symbol_rate_bd": RS, "rotation_deg": 45})),
        BlockError::Params(_)
    ));
    // liquid's tracker matches an RRC: other pulses are OQPSK-only.
    assert!(matches!(
        build_err(json!({"modulation": "bpsk", "symbol_rate_bd": RS, "pulse": "half-sine"})),
        BlockError::Params(_)
    ));
    // Hot: rotation, swap, loop bandwidth. Cold: anything else.
    let base = json!({"modulation": "qpsk", "symbol_rate_bd": RS});
    let mut b = build("psk_demod", base.clone(), PortType::Iq);
    b.init(&[PortInfo {
        ty: PortType::Iq,
        rate_hz: FS,
        max_items: 512,
        hold_items: 0,
    }])
    .unwrap();
    let hot = with(
        base.clone(),
        json!({"rotation_deg": 270, "iq_swap": true, "loop_bandwidth": 0.05}),
    );
    assert_eq!(
        update(b.as_mut(), hot, PortType::Iq),
        crate::ParamUpdate::Applied
    );
    assert_eq!(
        update(
            b.as_mut(),
            with(base.clone(), json!({"rolloff": 0.5})),
            PortType::Iq
        ),
        crate::ParamUpdate::Rebuild
    );
    let maps = std::collections::BTreeMap::new();
    let ctx = BuildCtx {
        field_maps: &maps,
        input_types: &[PortType::Iq],
    };
    assert!(
        b.update_params(&params(with(base, json!({"rotation_deg": 30}))), &ctx)
            .is_err()
    );
    // Fewer than two samples per symbol cannot be tracked.
    let mut b = build(
        "psk_demod",
        json!({"modulation": "bpsk", "symbol_rate_bd": 30000}),
        PortType::Iq,
    );
    assert!(
        b.init(&[PortInfo {
            ty: PortType::Iq,
            rate_hz: FS,
            max_items: 512,
            hold_items: 0,
        }])
        .is_err()
    );
}

// ------------------------------------------------------------------------------ burst (T-875)

/// A burst placed in a stream: (start sample, transmission, bits).
type Placed = (usize, Tx, Vec<u8>);

/// Places clean transmissions (`gain_db` over the first, after `gap` symbols of silence each)
/// in one stream, then adds noise at the first one's `es_n0_db`. Returns the samples and each
/// burst's (start sample, transmission, bits).
fn burst_stream(parts: &[(Tx, f64, usize)], tail: usize) -> (Vec<Complex32>, Vec<Placed>) {
    let mut x = Vec::new();
    let mut placed = Vec::new();
    let mut ref_power = None;
    for &(tx, gain_db, gap) in parts {
        x.extend(std::iter::repeat_n(
            Complex32::new(0.0, 0.0),
            (gap as f64 * SPS) as usize,
        ));
        let (bits, clean) = transmit(Tx {
            es_n0_db: 300.0,
            ..tx
        });
        let p = clean.iter().map(|z| f64::from(z.norm_sqr())).sum::<f64>() / clean.len() as f64;
        ref_power.get_or_insert(p);
        let g = 10f64.powf(gain_db / 20.0) as f32;
        placed.push((x.len(), tx, bits));
        x.extend(clean.iter().map(|z| z * g));
    }
    x.extend(std::iter::repeat_n(
        Complex32::new(0.0, 0.0),
        (tail as f64 * SPS) as usize,
    ));
    let noise = ref_power.unwrap_or(1.0) * SPS / 10f64.powf(parts[0].0.es_n0_db / 10.0);
    let mut rng = Lcg::new(parts[0].0.seed ^ 0x5eed);
    for v in &mut x {
        *v += rng.cnoise(noise);
    }
    (x, placed)
}

/// Bits of the burst transmitted at sample `start`, read from the output **by its time map**
/// (ADR-0011 §1.1: every item carries its capture time), not by searching for a lag: symbol
/// `n` is the item whose stamped centre is nearest `start + (n + tau)·SPS` (OQPSK: bit `b` at
/// `(b/2 + tau)·SPS`, each rail at its own strobe). Returns (bits missing or wrong, worst
/// time-map error in samples over the matched symbols).
fn errors_by_time(c: &Chain, start: usize, tx: Tx, truth: &[u8]) -> (usize, f64) {
    let kb = bits_per_symbol(tx.modulation);
    let oqpsk = tx.modulation == "oqpsk";
    let out = c.out(0, 0);
    let mut got: Vec<Option<u8>> = vec![None; truth.len()];
    let mut worst = 0.0f64;
    let mut at = 0;
    for (meta, &len) in out.metas.iter().zip(&out.lens) {
        for i in 0..len {
            let v = u8::from(out.real[at + i] > 0.0);
            let src = if oqpsk {
                meta.source_index + i as f64 * meta.source_per_item
            } else {
                meta.source_index + (i / kb * kb) as f64 * meta.source_per_item
            };
            let pos = (src - start as f64) / SPS - tx.tau;
            let (b, off) = if oqpsk {
                let b = (2.0 * pos).round();
                (b as isize, (pos - b / 2.0) * SPS)
            } else {
                let n = pos.round();
                (
                    (n as isize) * kb as isize + (i % kb) as isize,
                    (pos - n) * SPS,
                )
            };
            if b >= 0 && (b as usize) < truth.len() && got[b as usize].is_none() {
                got[b as usize] = Some(v);
                worst = worst.max(off.abs());
            }
        }
        at += len;
    }
    let errors = truth
        .iter()
        .zip(&got)
        .filter(|(t, g)| g.is_none_or(|g| g != **t))
        .count();
    (errors, worst)
}

/// Fewest bit errors over the phase ambiguity (as a consumer's sync word resolves it), for
/// the burst at `start`.
fn burst_errors_by_time(
    params: &Value,
    x: &[Complex32],
    chunk: usize,
    burst: &Placed,
) -> (usize, f64) {
    let (start, tx, truth) = burst;
    rotations(tx.modulation)
        .into_iter()
        .map(|rot| {
            let p = with(params.clone(), json!({"rotation_deg": rot}));
            errors_by_time(&run(&p, x, chunk), *start, *tx, truth)
        })
        .min_by_key(|e| e.0)
        .unwrap()
}

/// Bits a differential mode cannot know (the first symbol has no reference), and OQPSK's
/// half-symbol rail ambiguity (one bit).
fn allowance(m: &str) -> usize {
    match m {
        "dbpsk" | "dqpsk" | "pi4-dqpsk" | "d8psk" => bits_per_symbol(m),
        "oqpsk" => 1,
        _ => 0,
    }
}

/// T-875 (ADR-0015 §10 M-14 residue; SIGNAL-034/033/024/027/025/028/019/004/054/069, SPACE-081
/// families): a **16-symbol** burst of every constellation — 1/32 of the streaming path's
/// 512-symbol acquisition window — decodes whole from its first symbol in burst mode, blind,
/// through the same 600 Hz carrier offset, phase, timing offset and resampling as the
/// streaming test, at the same Es/N0. Bits are matched by the output's time map. RRC OQPSK is
/// the exception that needs 64 symbols (its x⁴ line is weak; measured in the module docs).
/// The streaming path, on the same 64-symbol QPSK burst, loses its start: the gap this closes.
#[test]
fn burst_mode_decodes_short_bursts_from_their_first_symbol() {
    let cases: [(&str, f64, Value, usize); 9] = [
        ("bpsk", 9.0, json!({}), 16),
        ("dbpsk", 11.0, json!({}), 16),
        ("qpsk", 13.0, json!({}), 16),
        ("dqpsk", 15.0, json!({}), 16),
        ("pi4-dqpsk", 15.0, json!({}), 16),
        ("8psk", 19.0, json!({}), 16),
        ("d8psk", 21.0, json!({}), 16),
        ("oqpsk", 12.0, json!({"pulse": "half-sine"}), 16),
        ("oqpsk", 13.0, json!({}), 64),
    ];
    for (m, snr, extra, n) in cases {
        let shape = if extra.get("pulse").is_some() {
            Shape::HalfSine
        } else {
            Shape::Rrc(0.35)
        };
        let tx = Tx {
            symbols: n,
            shape,
            ..Tx::new(m, snr)
        };
        let (x, placed) = burst_stream(&[(tx, 0.0, 300)], 300);
        let params = with(
            json!({"modulation": m, "symbol_rate_bd": RS, "burst": true}),
            extra,
        );
        let (e, worst) = burst_errors_by_time(&params, &x, 1_000, &placed[0]);
        assert!(
            e <= allowance(m),
            "{m} ({n} symbols at {snr} dB): {e} of {} bits missing or wrong",
            placed[0].2.len()
        );
        assert!(worst < 0.25 * SPS, "{m}: time map off by {worst} samples");
    }
    // The streaming path on a 64-symbol QPSK burst: its acquisition window is still filling.
    let tx = Tx {
        symbols: 64,
        ..Tx::new("qpsk", 13.0)
    };
    let (x, placed) = burst_stream(&[(tx, 0.0, 300)], 3_000);
    let stream = json!({"modulation": "qpsk", "symbol_rate_bd": RS});
    let (e, _) = burst_errors_by_time(&stream, &x, 1_000, &placed[0]);
    assert!(
        e > 32,
        "the streaming path decoded the burst's start ({e} errors)"
    );
}

/// ADR-0011 §9.2 burst boundaries: three bursts from three blind transmitters (each its own
/// carrier offset, phase, timing and level, one 6 dB above another) are each decoded, each
/// starts an output chunk flagged DISCONTINUITY, no item is emitted between them, and the
/// status counts them. The whole stream arrives in one input chunk, so the second and third
/// wait for later calls (one burst per output chunk).
#[test]
fn burst_mode_marks_each_burst_and_emits_nothing_between() {
    let base = Tx::new("qpsk", 15.0);
    let a = Tx {
        symbols: 40,
        ..base
    };
    let b = Tx {
        symbols: 200,
        cfo_hz: -400.0,
        phase: -2.0,
        tau: 0.8,
        seed: 21,
        ..base
    };
    let c = Tx {
        symbols: 24,
        cfo_hz: 150.0,
        phase: 0.3,
        tau: 0.1,
        seed: 33,
        ..base
    };
    let (x, placed) = burst_stream(&[(a, 0.0, 80), (b, 6.0, 30), (c, -3.0, 50)], 3_000);
    let params = json!({"modulation": "qpsk", "symbol_rate_bd": RS, "burst": true});
    let chunk = 10_000;
    for burst in &placed {
        let (e, worst) = burst_errors_by_time(&params, &x, chunk, burst);
        assert_eq!(
            e, 0,
            "burst at sample {}: {e} bits missing or wrong",
            burst.0
        );
        assert!(worst < 0.25 * SPS, "time map off by {worst} samples");
    }
    let run = run(&params, &x, chunk);
    let out = run.out(0, 0);
    let starts: Vec<f64> = out
        .metas
        .iter()
        .zip(&out.lens)
        .filter(|(m, len)| **len > 0 && m.flags.contains(ChunkFlags::DISCONTINUITY))
        .map(|(m, _)| m.source_index)
        .collect();
    assert_eq!(starts.len(), 3, "one DISCONTINUITY per burst: {starts:?}");
    // Every item lies inside a burst (plus the edge margin of a few symbols).
    let mut at = 0;
    for (meta, &len) in out.metas.iter().zip(&out.lens) {
        for i in 0..len {
            let t = meta.source_index + i as f64 * meta.source_per_item;
            let inside = placed.iter().any(|(s, tx, _)| {
                let lo = *s as f64 - 4.0 * SPS;
                let hi = *s as f64 + (tx.symbols as f64 + 4.0) * SPS;
                t >= lo && t <= hi
            });
            assert!(
                inside,
                "item {} at sample {t} is outside every burst",
                at + i
            );
        }
        at += len;
    }
    for ((s, _, _), got) in placed.iter().zip(&starts) {
        let first = *s as f64;
        assert!(
            *got >= first - 4.0 * SPS && *got <= first + 0.5 * SPS,
            "a burst at sample {s} starts its chunk at {got}"
        );
    }
    let st = run.block(0).status();
    let bursts = st.extra.iter().find(|(k, _)| *k == "bursts").unwrap().1;
    assert_eq!(bursts, 3.0);
    assert!(st.extra.iter().all(|(k, _)| k != "bursts_dropped"));
}

/// ADR-0011 §1.6: burst mode's items are identical however the input is chunked — the FIFO,
/// the detector's blocks and the deferral of a second burst to the next call never move an
/// item or change a value. (At 4 096 both bursts arrive in one call, so the second waits for
/// the next. A single chunk holding the whole stream and `END` cannot emit two bursts: the
/// second is counted in `bursts_dropped`, see the module docs.)
#[test]
fn burst_mode_output_is_chunking_invariant() {
    let base = Tx::new("qpsk", 15.0);
    let b = Tx {
        symbols: 120,
        cfo_hz: -300.0,
        seed: 5,
        ..base
    };
    let (x, _) = burst_stream(
        &[
            (
                Tx {
                    symbols: 30,
                    ..base
                },
                0.0,
                60,
            ),
            (b, 3.0, 20),
        ],
        400,
    );
    let params = json!({"modulation": "qpsk", "symbol_rate_bd": RS, "burst": true});
    let c = assert_chunk_invariant(
        || vec![build("psk_demod", params.clone(), PortType::Iq)],
        PortType::Iq,
        FS,
        &PortVec::Iq(x.clone()),
        &[4_096, 1_000, 37, 1],
    );
    assert!(c.out(0, 0).real.len() >= 2 * 150, "both bursts emitted");
}

/// Blind means no false bursts: noise alone, and a burst of noise 10 dB up (energy, but no
/// PSK line), emit nothing. (Below about a dozen symbols no blind test separates the two,
/// and a rise is taken on its energy — see the module docs.)
#[test]
fn burst_mode_emits_nothing_for_noise_or_a_burst_of_noise() {
    for m in ["qpsk", "8psk", "oqpsk"] {
        for seed in 0..3 {
            let mut rng = Lcg::new(700 + seed);
            let x: Vec<Complex32> = (0..14_000)
                .map(|i| {
                    rng.cnoise(if (5_000..6_500).contains(&i) {
                        10.0
                    } else {
                        1.0
                    })
                })
                .collect();
            let params = json!({"modulation": m, "symbol_rate_bd": RS, "burst": true});
            let c = run(&params, &x, 1_000);
            assert!(
                c.out(0, 0).real.is_empty(),
                "{m} seed {seed}: items from noise"
            );
            let s = c.block(0).status();
            assert_eq!(s.lock, Lock::Searching);
            let bursts = s.extra.iter().find(|(k, _)| *k == "bursts").unwrap().1;
            assert_eq!(bursts, 0.0, "{m} seed {seed}");
        }
    }
}

/// A capture cut to a burst (a region at a detection's own time extent) that ends with it: no
/// rise is ever seen, so the stream start's own candidate must take the burst, and `END`
/// closes it and flushes the resampler, so it decodes whole to its last symbol. (A 6-symbol
/// lead: the resampler's first output lands a filter span into the stream, about 4 symbols
/// here, on either path.)
#[test]
fn burst_mode_decodes_a_capture_that_starts_and_ends_on_the_burst() {
    let tx = Tx {
        symbols: 100,
        ..Tx::new("qpsk", 13.0)
    };
    let (x, placed) = burst_stream(&[(tx, 0.0, 6)], 0);
    let params = json!({"modulation": "qpsk", "symbol_rate_bd": RS, "burst": true});
    let (e, _) = burst_errors_by_time(&params, &x, 1_000, &placed[0]);
    assert!(e <= 2, "{e} bits missing or wrong");
}

/// The constant burst mode's timing seed rests on: after a (flushed) reset, liquid's tracker
/// strobes feed sample `(j − 9)·k` exactly, at every k.
#[test]
fn liquid_strobes_nine_symbols_behind_the_feed_after_a_reset() {
    use super::{Symtrack, TRACK_M};
    use hk_liquid_sys as lq;
    for k in [3u32, 5] {
        let kf = f64::from(k);
        let scheme = lq::scheme_id(lq::liquid_getopt_str2mod, "qpsk").unwrap();
        let mut rng = Lcg::new(11);
        let n_sym = 80usize;
        let pts: Vec<Complex64> = (0..n_sym)
            .map(|_| Complex64::from_polar(1.0, PI / 4.0 + TAU * (rng.next_u64() % 4) as f64 / 4.0))
            .collect();
        let mut best = (f64::INFINITY, 0.0, 0usize);
        for step in 0..20 {
            let tau = step as f64 / 20.0 - 0.5;
            let x: Vec<Complex32> = (0..((n_sym as f64 + 12.0) * kf) as usize)
                .map(|s| {
                    let t = s as f64 / kf - tau;
                    let v: Complex64 = pts
                        .iter()
                        .enumerate()
                        .filter(|(n, _)| (t - *n as f64).abs() < 9.0)
                        .map(|(n, p)| p * rrc(t - n as f64, 0.35))
                        .sum();
                    Complex32::new(v.re as f32, v.im as f32)
                })
                .collect();
            let mut t = Symtrack::new(k, 0.35, scheme, 0.2).unwrap();
            let mut zeros = vec![Complex32::new(0.0, 0.0); 4 * k as usize * TRACK_M as usize + 64];
            let mut y = vec![Complex32::new(0.0, 0.0); 2 * x.len().max(zeros.len())];
            t.reset(&mut zeros, &mut y);
            let ny = t.execute(&mut x.clone(), &mut y);
            for d in 7..12 {
                let js = d + 4..(d + 50).min(ny);
                let yy = |j: usize| Complex64::new(f64::from(y[j].re), f64::from(y[j].im));
                let g: Complex64 = js
                    .clone()
                    .map(|j| yy(j) * pts[j - d].conj())
                    .sum::<Complex64>()
                    / js.len() as f64;
                let err = js
                    .clone()
                    .map(|j| (yy(j) / g - pts[j - d]).norm_sqr())
                    .sum::<f64>()
                    / js.len() as f64;
                if err < best.0 {
                    best = (err, tau, d);
                }
            }
        }
        assert_eq!(best.2, 9, "k {k}: delay {best:?}");
        assert!(best.1.abs() < 0.03, "k {k}: strobe {best:?}");
    }
}

/// The measurement behind the module docs' before/after table: the shortest burst each mode
/// decodes whole (the streaming path with `HK_PSK_REPORT_STREAM=1`). Run with
/// `--run-ignored only --no-capture`.
#[test]
#[ignore = "T-875 measurement, not a guard (≈90 s)"]
fn burst_mode_shortest_burst_report() {
    let burst = std::env::var("HK_PSK_REPORT_STREAM").is_err();
    for (m, snr, extra) in [
        ("bpsk", 9.0, json!({})),
        ("dbpsk", 11.0, json!({})),
        ("qpsk", 13.0, json!({})),
        ("dqpsk", 15.0, json!({})),
        ("pi4-dqpsk", 15.0, json!({})),
        ("8psk", 19.0, json!({})),
        ("d8psk", 21.0, json!({})),
        ("oqpsk", 13.0, json!({})),
        ("oqpsk", 12.0, json!({"pulse": "half-sine"})),
    ] {
        let shape = if extra.get("pulse").is_some() {
            Shape::HalfSine
        } else {
            Shape::Rrc(0.35)
        };
        let mut line = format!("{m} {extra} @ {snr} dB:");
        for n in [8usize, 12, 16, 24, 32, 48, 64, 128, 256, 512, 1024, 2048] {
            let tx = Tx {
                symbols: n,
                shape,
                ..Tx::new(m, snr)
            };
            let (x, placed) = burst_stream(&[(tx, 0.0, 300)], if burst { 300 } else { 3_000 });
            let params = with(
                json!({"modulation": m, "symbol_rate_bd": RS, "burst": burst}),
                extra.clone(),
            );
            let (e, _) = burst_errors_by_time(&params, &x, 1_000, &placed[0]);
            line.push_str(&format!(" {n}:{e}/{}", placed[0].2.len()));
        }
        eprintln!("{line}");
    }
}

/// The measurement behind `MARGIN_RISE`: bursts of pure noise 10 dB up (energy, no PSK line)
/// and noise-only streams, 40 seeds each; how many burst mode takes for PSK.
#[test]
#[ignore = "T-875 measurement, not a guard (≈60 s)"]
fn burst_mode_false_accept_report() {
    for m in ["bpsk", "qpsk", "8psk", "oqpsk"] {
        let mut line = format!("{m}:");
        for len in [0usize, 12, 24, 64, 150, 400] {
            let mut accepted = 0;
            let trials = 40;
            for seed in 0..trials {
                let mut rng = Lcg::new(1000 + seed);
                let n = 8_000 + len * 10;
                let x: Vec<Complex32> = (0..n)
                    .map(|i| {
                        rng.cnoise(if i >= 4_000 && i < 4_000 + len * 10 {
                            10.0
                        } else {
                            1.0
                        })
                    })
                    .collect();
                let p = json!({"modulation": m, "symbol_rate_bd": RS, "burst": true});
                let c = run(&p, &x, 1_000);
                let s = c.block(0).status();
                let b = s
                    .extra
                    .iter()
                    .find(|(k, _)| *k == "bursts")
                    .map_or(0.0, |v| v.1);
                if b > 0.0 {
                    accepted += 1;
                }
            }
            line.push_str(&format!(" {len}: {accepted}/{trials};"));
        }
        eprintln!("{line}");
    }
}
