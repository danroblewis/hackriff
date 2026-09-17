//! **The headline control for T-414: an empty sky must be empty at every sample rate.**
//!
//! T-322 measured the defect this guards. `AcquisitionConfig`'s old default was a bare
//! peak-to-mean ratio of 2.5, sized for a short profile — but the number of code-phase cells the
//! maximum is taken over is the *samples in one 1 ms code period*, which the sample rate decides:
//! 2046 at the minimum acquirable rate, 4000 at 4 Msps, 20 000 at 20 Msps. A fixed bar sits
//! further and further below the noise-only maximum as the rate rises, and acquisition returned
//! **all 32 satellites out of pure noise**.
//!
//! That failure is silent and *inverted*. It does not report a fault; it reports an intact
//! constellation, so the jamming assessment reading it never fires. A capability that always says
//! "fine" is worse than one that is absent, because absence is visible.
//!
//! So this test runs the real correlator over real noise at all three rates and asserts **zero**
//! acquisitions at the crate default — and, on the *same* profiles, counts how many satellites
//! would have cleared 2.5. Both numbers come out of one search, so the fix and the defect are
//! measured against each other rather than against separate scenes.
//!
//! The partner control is in `below_noise_acquisition.rs`: a satellite ~20 dB under the noise
//! floor is still recovered **by PRN** at the same default. Nothing from noise, everything real
//! still found — either half alone is trivially satisfiable, and neither is satisfiable by
//! raising the bar until the sky goes quiet.

use hk_gnss::{
    AcquireError, AcquisitionConfig, AcquisitionThreshold, KnownCodeLed, PrnCodebook, acquire,
};
use num_complex::Complex32;

/// The bar that was the default, and the bug.
const OLD_FIXED_BAR: f32 = 2.5;

/// SplitMix64, so the noise is deterministic without an RNG dependency (repo convention).
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn unit(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 1.0) / (1u64 << 53) as f64
    }

    /// One unit-power complex Gaussian sample.
    fn complex_gaussian(&mut self) -> Complex32 {
        let (u1, u2) = (self.unit(), self.unit());
        let r = (-2.0 * u1.ln()).sqrt();
        let theta = 2.0 * std::f64::consts::PI * u2;
        let sigma = (0.5f64).sqrt();
        Complex32::new(
            (sigma * r * theta.cos()) as f32,
            (sigma * r * theta.sin()) as f32,
        )
    }
}

/// Non-coherent blocks, matching the production dwell.
const BLOCKS: usize = 4;

/// The production L1 dwell's own search: the whole 32-satellite codebook over ±5 kHz in 500 Hz
/// steps, 1 ms coherent x 4 non-coherent. Nothing here is narrowed to make the control easier —
/// "all 32 satellites out of pure noise" is the measurement being reproduced, so the codebook is
/// whole, and a narrower Doppler grid would be a smaller search than the one that ships.
fn config(sample_rate_hz: f64) -> AcquisitionConfig {
    AcquisitionConfig {
        sample_rate_hz,
        doppler_max_hz: 5_000.0,
        doppler_step_hz: 500.0,
        coherent_ms: 1,
        noncoherent_blocks: BLOCKS,
        ..Default::default()
    }
}

/// **Zero acquisitions from noise at 2.046, 4 and 20 Msps, at the crate default.**
#[test]
fn noise_alone_yields_no_satellites_at_any_rate() {
    let book = PrnCodebook::gps_l1_ca();
    let led = KnownCodeLed::with_codebook(&book);
    let mut rng = Rng(0x5EED_0414_0000);

    for rate in [2_046_000.0f64, 4.0e6, 20.0e6] {
        let cfg = config(rate);
        let per_ms = (rate / 1000.0) as usize;
        let iq: Vec<Complex32> = (0..per_ms * BLOCKS)
            .map(|_| rng.complex_gaussian())
            .collect();

        let result = acquire(&led, &book, &iq, &cfg).expect("acquisition runs");

        // The counterfactual, off the *same* profiles: what the old fixed bar would have done.
        let would_have = result
            .searched
            .iter()
            .filter(|s| s.peak_ratio >= OLD_FIXED_BAR)
            .count();
        let worst = result
            .searched
            .iter()
            .map(|s| s.peak_ratio)
            .fold(0.0f32, f32::max);

        eprintln!(
            "{:>6.3} Msps: {per_ms} code phases x {} Doppler bins = {} cells; derived bar \
             {:.2}; loudest noise peak/mean {worst:.2}; acquired {} of {}; the old fixed \
             {OLD_FIXED_BAR} would have acquired {would_have} of {}",
            rate / 1e6,
            result.search_cells / per_ms,
            result.search_cells,
            result.threshold_ratio,
            result.acquired.len(),
            book.len(),
            book.len(),
        );

        assert!(
            result.acquired.is_empty(),
            "{:.3} Msps: acquired {} satellites from pure noise at the default bar {:.2}: {:?}",
            rate / 1e6,
            result.acquired.len(),
            result.threshold_ratio,
            result.acquired
        );

        // Anti-vacuity: if the old bar would also have found nothing here, this scene is not the
        // one that exposed the defect and the assertion above proves nothing.
        assert!(
            would_have > 0,
            "{:.3} Msps: the old fixed bar of {OLD_FIXED_BAR} found nothing either, so this \
             noise does not reproduce T-322's measurement and the control is vacuous",
            rate / 1e6
        );
    }
}

