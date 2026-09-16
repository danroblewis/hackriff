//! T-267 (C23): control-channel hunting, blind, through the mock SDR device.
//!
//! The scene holds a continuous C4FM control channel, a continuous 4FSK **decoy** with no
//! framing, and bursty NBFM, all on the 12.5 kHz LMR raster. The run is handed the recording
//! through the device interface with **no frequency hint**: the only frequency the hunt is told
//! is the device's own tuned centre, which the mock reports from the recording exactly as a real
//! HackRF would report where it is tuned. Truth is read afterwards, purely to assert.
//!
//! The claim under test is C23's method and its named pitfall together:
//!
//! - candidacy = continuous occupancy (FCO) on the LMR raster;
//! - confirmation = frame sync **and** valid CRC;
//! - and the decoy — which satisfies candidacy perfectly — is rejected.

use std::path::PathBuf;

use hk_core::Source;
use hk_demod::fsk::{C4fmConfig, C4fmDemod};
use hk_detect::trunk::{
    CcCandidate, CcConfirmer, MIN_CC_FCO, RASTER_TOLERANCE_HZ, RasterFit, best_lmr_raster,
};
use hk_e2e::blind::strip_truth;
use hk_e2e::{SynthRequest, synth_or_skip};
use num_complex::Complex32;

const T267: &str = "T-267";

/// Decimation from the capture rate to a rate the C4FM demodulator can work at cheaply.
const DECIM: usize = 10;
/// Occupancy window, seconds.
const WINDOW_S: f64 = 0.01;
/// How far above the band's own noise floor a window counts as occupied, dB.
///
/// A priori: this is an occupancy threshold, not a confirmation threshold. It decides only which
/// channels are worth demodulating; moving it can add or remove candidates but can never confirm
/// one, because confirmation is sync + CRC. 6 dB is the conventional "clearly above the floor".
const OCCUPIED_MARGIN_DB: f64 = 6.0;

fn tmp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hk-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&d).expect("temp dir");
    d
}

/// Windowed-sinc low-pass taps, cutoff `fc` Hz at `fs`.
fn lowpass(fs: f64, fc: f64, n: usize) -> Vec<f32> {
    let m = n as f64 - 1.0;
    let mut t: Vec<f32> = (0..n)
        .map(|i| {
            let x = i as f64 - m / 2.0;
            let sinc = if x.abs() < 1e-9 {
                2.0 * fc / fs
            } else {
                (std::f64::consts::TAU * fc * x / fs).sin() / (std::f64::consts::PI * x)
            };
            // Hamming window.
            let w = 0.54 - 0.46 * (std::f64::consts::TAU * i as f64 / m).cos();
            (sinc * w) as f32
        })
        .collect();
    let s: f32 = t.iter().sum();
    for v in &mut t {
        *v /= s;
    }
    t
}

/// Mixes `offset_hz` to baseband and decimates by [`DECIM`], anti-aliasing on the way.
///
/// This is deliberately a wide anti-alias filter, not a 12.5 kHz channel filter: the C4FM
/// demodulator applies its own channel filter, and leaving adjacent energy in means neighbouring
/// raster channels also reach candidacy. That is realistic, and harmless — an extra candidate
/// costs a demodulation and is then rejected by sync + CRC, which is the whole design.
fn channelize(x: &[Complex32], fs: f64, offset_hz: f64, taps: &[f32]) -> Vec<Complex32> {
    let w = -std::f64::consts::TAU * offset_hz / fs;
    let n = x.len();
    let mut out = Vec::with_capacity(n / DECIM + 1);
    let half = taps.len() / 2;
    let mut i = half;
    while i + half < n {
        let mut acc = Complex32::new(0.0, 0.0);
        for (k, &t) in taps.iter().enumerate() {
            let j = i + k - half;
            let ph = (w * j as f64) % std::f64::consts::TAU;
            acc += x[j] * Complex32::new(ph.cos() as f32, ph.sin() as f32) * t;
        }
        out.push(acc);
        i += DECIM;
    }
    out
}

fn db(p: f64) -> f64 {
    10.0 * p.max(1e-30).log10()
}

/// Per-window mean power of `x`.
fn window_powers(x: &[Complex32], win: usize) -> Vec<f64> {
    x.chunks(win)
        .filter(|c| c.len() == win)
        .map(|c| c.iter().map(|s| f64::from(s.norm_sqr())).sum::<f64>() / win as f64)
        .collect()
}

fn median(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    s[s.len() / 2]
}

/// MSB-first dibits of a hex string.
fn hex_dibits(hex: &str) -> Vec<u8> {
    (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap())
        .flat_map(|b| [(b >> 6) & 3, (b >> 4) & 3, (b >> 2) & 3, b & 3])
        .collect()
}

