//! T-988 (SIGNAL-090): the explorer's window-3 capture, `lmr-461p125-nbfm-ctcss` (live HackRF,
//! San Francisco, 2026-09-25 06:45 PDT, 2.4 MS/s ci8 at 461.675 MHz), through the blind
//! sub-audible detector — the hard case of the ticket.
//!
//! **The evidence, and why this test asserts "no tone".** Two claims disagree about the 8.1 s
//! NBFM burst at 461.125 MHz: the explorer agent read 233.6 Hz, the independent oracle
//! (`py/fixtures/ctcss_ref.py`, T-985) 100.0 Hz. Neither is a CTCSS tone. The burst's
//! sub-audible spectrum is a **comb of lines every 16.67 Hz** (33.3, 50, 66.7, 100, 116.7, 133.3,
//! 166.7, 200, 216.7, 233.3 Hz, each 14–27 dB over the band median, a 60 ms periodic buzz such
//! as a TDMA radio's frame), and 100 Hz and 233.3 Hz are simply its two strongest members —
//! 233.3, not 233.6, and the per-second strongest line wanders between 66.6, 100 and 166.5 Hz.
//! A tone is one line; the detector's comb guard reports `none` with the comb spacing, which is
//! what the capture actually shows.
//!
//! The capture is **external** (72 MB; `fixtures/manifest.json` `status: external`). This test
//! reads it from `HK_EXPLORER_CAPTURES` (default `~/.hackriff-ops/explorer/captures/20260925`)
//! and passes vacuously, saying so, when it is absent.

use std::path::PathBuf;

use hk_demod::dsp::{Discriminator, FirDecimator, lowpass_taps};
use hk_demod::subaudible::{SubaudibleConfig, SubaudibleDetector};
use hk_model::SubaudibleKind;
use num_complex::Complex32;

const FS: f64 = 2.4e6;
const TUNED_HZ: f64 = 461.675e6;
const CHANNEL_HZ: f64 = 461.125e6;
/// The burst (explorer truth: t_start 6.9 s, 8.1 s long).
const BURST_S: (f64, f64) = (6.9, 8.1);

fn capture() -> Option<PathBuf> {
    let dir = std::env::var_os("HK_EXPLORER_CAPTURES")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(".hackriff-ops/explorer/captures/20260925"))
        })?;
    let p = dir.join("lmr-461p125-nbfm-ctcss.sigmf-data");
    p.is_file().then_some(p)
}

#[test]
fn window3_461p125_burst_is_a_buzz_comb_not_a_ctcss_tone() {
    let Some(path) = capture() else {
        eprintln!("SKIP: lmr-461p125-nbfm-ctcss.sigmf-data (external capture) not present");
        return;
    };
    let raw = std::fs::read(&path).unwrap();
    let (s0, n) = ((BURST_S.0 * FS) as usize, (BURST_S.1 * FS) as usize);
    let iq = &raw[2 * s0..2 * (s0 + n)];
    // Channel: mix to 0 Hz, 2.4 MS/s → 240 kS/s → 48 kS/s, ±7.5 kHz.
    let mut d1 = FirDecimator::<Complex32>::new(lowpass_taps(FS, 10e3, 100e3, 60.0).unwrap(), 10);
    let mut d2 =
        FirDecimator::<Complex32>::new(lowpass_taps(FS / 10.0, 7.5e3, 18e3, 60.0).unwrap(), 5);
    let mut disc = Discriminator::new(48_000.0);
    let mut det = SubaudibleDetector::new(SubaudibleConfig::default(), 48_000.0).unwrap();
    let step = -std::f64::consts::TAU * (CHANNEL_HZ - TUNED_HZ) / FS;
    let mut out = Vec::with_capacity(48_000);
    for (k, s) in iq.chunks_exact(2).enumerate() {
        let x = Complex32::new(f32::from(s[0] as i8), f32::from(s[1] as i8));
        let ph = step * (s0 + k) as f64;
        let lo = Complex32::new(ph.cos() as f32, ph.sin() as f32);
        if let Some(y) = d1.push(x * lo)
            && let Some(z) = d2.push(y)
        {
            out.push(disc.push(z));
            if out.len() == 4_800 {
                det.push(&out, true);
                out.clear();
            }
        }
    }
    det.push(&out, true);
    let r = det.report();
    eprintln!("461.125 MHz burst: {r:?}");
    assert!(r.analysed_s >= 7.9, "the whole window analysed: {r:?}");
    assert_eq!(
        r.kind,
        SubaudibleKind::None,
        "the burst carries a 16.67 Hz buzz comb, not a CTCSS tone (neither the explorer's 233.6 Hz \
         nor the oracle's 100 Hz): {r:?}"
    );
    let why = r.reason.as_deref().unwrap_or_default();
    assert!(why.contains("comb"), "named as a comb: {why}");
    let spacing: f64 = why
        .split("every ")
        .nth(1)
        .and_then(|s| s.split(' ').next())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);
    // The comb guard names the first sub-multiple whose members clear the guard: 16.67 Hz or a
    // multiple of it (33.3 Hz: the odd members alone already clear it).
    let k = (spacing / (50.0 / 3.0)).round();
    assert!(
        k >= 1.0 && (spacing - k * 50.0 / 3.0).abs() < 0.3,
        "a comb on the 16.67 Hz grid: {why}"
    );
}
