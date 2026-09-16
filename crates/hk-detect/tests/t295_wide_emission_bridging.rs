//! T-295: does a wide emission bridge a neighbouring emitter into one box, and is that specific
//! to a **swept** carrier or general to any wide emission?
//!
//! T-255 saw one 394 kHz box spanning three emitter species in the LoRa scene and the ticket read
//! that as the connected-component labeller bridging them across a chirp's wide per-frame
//! footprint. This is the control that decides the question, because the two runs below differ in
//! exactly one thing: the wide emitter at +100 kHz either **sweeps** its 125 kHz channel every
//! 4.096 ms (an SF9-shaped LoRa chirp, so its instantaneous footprint is narrow and its per-frame
//! footprint is the whole channel), or **stands still** and fills the same channel at the same
//! mean power for the same span. The narrow neighbour, the noise, the seeds and the chain are
//! identical.
//!
//! The steady variant is a fixed-random-phase multitone at one tone per detector bin rather than
//! band-limited noise, deliberately: tones have no out-of-band leakage at all, so nothing but the
//! labeller's own connectivity can join the two emitters, and a positive result cannot be blamed
//! on generator splatter.
//!
//! - If only the sweep bridges, the defect is sweep-specific.
//! - If both bridge, it is general to the labeller on any wide emission.
//! - If neither bridges, there is no bridging in the labeller to fix, and a merged box in a real
//!   run came from somewhere else.

mod common;

use std::f64::consts::TAU;

use common::*;
use hk_detect::{Detector, DetectorConfig};
use hk_dsp::synth::{Rng, complex_noise};
use hk_e2e::Cf32;
use hk_model::SurveyId;
use num_complex::Complex32;

const T295: &str = "T-295";
const FS: f64 = 500e3;
const CENTER_HZ: f64 = 903.0e6;
const DUR_S: f64 = 0.30;
/// Noise power per sample, full scale 1 (−40 dBFS, the T-255 scene's receiver).
const NOISE: f64 = 1e-4;

/// The wide emitter: 125 kHz at +100 kHz, on for most of the run.
const WIDE_OFFSET_HZ: f64 = 100e3;
const WIDE_BW_HZ: f64 = 125e3;
const WIDE_ON: (f64, f64) = (0.05, 0.25);
/// Loud enough that the whole channel clears the extend threshold in one frame. At the scene's
/// 12 dB *in the channel* a 125 kHz emitter is −9 dB in a 977 Hz bin and breaks into a handful of
/// narrow boxes — the first draft of this control did exactly that, and a control whose wide
/// emitter never becomes a wide box cannot answer whether a wide box swallows its neighbour.
const WIDE_SNR_DB: f64 = 25.0;
/// SF9 at 125 kHz: the sweep folds every 4.096 ms, one detector frame.
const SWEEP_PERIOD_S: f64 = 4.096e-3;

/// The narrow neighbour: 24 kHz of 2-FSK at −60 kHz, inside the wide emitter's span.
const NARROW_OFFSET_HZ: f64 = -60e3;
const NARROW_BW_HZ: f64 = 24e3;
const NARROW_ON: (f64, f64) = (0.10, 0.13);
const NARROW_SNR_DB: f64 = 16.0;

/// Sample range of an on-air span.
fn span(on: (f64, f64)) -> (usize, usize) {
    (
        (on.0 * FS).round() as usize,
        ((on.1 * FS).round() as usize).min((DUR_S * FS) as usize),
    )
}

/// Absolute power for `snr_db` measured in `bw_hz` against the scene's noise density.
fn power(snr_db: f64, bw_hz: f64) -> f64 {
    NOISE / FS * bw_hz * undb(snr_db)
}

/// Adds the narrow neighbour: rectangular 2-FSK, ±9.6 kHz at 4800 Bd.
fn add_narrow(x: &mut [Complex32], rng: &mut Rng) {
    let (lo, hi) = span(NARROW_ON);
    let sps = (FS / 4800.0).round().max(1.0) as usize;
    let bits: Vec<f64> = (0..(hi - lo) / sps + 2)
        .map(|_| if rng.unit() < 0.5 { -9600.0 } else { 9600.0 })
        .collect();
    let amp = power(NARROW_SNR_DB, NARROW_BW_HZ).sqrt();
    let mut phase = 0.0f64;
    for (j, i) in (lo..hi).enumerate() {
        phase += TAU * (NARROW_OFFSET_HZ + bits[(j / sps).min(bits.len() - 1)]) / FS;
        x[i] += Complex32::new((amp * phase.cos()) as f32, (amp * phase.sin()) as f32);
    }
}

/// The swept variant: a LoRa-shaped up-chirp folding through the channel every symbol, each symbol
/// starting at its own point in the channel.
///
/// That per-symbol start offset is what a LoRa data symbol *is*, and it is load-bearing here:
/// without it the fold is exactly frame-synchronous with the 1.024 ms sub-FFTs the 4.096 ms frame
/// averages, every frame sees the same quarter of the sweep, and the emitter comes out as four
/// fixed 31.25 kHz lines instead of one wide box. The first draft of this control did that.
fn add_sweep(x: &mut [Complex32], rng: &mut Rng) {
    let (lo, hi) = span(WIDE_ON);
    let amp = power(WIDE_SNR_DB, WIDE_BW_HZ).sqrt();
    let sym = (SWEEP_PERIOD_S * FS).round() as usize;
    let starts: Vec<f64> = (0..(hi - lo) / sym + 2).map(|_| rng.unit()).collect();
    let mut phase = 0.0f64;
    for (j, i) in (lo..hi).enumerate() {
        let u = (starts[(j / sym).min(starts.len() - 1)] + (j % sym) as f64 / sym as f64).fract();
        phase += TAU * (WIDE_OFFSET_HZ + (u - 0.5) * WIDE_BW_HZ) / FS;
        x[i] += Complex32::new((amp * phase.cos()) as f32, (amp * phase.sin()) as f32);
    }
}