/// Fewest symbol mismatches of `pattern` at any alignment in `stream`.
fn min_mismatch(stream: &[u8], pattern: &[u8]) -> usize {
    if stream.len() < pattern.len() {
        return pattern.len();
    }
    (0..=stream.len() - pattern.len())
        .map(|i| {
            (0..pattern.len())
                .filter(|&k| stream[i + k] != pattern[k])
                .count()
        })
        .min()
        .unwrap_or(pattern.len())
}

/// One raster channel's measurement.
struct Channel {
    index: i64,
    center_hz: f64,
    fco: f64,
    raster: RasterFit,
    baseband: Vec<Complex32>,
}

#[test]
fn t267_control_channel_confirmed_by_sync_and_crc_and_the_continuous_decoy_rejected() {
    let out = synth_or_skip!(
        SynthRequest::new("trunk_control_channel")
            .seed(267)
            .param("duration_s", 1.0)
    );
    let fx = out.fixture(0).unwrap();

    // ---- The device. A truth-stripped copy is served, so nothing downstream can read the
    // annotations even by accident; the hunt below never opens the fixture.
    let src = tmp_dir("t267-src");
    let blind_meta = strip_truth(&fx.meta_path, &src, "blind", 0.0).expect("strip truth");
    let driver = hk_core::MockSdrDriver::new(
        &blind_meta,
        hk_core::MockOptions {
            block_len: 1 << 16,
            ..hk_core::MockOptions::default()
        },
    )
    .expect("mock driver");
    let mut source = driver
        .open_mock(&driver.default_request())
        .expect("open mock");
    let fs = source.recording().sample_rate_hz;
    // The ONLY frequency the hunt is given: where the device says it is tuned.
    let tuned_center_hz = source.recording().center_hz;
    source.control().start().expect("start");

    let mut iq: Vec<Complex32> = Vec::new();
    let mut buf: Vec<Complex32> = Vec::new();
    while let Some(_hdr) = source.read_block(&mut buf).expect("read") {
        iq.extend_from_slice(&buf);
    }
    assert!(
        iq.len() as f64 > 0.5 * fs,
        "[{T267}] device delivered {} samples at {fs} Hz",
        iq.len()
    );
    eprintln!(
        "[{T267}] device: {} samples, fs {fs:.0} Hz, tuned {:.4} MHz",
        iq.len(),
        tuned_center_hz / 1e6
    );

    // ---- Sweep the LMR raster across the device's window. The raster is an a-priori standard
    // (12.5 kHz narrowband LMR), and its origin is the device's tuned centre — no truth anywhere.
    let raster_hz = 12_500.0;
    let usable = 0.8 * fs / 2.0;
    let max_ch = (usable / raster_hz).floor() as i64;
    let taps = lowpass(fs, 0.8 * fs / (2.0 * DECIM as f64), 63);
    let chan_fs = fs / DECIM as f64;
    let win = (WINDOW_S * chan_fs) as usize;

    let mut channels: Vec<Channel> = Vec::new();
    let mut floors = Vec::new();
    for index in -max_ch..=max_ch {
        let offset = index as f64 * raster_hz;
        let baseband = channelize(&iq, fs, offset, &taps);
        let p = window_powers(&baseband, win);
        if p.is_empty() {
            continue;
        }
        floors.push(median(&p));
        let center_hz = tuned_center_hz + offset;
        let raster = best_lmr_raster(center_hz, tuned_center_hz, RASTER_TOLERANCE_HZ)
            .expect("swept centres lie on the raster by construction");
        channels.push(Channel {
            index,
            center_hz,
            fco: f64::NAN,
            raster,
            baseband,
        });
    }

    // The band's own noise floor: the median of the per-channel medians. Most raster channels
    // are empty, so this is the floor, measured rather than assumed.
    let band_floor = median(&floors);
    let threshold = band_floor * 10f64.powf(OCCUPIED_MARGIN_DB / 10.0);
    eprintln!(
        "[{T267}] band floor {:.1} dB, occupied threshold {:.1} dB over {} raster channels",
        db(band_floor),
        db(threshold),
        channels.len()
    );

    for c in &mut channels {
        let p = window_powers(&c.baseband, win);
        let occ = p.iter().filter(|&&v| v >= threshold).count();
        c.fco = occ as f64 / p.len() as f64;
    }

    // ---- Candidacy: continuous occupancy on the raster. This is where a pure-FCO detector
    // would stop and be wrong.
    let candidates: Vec<&Channel> = channels.iter().filter(|c| c.fco >= MIN_CC_FCO).collect();
    eprintln!(
        "[{T267}] candidates (FCO >= {MIN_CC_FCO}): {:?}",
        candidates
            .iter()
            .map(|c| (c.index, format!("{:.3}", c.fco)))
            .collect::<Vec<_>>()
    );
    assert!(
        candidates.len() >= 2,
        "[{T267}] the control channel and the decoy must BOTH reach candidacy: {}",
        candidates.len()
    );

    // ---- Confirmation: frame sync AND valid CRC.
    let demod = C4fmDemod::new(C4fmConfig::default());
    let confirmer = CcConfirmer::default();
    let mut confirmed = Vec::new();
    for c in &candidates {
        let cand = CcCandidate::new(c.center_hz, raster_hz, c.fco, c.raster)
            .expect("FCO already cleared the candidacy floor");
        let Ok(sym) = demod.demodulate(&c.baseband, chan_fs, 0.0) else {
            continue;
        };
        let outcome = confirmer.scan(&sym.dibits);
        eprintln!(
            "[{T267}] channel {:+3} @ {:.4} MHz fco {:.3} -> {} dibits, sync {} crc_valid {}",
            c.index,
            c.center_hz / 1e6,
            c.fco,
            sym.dibits.len(),
            outcome.sync_hits,
            outcome.crc_valid
        );
        if let Some(cc) = confirmer.confirm(&cand, &sym.dibits) {
            confirmed.push((c.index, cc));
        }
    }

    // ---- Truth, opened only now, and only to check the answer.
    let scenario = fx.scenario().unwrap();
    let truth = &scenario.value["trunking"];
    let cc_truth_hz = truth["control_channel"]["rf_center_hz"].as_f64().unwrap();
    let decoy_truth_hz = truth["continuous_decoy"]["rf_center_hz"].as_f64().unwrap();

    // Recovery quality on the channel that truly holds the CC, in both symbol polarities. This
    // describes what the demodulator recovered; it decides nothing.
    if let Some(c) = channels
        .iter()
        .find(|c| (c.center_hz - cc_truth_hz).abs() <= raster_hz / 2.0)
        && let Ok(sym) = demod.demodulate(&c.baseband, chan_fs, 0.0)
    {
        let mut hist = [0usize; 4];
        for &d in &sym.dibits {
            hist[d as usize] += 1;
        }
        let sync = hex_dibits(truth["control_channel"]["sync_hex"].as_str().unwrap());
        let inverted: Vec<u8> = sym
            .dibits
            .iter()
            .map(|&d| [2u8, 3, 0, 1][d as usize])
            .collect();
        eprintln!(
            "[{T267}] recovery on {:+}: hist {hist:?}, outer {:.0} Hz, residual cfo {:.0} Hz; \
             best sync mismatch {}/{} as-is, {}/{} inverted",
            c.index,
            sym.outer_deviation_hz,
            sym.residual_cfo_hz,
            min_mismatch(&sym.dibits, &sync),
            sync.len(),
            min_mismatch(&inverted, &sync),
            sync.len()
        );
    }

    assert_eq!(
        confirmed.len(),
        1,
        "[{T267}] exactly one control channel confirmed, got {:?}",
        confirmed.iter().map(|(i, _)| *i).collect::<Vec<_>>()
    );
    let (_, cc) = &confirmed[0];
    assert!(
        (cc.cc_freq_hz() - cc_truth_hz).abs() <= raster_hz / 2.0,
        "[{T267}] confirmed {:.4} MHz but the CC is at {:.4} MHz",
        cc.cc_freq_hz() / 1e6,
        cc_truth_hz / 1e6
    );

    // The decoy passed candidacy and must NOT have been confirmed. This is C23's pitfall.
    assert!(
        (cc.cc_freq_hz() - decoy_truth_hz).abs() > raster_hz / 2.0,
        "[{T267}] the confirmed channel is the continuous decoy at {:.4} MHz",
        decoy_truth_hz / 1e6
    );
    let decoy_was_a_candidate = candidates
        .iter()
        .any(|c| (c.center_hz - decoy_truth_hz).abs() <= raster_hz / 2.0);
    assert!(
        decoy_was_a_candidate,
        "[{T267}] the decoy must reach candidacy, or it is not testing the pitfall"
    );

    // Evidence, and the scope line: confirming a CC does not name its protocol.
    let ev = cc.evidence();
    assert!(
        ev.sync_hits() >= 2 && ev.crc_valid() >= 2,
        "evidence {ev:?}"
    );
    assert_eq!(cc.protocol(), hk_model::TrunkProtocol::Unknown);
    eprintln!(
        "[{T267}] confirmed {:.4} MHz: sync {} crc_valid {}/{} pattern {} \
         (truth {:.4} MHz, decoy {:.4} MHz rejected)",
        cc.cc_freq_hz() / 1e6,
        ev.sync_hits(),
        ev.crc_valid(),
        ev.crc_checked(),
        ev.pattern(),
        cc_truth_hz / 1e6,
        decoy_truth_hz / 1e6
    );
}
