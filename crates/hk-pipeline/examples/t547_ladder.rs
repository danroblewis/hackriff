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

use std::f64::consts::TAU;
use std::io::{BufWriter, Write};
use std::ops::Range;
use std::path::PathBuf;

use hk_classify::features::{FeatureInput, features};
use hk_core::{Discontinuity, ProvenanceHandle};
use hk_demod::dsp::lowpass_taps;
use hk_demod::fsk::{FskDemod, FskDemodConfig, FskDemodRequest};
use hk_dsp::ChannelTime;
use hk_estimate::snippet::{ChannelSnippet, SnippetFlags};
use hk_model::{Provenance, SampleTime, Timestamp};
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
    bimodality: Option<f64>,
    eye_open: Option<f64>,
    timing_var: Option<f64>,
    snr_db: Option<f64>,
    evm: Option<f64>,
    line_viol: Option<f64>,
    bit_struct: Option<f64>,
    n_symbols: usize,
}

fn measure(
    win: &[Complex32],
    args: &Args,
    demod: &FskDemod,
    prov: &ProvenanceHandle,
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
    let snip = snippet(x, args.fs, prov, bx.clone());
    let req = FskDemodRequest {
        rate_bd: args.rate_bd,
        deviation_hz: Some(args.dev_hz),
        cfo_hz: args.cfo_hz,
        range: bx,
        bandwidth_hz: Some(2.0 * args.dev_hz + args.rate_bd),
    };
    let mut row = Row {
        window,
        gain_db: gain_db.unwrap_or(f64::NAN),
        sigma_lsb,
        clip_frac: clipped as f64 / n as f64,
        ok: false,
        err: "",
        bimodality,
        eye_open: None,
        timing_var: None,
        snr_db: None,
        evm: None,
        line_viol: None,
        bit_struct: None,
        n_symbols: 0,
    };
    match demod.demodulate(&snip, &req) {
        Ok(sym) => {
            row.ok = true;
            row.eye_open = Some(sym.lock.eye_opening);
            row.timing_var = Some(sym.lock.timing_rms_ui);
            row.snr_db = sym.symbol_snr_db;
            row.evm = evm(&sym.soft);
            row.line_viol = line_violations(&sym.bits);
            row.bit_struct = bit_structure(&sym.bits);
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
    row
}

fn main() {
    let args = parse();
    let iq = read_cf32(&args.iq);
    let demod = FskDemod::new(FskDemodConfig::default());
    let prov = provenance(args.fs);

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
                let (demod, prov, args) = (&demod, &prov, &args);
                let iq = &iq;
                s.spawn(move || {
                    let mut v = Vec::new();
                    for (k, &st) in c.iter().enumerate() {
                        let idx = ci * chunk + k;
                        let win = &iq[st..st + args.win];
                        v.push(measure(win, args, demod, prov, idx, None));
                        for &g in &args.gains_db {
                            v.push(measure(win, args, demod, prov, idx, Some(g)));
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
        writeln!(
            w,
            r#"{{"label":"{}","window":{},"gain_db":{},"sigma_lsb":{:.4},"clip_frac":{:.6},"ok":{},"err":"{}","n_symbols":{},"bimodality":{},"eye_open":{},"timing_var":{},"snr_db":{},"evm":{},"line_viol":{},"bit_struct":{}}}"#,
            args.label,
            r.window,
            if r.gain_db.is_nan() { "null".to_string() } else { format!("{:.2}", r.gain_db) },
            r.sigma_lsb,
            r.clip_frac,
            r.ok,
            r.err,
            r.n_symbols,
            jnum(r.bimodality),
            jnum(r.eye_open),
            jnum(r.timing_var),
            jnum(r.snr_db),
            jnum(r.evm),
            jnum(r.line_viol),
            jnum(r.bit_struct),
        )
        .unwrap();
    }
    w.flush().unwrap();
    eprintln!("windows={} rows={}", starts.len(), rows.len());
}
