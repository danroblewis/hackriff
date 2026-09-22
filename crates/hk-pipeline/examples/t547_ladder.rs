//! T-547 measurement harness: the ADR-0015 §2.2 evidence-metric ladder under 8-bit
//! quantisation and gain state.
//!
//! Research tool, not product code. It reads a `cf32_le` IQ file (produced by `just synth
//! --datatype cf32_le`, so the reference really is float), cuts it into fixed windows, and for
//! each window evaluates the same metrics at a grid of **gain states**: the float reference,
//! plus `round(clamp(g·x·127))/127` at a list of gains `g`, which is exactly what
//! `hkpy.synth.scene.Scene.quantise` does to make a `ci8` fixture and what a HackRF's ADC does
//! to the analogue signal in front of it.
//!
//! Metrics come from the real M1 implementations wherever one exists:
//!   * `bimodality` -> `hk_classify::features` `if_local_bimodality` (S1, Sarle's coefficient of
//!     the instantaneous frequency about its local trend), on the mixed-and-filtered channel;
//!   * `eye_open`, `timing_var`, `snr` -> `hk_demod::fsk::FskDemod::demodulate`'s `FskLock`;
//!   * `evm`, `line_violations`, `bit_structure` -> computed here from that demodulator's soft
//!     symbols and bits, because no block publishes them yet; their definitions are in the
//!     write-up and are the harness's own.
//!
//! Output is one JSON object per (window, gain state) on stdout.
//!
//! **T-619** extends the same harness to two more paths, selected by `--path`:
//!   * `ook` — the AM/OOK ladder of ADR-0015 §1.1 ("AM/OOK: envelope bimodality"), driven through
//!     the real M1 blocks `am_demod` → `clock_recovery` → `slicer` (built through the block
//!     registry, so their parameters are schema-validated exactly as a recipe's are). Metrics:
//!     `env_bimodality` (Sarle's coefficient of the `am_demod` envelope; the harness's own, and
//!     the same formula `hk_classify::features` uses on the instantaneous frequency),
//!     `env_depth` and `eye_open`/`snr_db` from the blocks' own `Status`, `timing_var` from
//!     `clock_recovery`'s `timing_error` diagnostic port, `gamma_max` and `mu42_a` from the
//!     shipped C15 feature tree (the Azzouz–Nandi envelope pair), and `evm`/`line_viol`/`bit_struct` with the T-547 definitions.
//!   * `c4fm` — `hk_demod::fsk::c4fm::C4fmDemod` (the C23 trunking path). Metrics: the real
//!     block's `level_margin`, `outer_deviation_hz` and `residual_cfo_hz` (as `offset_ratio`),
//!     `if_local_bimodality`/`if_local_modality` from the shipped feature tree, and
//!     `dibit_balance`/`bit_struct` from the returned dibits.
//!
//! `--path fsk` (the default) is the T-547 harness unchanged; the only difference in its output
//! is one added `"path"` key per line.

use std::f64::consts::TAU;
use std::io::{BufWriter, Write};
use std::ops::Range;
use std::path::PathBuf;

use hk_blocks::block::{Io, PortInfo, TapMask};
use hk_blocks::buffer::{ChunkMeta, Input, Output, PortSlice};
use hk_blocks::registry::{BuildCtx, Registry};
use hk_classify::features::{FeatureInput, features};
use hk_core::{Discontinuity, ProvenanceHandle};
use hk_demod::dsp::lowpass_taps;
use hk_demod::fsk::c4fm::{C4fmConfig, C4fmDemod, C4fmError};
use hk_demod::fsk::{FskDemod, FskDemodConfig, FskDemodRequest};
use hk_dsp::ChannelTime;
use hk_estimate::snippet::{ChannelSnippet, SnippetFlags};
use hk_model::{Provenance, SampleTime, Timestamp};
use hk_recipe::{Params, PortType};
use num_complex::Complex32;