/// The bar the default derives is not one number: it tracks the search it is for. Printed
/// alongside the control above so the two are read together.
#[test]
fn the_default_bar_tracks_the_rate_it_is_used_at() {
    let mut last = 0.0f32;
    for rate in [2_046_000.0f64, 4.0e6, 20.0e6] {
        let cfg = config(rate);
        let bar = cfg.peak_to_mean_bar().expect("derivable");
        assert!(
            bar > last,
            "a ten-times-wider search must cost more, not the same: {last} -> {bar}"
        );
        assert!(
            bar > OLD_FIXED_BAR * 2.0,
            "the derived bar {bar} at {rate} Hz is near the old fixed {OLD_FIXED_BAR}; then the \
             old default was not the trap it is documented to be"
        );
        last = bar;
    }
}

/// **The refusal, at the rates that matter.** A caller that fixes 2.5 explicitly does not get a
/// silently useless detector; it gets an error naming the geometry that makes the number wrong.
#[test]
fn an_explicit_2_5_is_refused_at_every_acquirable_rate() {
    for rate in [2_046_000.0f64, 4.0e6, 20.0e6] {
        let cfg = AcquisitionConfig {
            threshold: AcquisitionThreshold::PeakToMean(OLD_FIXED_BAR),
            ..config(rate)
        };
        let err = cfg
            .peak_to_mean_bar()
            .expect_err("2.5 must never be accepted");
        assert!(
            matches!(err, AcquireError::ThresholdUnsound { .. }),
            "{rate} Hz: {err:?}"
        );
        eprintln!("{:>6.3} Msps refusal: {err}", rate / 1e6);
    }
}

/// **Calibration, not a gate.** Runs the real correlator over many independent noise realisations
/// and prints the empirical distribution of the per-satellite peak-to-mean, so the analytic tail
/// can be checked against the thing it models rather than against itself. `--ignored`, because it
/// is minutes of FFTs.
#[test]
#[ignore = "calibration sweep; minutes of FFTs"]
fn measure_the_noise_only_peak_distribution() {
    const RATE: f64 = 2_046_000.0;
    const REALISATIONS: usize = 40;
    let cfg = config(RATE);
    let book = PrnCodebook::gps_l1_ca();
    let led = KnownCodeLed::with_codebook(&book);
    let mut rng = Rng(0xC0FF_EE00_0414);
    let per_ms = (RATE / 1000.0) as usize;
    let mut peaks: Vec<f32> = Vec::new();
    for _ in 0..REALISATIONS {
        let iq: Vec<Complex32> = (0..per_ms * BLOCKS)
            .map(|_| rng.complex_gaussian())
            .collect();
        let r = acquire(&led, &book, &iq, &cfg).unwrap();
        peaks.extend(r.searched.iter().map(|s| s.peak_ratio));
    }
    peaks.sort_by(f32::total_cmp);
    let n = peaks.len();
    let q = |p: f64| peaks[((n as f64 * p) as usize).min(n - 1)];
    eprintln!(
        "{n} per-satellite searches at {:.3} Msps, {} cells, analytic bar {:.2}\n\
         median {:.2}  p90 {:.2}  p99 {:.2}  p99.9 {:.2}  max {:.2}",
        RATE / 1e6,
        cfg.search_cells().unwrap(),
        cfg.peak_to_mean_bar().unwrap(),
        q(0.5),
        q(0.9),
        q(0.99),
        q(0.999),
        peaks[n - 1]
    );
    for bar in [6.0f32, 6.5, 7.0, 7.5, 8.0, 8.5, 9.0] {
        let over = peaks.iter().filter(|&&p| p >= bar).count();
        eprintln!(
            "  bar {bar:.1}: {over}/{n} = {:.2e}",
            over as f64 / n as f64
        );
    }
}