/// The steady variant: the same band and mean power, filled by one fixed-random-phase tone per
/// detector bin, so it is wide in *every* frame and leaks nothing outside the channel.
fn add_steady(x: &mut [Complex32], rng: &mut Rng, bin_hz: f64) {
    let (lo, hi) = span(WIDE_ON);
    let k_max = (0.5 * WIDE_BW_HZ / bin_hz).round() as i32;
    let tones = (2 * k_max + 1) as f64;
    let amp = (power(WIDE_SNR_DB, WIDE_BW_HZ) / tones).sqrt() as f32;
    let mut cur = Vec::new();
    let mut rot = Vec::new();
    for k in -k_max..=k_max {
        let f = WIDE_OFFSET_HZ + f64::from(k) * bin_hz;
        cur.push(Complex32::from_polar(amp, (rng.unit() * TAU) as f32));
        rot.push(Complex32::from_polar(1.0, (TAU * f / FS) as f32));
    }
    for v in x[lo..hi].iter_mut() {
        let mut acc = Complex32::default();
        for (c, r) in cur.iter_mut().zip(&rot) {
            acc += *c;
            *c *= r;
        }
        *v += acc;
    }
}

/// One run: `(widest box Hz, boxes that overlap both emitters, one-line rows)`.
fn run(kind: &str) -> (f64, usize, Vec<String>) {
    let n = (DUR_S * FS) as usize;
    let chain = ChainConfig::new(512, 4);
    let bin_hz = FS / chain.fft_len as f64;
    let mut rng = Rng::new(0x295);
    let mut x = complex_noise(&mut rng, n, NOISE);
    add_narrow(&mut x, &mut rng);
    match kind {
        "sweep" => add_sweep(&mut x, &mut rng),
        _ => add_steady(&mut x, &mut rng, bin_hz),
    }
    let iq = to_ci8(
        &x.iter()
            .map(|v| Cf32 { re: v.re, im: v.im })
            .collect::<Vec<_>>(),
    );
    let mut det = Detector::new(DetectorConfig::new(SurveyId::new())).unwrap();
    let prov = provenance(CENTER_HZ, FS, 24.0);
    let (got, frames) = replay_ci8(&iq, FS, &[(0, prov)], &chain, &mut det);

    let narrow = (
        CENTER_HZ + NARROW_OFFSET_HZ - NARROW_BW_HZ / 2.0,
        CENTER_HZ + NARROW_OFFSET_HZ + NARROW_BW_HZ / 2.0,
    );
    let wide = (
        CENTER_HZ + WIDE_OFFSET_HZ - WIDE_BW_HZ / 2.0,
        CENTER_HZ + WIDE_OFFSET_HZ + WIDE_BW_HZ / 2.0,
    );
    let mut widest = 0.0f64;
    let mut bridged = 0;
    let mut rows = Vec::new();
    for d in got.sorted() {
        let w = d.f_hi_hz - d.f_lo_hz;
        widest = widest.max(w);
        let hits = |b: (f64, f64)| d.f_lo_hz <= b.1 && b.0 <= d.f_hi_hz;
        if hits(narrow) && hits(wide) {
            bridged += 1;
            rows.push(format!("BRIDGED {}", describe(d, FS)));
        } else {
            rows.push(describe(d, FS));
        }
    }
    eprintln!(
        "[{T295}/{kind}] {} boxes over {frames} frames, widest {:.1} kHz, {bridged} spanning both \
         the {:.0} kHz neighbour and the {:.0} kHz wide emitter {:.0} kHz away",
        rows.len(),
        widest / 1e3,
        NARROW_BW_HZ / 1e3,
        WIDE_BW_HZ / 1e3,
        (WIDE_OFFSET_HZ - NARROW_OFFSET_HZ).abs() / 1e3,
    );
    for r in &rows {
        eprintln!("[{T295}/{kind}]   {r}");
    }
    (widest, bridged, rows)
}

/// The invariant the ticket names: a wide emission must not swallow its neighbour, whether or not
/// it sweeps. Both arms run so the *reason* is recorded, not just the verdict.
#[test]
fn a_wide_emission_does_not_swallow_a_neighbour_whether_or_not_it_sweeps() {
    let (sweep_w, sweep_b, _) = run("sweep");
    let (steady_w, steady_b, _) = run("steady");
    eprintln!(
        "[{T295}] verdict: swept {sweep_b} bridged boxes (widest {:.1} kHz), steady {steady_b} \
         (widest {:.1} kHz) — {}",
        sweep_w / 1e3,
        steady_w / 1e3,
        match (sweep_b > 0, steady_b > 0) {
            (true, true) => "GENERAL to any wide emission",
            (true, false) => "SPECIFIC to swept carriers",
            (false, true) => "present only WITHOUT a sweep",
            (false, false) => "no bridging in the labeller on this scene",
        }
    );
    assert_eq!(
        (sweep_b, steady_b),
        (0, 0),
        "[{T295}] a wide emission bridged its neighbour into one box"
    );
}