struct Args {
    iq: PathBuf,
    fs: f64,
    rate_bd: f64,
    dev_hz: f64,
    cfo_hz: f64,
    win: usize,
    stride: usize,
    max_windows: usize,
    skip: usize,
    gains_db: Vec<f64>,
    label: String,
    threads: usize,
    no_quant: bool,
    box_lo: usize,
    box_hi: usize,
    path: PathKind,
    ch_bw_hz: f64,
}

/// Which evidence ladder to measure (T-619).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathKind {
    /// T-547: the M1 2-FSK demodulator plus `if_local_bimodality`.
    Fsk,
    /// The AM/OOK envelope ladder: `am_demod` → `clock_recovery` → `slicer`.
    Ook,
    /// The C4FM (4-level FSK) ladder: `hk_demod::fsk::c4fm`.
    C4fm,
}

impl PathKind {
    fn as_str(self) -> &'static str {
        match self {
            PathKind::Fsk => "fsk",
            PathKind::Ook => "ook",
            PathKind::C4fm => "c4fm",
        }
    }
}

fn arg(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn parse() -> Args {
    let a: Vec<String> = std::env::args().collect();
    let num = |k: &str, d: f64| arg(&a, k).map_or(d, |v| v.parse().unwrap());
    Args {
        iq: PathBuf::from(arg(&a, "--iq").expect("--iq <cf32 file>")),
        fs: num("--fs", 500_000.0),
        rate_bd: num("--rate", 4800.0),
        dev_hz: num("--dev", 9600.0),
        cfo_hz: num("--cfo", 53_000.0),
        win: num("--win", 4096.0) as usize,
        stride: num("--stride", 4096.0) as usize,
        max_windows: num("--windows", 4096.0) as usize,
        skip: num("--skip", 0.0) as usize,
        gains_db: arg(&a, "--gains-db").map_or_else(
            || {
                vec![
                    -11.1, -5.1, 0.9, 6.9, 13.0, 19.0, 25.0, 31.1, 34.6, 37.1, 40.0,
                ]
            },
            |s| s.split(',').map(|t| t.trim().parse().unwrap()).collect(),
        ),
        label: arg(&a, "--label").unwrap_or_else(|| "run".into()),
        threads: num("--threads", 8.0) as usize,
        no_quant: std::env::args().any(|x| x == "--no-quant"),
        box_lo: num("--box-lo", 0.0) as usize,
        box_hi: num("--box-hi", 0.0) as usize,
        path: match arg(&a, "--path").as_deref() {
            Some("ook") => PathKind::Ook,
            Some("c4fm") => PathKind::C4fm,
            None | Some("fsk") => PathKind::Fsk,
            Some(other) => panic!("--path {other}: expected fsk|ook|c4fm"),
        },
        // Channel bandwidth the AM/OOK ladder filters to before the envelope detector.
        ch_bw_hz: num("--ch-bw", 12_000.0),
    }
}

fn read_cf32(p: &PathBuf) -> Vec<Complex32> {
    let bytes = std::fs::read(p).expect("read iq");
    bytes
        .chunks_exact(8)
        .map(|c| {
            Complex32::new(
                f32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                f32::from_le_bytes([c[4], c[5], c[6], c[7]]),
            )
        })
        .collect()
}

fn provenance(fs: f64) -> ProvenanceHandle {
    let p: Provenance = serde_json::from_value(serde_json::json!({
        "device_id": "synthetic:t547",
        "tune": { "center_hz": 433.92e6, "sample_rate_hz": fs, "lna_db": 24.0, "vga_db": 20.0,
                  "amp_on": false, "bandwidth_hz": fs * 0.75 },
        "overload": false, "quantisation_limited": false, "clock_source": "internal",
        "clock_locked": true, "timestamp_method": "synthetic", "timestamp_error_budget_ns": 0,
    }))
    .unwrap();
    let _ = Discontinuity::STREAM_START;
    ProvenanceHandle::new(p)
}

fn snippet(
    samples: Vec<Complex32>,
    fs: f64,
    prov: &ProvenanceHandle,
    bx: Range<usize>,
) -> ChannelSnippet {
    let n = samples.len();
    ChannelSnippet {
        samples,
        sample_rate_hz: fs,
        passband_hz: fs * 0.45,
        source_rate_hz: fs,
        tuned_center_hz: 433.92e6,
        center_offset_hz: 0.0,
        box_bandwidth_hz: fs * 0.5,
        box_range: bx,
        time: ChannelTime {
            out_index: 0,
            source_index: 0.0,
            source_per_output: 1.0,
            time: SampleTime {
                sample_index: 0,
                host_time: Timestamp::from_unix_nanos(1_789_000_000_000_000_000),
            },
        },
        provenance: prov.clone(),
        clip_count: 0,
        clip_checked: n as u64,
        flags: SnippetFlags::default(),
    }
}

/// `round(clamp(g·x·127, -128, 127))/127`, plus the clipped-sample count (either component).
fn quantise(x: &[Complex32], g: f32) -> (Vec<Complex32>, u64) {
    let mut clipped = 0u64;
    let q = |v: f32, clip: &mut bool| -> f32 {
        let s = v * g * 127.0;
        if !(-128.5..=127.5).contains(&s) {
            *clip = true;
        }
        s.round().clamp(-128.0, 127.0) / 127.0
    };
    let out = x
        .iter()
        .map(|s| {
            let mut c = false;
            let v = Complex32::new(q(s.re, &mut c), q(s.im, &mut c));
            if c {
                clipped += 1;
            }
            v
        })
        .collect();
    (out, clipped)
}

/// Mix by `-cfo` and low-pass to the FSK channel (the demodulator's own S0/S1 front end).
fn channelise(x: &[Complex32], fs: f64, cfo: f64, dev: f64, rate: f64) -> Vec<Complex32> {
    let w = -TAU * cfo / fs;
    let mixed: Vec<Complex32> = x
        .iter()
        .enumerate()
        .map(|(k, s)| {
            let ph = (w * k as f64) % TAU;
            s * Complex32::new(ph.cos() as f32, ph.sin() as f32)
        })
        .collect();
    let taps = match lowpass_taps(fs, dev + 0.5 * rate, dev + 1.1 * rate, 40.0) {
        Ok(t) => t,
        Err(_) => return mixed,
    };
    let d = taps.len() / 2;
    let n = mixed.len();
    (0..n)
        .map(|i| {
            let mut acc = Complex32::new(0.0, 0.0);
            for (k, &t) in taps.iter().enumerate() {
                let j = i + k;
                if j >= d && j - d < n {
                    acc += mixed[j - d] * t;
                }
            }
            acc
        })
        .collect()
}

/// Manchester violation rate: the share of disjoint bit pairs that are `00` or `11`. Under iid
/// fair bits this is Binomial(floor(N/2), 1/2); the point of measuring it is that slicer output
/// on noise is not iid.
fn line_violations(bits: &[u8]) -> Option<f64> {
    let m = bits.len() / 2;
    if m < 8 {
        return None;
    }
    let bad = (0..m).filter(|&i| bits[2 * i] == bits[2 * i + 1]).count();
    Some(bad as f64 / m as f64)
}

/// Wald-Wolfowitz runs-test |z|. Small |z| = "structured like a real bit stream"; large |z| =
/// constant (few runs) or all-toggle (many runs). ADR-0015 S3 "bit-structure sanity".
fn bit_structure(bits: &[u8]) -> Option<f64> {
    let n = bits.len();
    if n < 20 {
        return None;
    }
    let n1 = bits.iter().filter(|&&b| b == 1).count() as f64;
    let n0 = n as f64 - n1;
    if n1 < 1.0 || n0 < 1.0 {
        return Some(f64::INFINITY);
    }
    let runs = 1 + bits.windows(2).filter(|w| w[0] != w[1]).count();
    let mu = 2.0 * n0 * n1 / n as f64 + 1.0;
    let var = (mu - 1.0) * (mu - 2.0) / (n as f64 - 1.0);
    if var <= 0.0 {
        return None;
    }
    Some((runs as f64 - mu).abs() / var.sqrt())
}

/// EVM over the soft symbols, as a ratio: rms(|s| - median|s|) / median|s|. `soft` is an affine
/// map of the demodulator's symbol values, so this is scale-free.
fn evm(soft: &[f32]) -> Option<f64> {
    if soft.len() < 16 {
        return None;
    }
    let mut a: Vec<f64> = soft.iter().map(|s| f64::from(*s).abs()).collect();
    a.sort_by(f64::total_cmp);
    let med = a[a.len() / 2];
    if med <= 0.0 || !med.is_finite() {
        return None;
    }
    let v = a.iter().map(|x| (x - med).powi(2)).sum::<f64>() / a.len() as f64;
    Some(v.sqrt() / med)
}

fn jnum(v: Option<f64>) -> String {
    match v {
        Some(x) if x.is_finite() => format!("{x:.6}"),
        _ => "null".into(),
    }
}

struct Row {
    window: usize,
    gain_db: f64,
    sigma_lsb: f64,
    clip_frac: f64,
    ok: bool,
    err: &'static str,
    /// Support: symbols (or dibits) the stage actually produced. `Evidence.n` in ADR-0015 terms.
    n_symbols: usize,
    /// The path's metrics, in a fixed order per path.
    metrics: Vec<(&'static str, Option<f64>)>,
}

/// Everything a path needs to evaluate one window, built once and shared across threads.
struct Tools {
    demod: FskDemod,
    c4fm: C4fmDemod,
    registry: Registry,
    prov: ProvenanceHandle,
}

/// Applies the gain state (and the ADC, unless `--no-quant`) and dispatches to the path.
fn measure(
    win: &[Complex32],
    args: &Args,
    tools: &Tools,
    window: usize,
    gain_db: Option<f64>,
) -> Row {
    let (x, clipped) = match gain_db {
        None => (win.to_vec(), 0),
        // `--no-quant` scales by the same gain but skips the ADC, which is the control that
        // separates "gain moved the metric" from "the ADC moved the metric".
        Some(g) if args.no_quant => {
            let k = 10f32.powf(g as f32 / 20.0);
            (win.iter().map(|s| s * k).collect(), 0)
        }
        Some(g) => quantise(win, 10f32.powf(g as f32 / 20.0)),
    };
    // Per-component rms of the *quantised* stream, in LSB: the ADC fill level.
    let p = x.iter().map(|s| f64::from(s.norm_sqr())).sum::<f64>() / x.len() as f64;
    let sigma_lsb = (p / 2.0).sqrt() * 127.0;

    let mut row = Row {
        window,
        gain_db: gain_db.unwrap_or(f64::NAN),
        sigma_lsb,
        clip_frac: clipped as f64 / x.len() as f64,
        ok: false,
        err: "",
        n_symbols: 0,
        metrics: Vec::new(),
    };
    match args.path {
        PathKind::Fsk => measure_fsk(x, args, tools, &mut row),
        PathKind::Ook => measure_ook(&x, args, tools, &mut row),
        PathKind::C4fm => measure_c4fm(&x, args, tools, &mut row),
    }
    row
}

/// T-547's path, unchanged: `if_local_bimodality` plus the M1 2-FSK demodulator's lock.
fn measure_fsk(x: Vec<Complex32>, args: &Args, tools: &Tools, row: &mut Row) {
    let ch = channelise(&x, args.fs, args.cfo_hz, args.dev_hz, args.rate_bd);
    let f = features(&FeatureInput {
        samples: &ch,
        sample_rate_hz: args.fs,
        obw_hz: Some(2.0 * args.dev_hz + args.rate_bd),
        snr_db: None,
        symbols: None,
    });
    let bimodality = f.get("if_local_bimodality");

    let n = x.len();
    let bx = if args.box_hi > args.box_lo && args.box_hi <= n {
        Range {
            start: args.box_lo,
            end: args.box_hi,
        }
    } else {
        Range { start: 0, end: n }
    };
    let snip = snippet(x, args.fs, &tools.prov, bx.clone());
    let req = FskDemodRequest {
        rate_bd: args.rate_bd,
        deviation_hz: Some(args.dev_hz),
        cfo_hz: args.cfo_hz,
        range: bx,
        bandwidth_hz: Some(2.0 * args.dev_hz + args.rate_bd),
    };
    let (mut eye_open, mut timing_var, mut snr_db, mut ev, mut lv, mut bs) =
        (None, None, None, None, None, None);
    match tools.demod.demodulate(&snip, &req) {
        Ok(sym) => {
            row.ok = true;
            eye_open = Some(sym.lock.eye_opening);
            timing_var = Some(sym.lock.timing_rms_ui);
            snr_db = sym.symbol_snr_db;
            ev = evm(&sym.soft);
            lv = line_violations(&sym.bits);
            bs = bit_structure(&sym.bits);
            row.n_symbols = sym.bits.len();
        }
        Err(e) => {
            row.err = match e {
                hk_demod::fsk::FskDemodError::TooShort(_) => "too_short",
                hk_demod::fsk::FskDemodError::TooFewSamplesPerSymbol(_) => "too_few_sps",
                hk_demod::fsk::FskDemodError::InvalidRequest(_) => "invalid",
                _ => "other",
            };
        }
    }
    row.metrics = vec![
        ("bimodality", bimodality),
        ("eye_open", eye_open),
        ("timing_var", timing_var),
        ("snr_db", snr_db),
        ("evm", ev),
        ("line_viol", lv),
        ("bit_struct", bs),
    ];
}

/// The AM/OOK ladder (T-619): `am_demod` -> `clock_recovery` -> `slicer`, all through the block
/// registry, plus the shipped feature tree's `blind_ook`.
fn measure_ook(x: &[Complex32], args: &Args, tools: &Tools, row: &mut Row) {
    let half_bw = 0.5 * args.ch_bw_hz;
    let ch = channelise(x, args.fs, args.cfo_hz, half_bw, args.rate_bd);
    let f = features(&FeatureInput {
        samples: &ch,
        sample_rate_hz: args.fs,
        obw_hz: Some(args.ch_bw_hz),
        snr_db: None,
        symbols: None,
    });
    let (mut env_bimodality, mut env_depth) = (None, None);
    let (mut eye_open, mut timing_var, mut snr_db, mut ev, mut lv, mut bs) =
        (None, None, None, None, None, None);
    if let Some(out) = ook_chain(&ch, args, &tools.registry) {
        row.ok = true;
        row.n_symbols = out.bits.len();
        env_bimodality = Some(sarle_bimodality(&out.envelope));
        env_depth = out.depth;
        eye_open = out.eye;
        snr_db = out.snr_db;
        timing_var = out.timing_rms;
        ev = evm(&out.soft);
        lv = line_violations(&out.bits);
        bs = bit_structure(&out.bits);
    } else {
        row.err = "chain";
    }
    row.metrics = vec![
        ("env_bimodality", env_bimodality),
        ("env_depth", env_depth),
        ("gamma_max", f.get("gamma_max")),
        ("mu42_a", f.get("mu42_a")),
        ("eye_open", eye_open),
        ("timing_var", timing_var),
        ("snr_db", snr_db),
        ("evm", ev),
        ("line_viol", lv),
        ("bit_struct", bs),
    ];
}

/// What one run of the AM/OOK block chain produced.
struct OokOut {
    envelope: Vec<f64>,
    depth: Option<f64>,
    soft: Vec<f32>,
    bits: Vec<u8>,
    eye: Option<f64>,
    snr_db: Option<f64>,
    timing_rms: Option<f64>,
}

/// Runs `am_demod` -> `clock_recovery` -> `slicer` over one window, in one chunk.
///
/// Every block is built through [`Registry::build`], so its parameters go through the same schema
/// validation a recipe's do, and every number below is the block's own `Status` or output.
fn ook_chain(x: &[Complex32], args: &Args, reg: &Registry) -> Option<OokOut> {
    let n = x.len();
    let maps = std::collections::BTreeMap::new();
    let iq_ctx = BuildCtx {
        field_maps: &maps,
        input_types: &[PortType::Iq],
    };
    let real_ctx = BuildCtx {
        field_maps: &maps,
        input_types: &[PortType::Real],
    };
    let soft_ctx = BuildCtx {
        field_maps: &maps,
        input_types: &[PortType::Soft],
    };

    // S1: envelope, normalised by the carrier level over ~48 symbols.
    let tau_s = (48.0 / args.rate_bd).clamp(1e-4, 10.0);
    let mut am = reg
        .build(
            "am_demod",
            &params(serde_json::json!({ "mode": "normalized", "time_constant_s": tau_s })),
            &iq_ctx,
        )
        .ok()?;
    let am_in = PortInfo {
        ty: PortType::Iq,
        rate_hz: args.fs,
        max_items: n,
        hold_items: 0,
    };
    let am_out_info = am.init(&[am_in]).ok()?;
    let mut am_out = Output::for_port(&am_out_info[0]);
    am_out.begin_chunk();
    {
        let inputs = [Input {
            meta: ChunkMeta::start(args.fs),
            data: PortSlice::Iq(x),
        }];
        let mut io = Io::new(&inputs, std::slice::from_mut(&mut am_out));
        am.process(&mut io).ok()?;
    }
    let env: Vec<f64> = match am_out.data.as_slice() {
        PortSlice::Real(v) => v.iter().map(|s| f64::from(*s)).collect(),
        _ => return None,
    };
    let depth = extra(&am.status(), "depth");

    // S2: symbol timing on the envelope, with the timing-error diagnostic tapped.
    let mut clk = reg
        .build(
            "clock_recovery",
            &params(serde_json::json!({
                "symbol_rate_bd": args.rate_bd, "pulse": "nrz",
                "algorithm": "gardner", "loop_bandwidth": 0.01,
            })),
            &real_ctx,
        )
        .ok()?;
    let clk_out_info = clk.init(&[am_out_info[0]]).ok()?;
    let mut clk_outs: Vec<Output> = clk_out_info.iter().map(Output::for_port).collect();
    for o in &mut clk_outs {
        o.begin_chunk();
    }
    {
        let inputs = [Input {
            meta: am_out.meta,
            data: am_out.data.as_slice(),
        }];
        let mut io = Io::new(&inputs, &mut clk_outs).with_taps(TapMask(0b11));
        clk.process(&mut io).ok()?;
    }
    let soft: Vec<f32> = match clk_outs[0].data.as_slice() {
        PortSlice::Soft(v) => v.to_vec(),
        _ => return None,
    };
    if soft.len() < 16 {
        return None;
    }
    let clk_status = clk.status();
    let timing_rms = match clk_outs.get(1).map(|o| o.data.as_slice()) {
        Some(PortSlice::Real(te)) if !te.is_empty() => Some(
            (te.iter()
                .map(|v| f64::from(*v) * f64::from(*v))
                .sum::<f64>()
                / te.len() as f64)
                .sqrt(),
        ),
        _ => None,
    };

    // S3: the slicer's bits, at the block's default threshold.
    let mut sl = reg
        .build(
            "slicer",
            &params(serde_json::json!({ "threshold": 0.0 })),
            &soft_ctx,
        )
        .ok()?;
    let sl_out_info = sl.init(&[clk_out_info[0]]).ok()?;
    let mut sl_out = Output::for_port(&sl_out_info[0]);
    sl_out.begin_chunk();
    {
        let inputs = [Input {
            meta: clk_outs[0].meta,
            data: clk_outs[0].data.as_slice(),
        }];
        let mut io = Io::new(&inputs, std::slice::from_mut(&mut sl_out));
        sl.process(&mut io).ok()?;
    }
    let bits: Vec<u8> = match sl_out.data.as_slice() {
        PortSlice::Bits(v) => v.to_vec(),
        _ => return None,
    };

    Some(OokOut {
        envelope: env,
        depth,
        soft,
        bits,
        eye: clk_status.quality.map(f64::from),
        snr_db: clk_status.snr_db.map(f64::from),
        timing_rms,
    })
}

/// The C4FM ladder (T-619): `hk_demod::fsk::c4fm` plus the shipped IF-shape features.
fn measure_c4fm(x: &[Complex32], args: &Args, tools: &Tools, row: &mut Row) {
    let ch = channelise(x, args.fs, args.cfo_hz, args.dev_hz, args.rate_bd);
    let f = features(&FeatureInput {
        samples: &ch,
        sample_rate_hz: args.fs,
        obw_hz: Some(2.0 * args.dev_hz + args.rate_bd),
        snr_db: None,
        symbols: None,
    });
    let (mut level_margin, mut offset_ratio, mut outer_dev, mut balance, mut bs) =
        (None, None, None, None, None);
    match tools.c4fm.demodulate(x, args.fs, args.cfo_hz) {
        Ok(sym) => {
            row.ok = true;
            row.n_symbols = sym.dibits.len();
            level_margin = Some(sym.level_margin);
            outer_dev = Some(sym.outer_deviation_hz);
            offset_ratio = Some(sym.residual_cfo_hz.abs() / sym.outer_deviation_hz.max(1e-9));
            balance = dibit_balance(&sym.dibits);
            // The dibit stream's structure, over the same runs test the 2-FSK path uses, applied
            // to the dibits expanded MSB-first into bits.
            let bits: Vec<u8> = sym.dibits.iter().flat_map(|d| [d >> 1, d & 1]).collect();
            bs = bit_structure(&bits);
        }
        Err(e) => {
            row.err = match e {
                C4fmError::TooFewSamplesPerSymbol(_) => "too_few_sps",
                C4fmError::TooFewSymbols(_) => "too_few_symbols",
                C4fmError::InvalidRequest(_) => "invalid",
                C4fmError::Design(_) => "design",
            };
        }
    }
    row.metrics = vec![
        ("if_local_bimodality", f.get("if_local_bimodality")),
        ("if_local_modality", f.get("if_local_modality")),
        ("level_margin", level_margin),
        ("offset_ratio", offset_ratio),
        ("outer_dev_hz", outer_dev),
        ("dibit_balance", balance),
        ("bit_struct", bs),
    ];
}

/// Params from a JSON object literal (the `hk_blocks` test kit's helper, which is crate-private).
fn params(v: serde_json::Value) -> Params {
    v.as_object().cloned().unwrap_or_default()
}

/// A block `Status` extra by name.
fn extra(s: &hk_blocks::Status, key: &str) -> Option<f64> {
    s.extra.iter().find(|(k, _)| *k == key).map(|(_, v)| v)
}

/// Sarle's bimodality coefficient `(skew² + 1) / kurtosis`, the same formula
/// `hk_classify::features` applies to the instantaneous frequency, here on the envelope.
fn sarle_bimodality(v: &[f64]) -> f64 {
    let n = v.len() as f64;
    if n < 4.0 {
        return 0.0;
    }
    let mean = v.iter().sum::<f64>() / n;
    let m2 = v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
    if m2 <= 0.0 {
        return 0.0;
    }
    let m3 = v.iter().map(|x| (x - mean).powi(3)).sum::<f64>() / n;
    let m4 = v.iter().map(|x| (x - mean).powi(4)).sum::<f64>() / n;
    let skew = m3 / m2.powf(1.5);
    let kurt = m4 / (m2 * m2);
    if kurt <= 0.0 {
        return 0.0;
    }
    (skew * skew + 1.0) / kurt
}

/// Chi-square |z| of the dibit histogram against uniform: 0 = perfectly balanced.
///
/// The C4FM analogue of `line_violations`. Real traffic and frame sync are balanced across the
/// four levels; a slicer centred on noise is not, because the level centre and the outer estimate
/// are both derived from the same noisy sample.
fn dibit_balance(dibits: &[u8]) -> Option<f64> {
    if dibits.len() < 20 {
        return None;
    }
    let n = dibits.len() as f64;
    let mut counts = [0.0f64; 4];
    for &d in dibits {
        counts[(d & 3) as usize] += 1.0;
    }
    let exp = n / 4.0;
    let chi2: f64 = counts.iter().map(|c| (c - exp).powi(2) / exp).sum();
    // Wilson–Hilferty: chi²(3) to a standard normal, so the number is comparable to
    // `bit_structure`'s |z|.
    let k = 3.0;
    Some((((chi2 / k).cbrt() - (1.0 - 2.0 / (9.0 * k))) / (2.0 / (9.0 * k)).sqrt()).abs())
}

fn main() {
    let args = parse();
    let iq = read_cf32(&args.iq);
    let tools = Tools {
        demod: FskDemod::new(FskDemodConfig::default()),
        c4fm: C4fmDemod::new(C4fmConfig {
            symbol_rate_bd: args.rate_bd,
            ..C4fmConfig::default()
        }),
        registry: Registry::builtin(),
        prov: provenance(args.fs),
    };

    let starts: Vec<usize> = (0..args.max_windows)
        .map(|i| args.skip + i * args.stride)
        .take_while(|s| s + args.win <= iq.len())
        .collect();

    let out = std::io::stdout();
    let mut w = BufWriter::new(out.lock());
    let chunk = starts.len().div_ceil(args.threads.max(1));
    let rows: Vec<Row> = std::thread::scope(|s| {
        let handles: Vec<_> = starts
            .chunks(chunk.max(1))
            .enumerate()
            .map(|(ci, c)| {
                let (tools, args) = (&tools, &args);
                let iq = &iq;
                s.spawn(move || {
                    let mut v = Vec::new();
                    for (k, &st) in c.iter().enumerate() {
                        let idx = ci * chunk + k;
                        let win = &iq[st..st + args.win];
                        v.push(measure(win, args, tools, idx, None));
                        for &g in &args.gains_db {
                            v.push(measure(win, args, tools, idx, Some(g)));
                        }
                    }
                    v
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });

    for r in &rows {
        write!(
            w,
            r#"{{"label":"{}","path":"{}","window":{},"gain_db":{},"sigma_lsb":{:.4},"clip_frac":{:.6},"ok":{},"err":"{}","n_symbols":{}"#,
            args.label,
            args.path.as_str(),
            r.window,
            if r.gain_db.is_nan() { "null".to_string() } else { format!("{:.2}", r.gain_db) },
            r.sigma_lsb,
            r.clip_frac,
            r.ok,
            r.err,
            r.n_symbols,
        )
        .unwrap();
        for (name, value) in &r.metrics {
            write!(w, r#","{}":{}"#, name, jnum(*value)).unwrap();
        }
        writeln!(w, "}}").unwrap();
    }
    w.flush().unwrap();
    eprintln!("windows={} rows={}", starts.len(), rows.len());
}
