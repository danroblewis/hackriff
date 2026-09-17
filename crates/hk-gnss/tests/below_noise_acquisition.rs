//! **Why GPS L1 is the documented exception to blind-first, demonstrated rather than asserted.**
//!
//! The project's rule is that signals are found by blind detection first. This test measures the
//! one case where that is physically impossible, on a single piece of synthetic IQ:
//!
//! 1. [`blind_energy_detection_cannot_see_l1`] — the periodogram of signal-plus-noise is
//!    statistically indistinguishable from the periodogram of the *same noise with the satellite
//!    removed*. There is no bump to find. No CFAR threshold separates them, because the thing
//!    being thresholded does not differ.
//! 2. [`known_code_correlation_recovers_the_satellite`] — despreading that same IQ against the
//!    published PRN code recovers the satellite, its code phase and its Doppler cleanly.
//!
//! Together those are the argument for the exception: not "blind detection is inconvenient here"
//! but "there is nothing for it to detect".
//!
//! **Unverified against hardware.** The IQ is synthetic at a stated C/N0. Confirming the real
//! sensitivity needs an active GNSS antenna on the bias-tee and a live L1 capture, which is
//! user-triggered.

use hk_dsp::fft::{CpuFft, FftBackend};
use hk_gnss::{AcquisitionConfig, KnownCodeLed, PrnCodebook, acquire, prn};
use num_complex::Complex32;

/// 2 samples per chip: the minimum that resolves the code, and a rate the HackRF can reach.
const SAMPLE_RATE_HZ: f64 = 2_046_000.0;
/// Samples in one 1 ms code period.
const SAMPLES_PER_CODE: usize = 2046;
/// Code periods generated. Four is under the 20 ms navigation bit, so no bit transition
/// interrupts coherent integration.
const BLOCKS: usize = 4;

/// The satellite hidden in the IQ.
const TRUTH_PRN: u8 = 11;
/// Its code delay, in samples (511 chips at 2 samples/chip).
const TRUTH_DELAY_SAMPLES: usize = 1022;
/// Its Doppler, deliberately off the search grid so recovery is not flattered.
const TRUTH_DOPPLER_HZ: f64 = 1_180.0;
/// Signal-to-noise ratio in the sampled 2.046 MHz band, dB. Around −20 dB is where real L1 sits:
/// roughly −128 dBm of signal under a thermal floor some 20–30 dB above it.
const SNR_DB: f64 = -20.0;

/// SplitMix64, so the scene is deterministic without an RNG dependency (repo convention).
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
        // (0, 1], so the log in Box–Muller is finite.
        ((self.next_u64() >> 11) as f64 + 1.0) / (1u64 << 53) as f64
    }

    /// One complex Gaussian sample with total power `power` (split across I and Q).
    fn complex_gaussian(&mut self, power: f64) -> Complex32 {
        let (u1, u2) = (self.unit(), self.unit());
        let r = (-2.0 * u1.ln()).sqrt();
        let theta = 2.0 * std::f64::consts::PI * u2;
        let sigma = (power / 2.0).sqrt();
        Complex32::new(
            (sigma * r * theta.cos()) as f32,
            (sigma * r * theta.sin()) as f32,
        )
    }
}

/// The sampled code replica for one period: chip `n` held for two samples.
fn replica(prn: u8) -> Vec<f32> {
    let code = prn::CaCode::new(prn).expect("PRN in range");
    (0..SAMPLES_PER_CODE)
        .map(|n| {
            let chip =
                ((n as f64) * prn::CHIP_RATE_HZ / SAMPLE_RATE_HZ) as usize % prn::CODE_LENGTH;
            f32::from(code.chips()[chip])
        })
        .collect()
}

/// Builds the scene twice over the *same* noise: once with the satellite present, once without.
/// Sharing the noise is what makes the periodogram comparison a fair one.
fn scene() -> (Vec<Complex32>, Vec<Complex32>) {
    let rep = replica(TRUTH_PRN);
    let n_total = SAMPLES_PER_CODE * BLOCKS;

    // Signal power 1; noise power set by the target SNR.
    let noise_power = 10f64.powf(-SNR_DB / 10.0);
    let mut rng = Rng(0x5EED_1575_0042);

    let mut with_sv = Vec::with_capacity(n_total);
    let mut noise_only = Vec::with_capacity(n_total);

    for n in 0..n_total {
        let noise = rng.complex_gaussian(noise_power);

        let chip = rep[(n + SAMPLES_PER_CODE * BLOCKS - TRUTH_DELAY_SAMPLES) % SAMPLES_PER_CODE];
        let phase = 2.0 * std::f64::consts::PI * TRUTH_DOPPLER_HZ * (n as f64) / SAMPLE_RATE_HZ;
        let carrier = Complex32::from_polar(1.0, phase as f32);
        let signal = carrier * chip;

        with_sv.push(signal + noise);
        noise_only.push(noise);
    }

    (with_sv, noise_only)
}

/// Largest periodogram bin, expressed in dB above the median bin — the statistic any energy
/// detector or CFAR thresholds on.
fn peak_excursion_db(iq: &[Complex32]) -> f64 {
    let n = SAMPLES_PER_CODE;
    let mut fft = CpuFft::new(n);
    let mut accum = vec![0f64; n];

    for block in iq.chunks_exact(n) {
        let mut buf: Vec<Complex32> = block.to_vec();
        fft.forward(&mut buf);
        for (a, b) in accum.iter_mut().zip(buf.iter()) {
            *a += f64::from(b.norm_sqr());
        }
    }

    let peak = accum.iter().copied().fold(0f64, f64::max);
    let mut sorted = accum.clone();
    sorted.sort_by(f64::total_cmp);
    let median = sorted[sorted.len() / 2];
    10.0 * (peak / median).log10()
}

/// **The reason the exception exists.** With the satellite present, the spectrum looks exactly
/// like noise: the largest periodogram excursion is no bigger than it is with the satellite
/// removed. A blind energy detector has nothing to threshold on, at any false-alarm rate.
#[test]
fn blind_energy_detection_cannot_see_l1() {
    let (with_sv, noise_only) = scene();

    let present = peak_excursion_db(&with_sv);
    let absent = peak_excursion_db(&noise_only);

    // Printed so the measurement is on the record, not just the verdict.
    eprintln!(
        "peak periodogram excursion above median: satellite present {present:.2} dB, \
         satellite absent {absent:.2} dB, difference {:.2} dB (SNR {SNR_DB} dB in \
         {:.3} MHz)",
        present - absent,
        SAMPLE_RATE_HZ / 1e6
    );

    assert!(
        (present - absent).abs() < 1.5,
        "the satellite changed the spectrum by {:.2} dB (present {present:.2} dB, absent \
         {absent:.2} dB above median). L1 is supposed to be invisible to energy detection; if \
         this scene shows a bump, the scene is wrong, not the rule.",
        present - absent
    );
}

/// The same IQ, despread against the published code, gives the satellite up: PRN, code phase and
/// Doppler. This is the known-signal pipeline *leading* — and it is the only thing that works.
#[test]
fn known_code_correlation_recovers_the_satellite() {
    let (with_sv, _) = scene();

    let book = PrnCodebook::gps_l1_ca();
    let led = KnownCodeLed::with_codebook(&book);
    let cfg = AcquisitionConfig {
        sample_rate_hz: SAMPLE_RATE_HZ,
        doppler_max_hz: 5_000.0,
        doppler_step_hz: 250.0,
        coherent_ms: 1,
        noncoherent_blocks: BLOCKS,
        // **The crate's own default**, not a number this test picked. T-414: a bar that is only
        // right at one sample rate is a trap, so the default is derived from the search geometry
        // and this test is one of the two controls on it — a real satellite must still be found.
        threshold: AcquisitionConfig::default().threshold,
    };

    let result = acquire(&led, &book, &with_sv, &cfg).expect("acquisition runs");

    let top = result
        .acquired
        .first()
        .unwrap_or_else(|| panic!("nothing acquired; searched {} PRNs", result.searched.len()));

    eprintln!(
        "acquired PRN {} at {:.0} Hz Doppler, {:.2} chips, peak/mean {:.1}, C/N0 ~{:.1} dB-Hz \
         (1 ms coherent x {BLOCKS} non-coherent, from IQ at SNR {SNR_DB} dB)",
        top.prn, top.doppler_hz, top.code_phase_chips, top.peak_ratio, top.cn0_dbhz
    );

    assert_eq!(top.prn, TRUTH_PRN, "acquired the wrong satellite: {top:?}");

    let truth_chips = (TRUTH_DELAY_SAMPLES as f64) * prn::CHIP_RATE_HZ / SAMPLE_RATE_HZ;
    let chip_err = (top.code_phase_chips - truth_chips).abs();
    assert!(
        chip_err <= 1.0,
        "code phase {:.2} chips, truth {truth_chips:.2}",
        top.code_phase_chips
    );

    let doppler_err = (top.doppler_hz - TRUTH_DOPPLER_HZ).abs();
    assert!(
        doppler_err <= cfg.doppler_step_hz,
        "Doppler {:.0} Hz, truth {TRUTH_DOPPLER_HZ:.0} Hz",
        top.doppler_hz
    );

    // Exactly one satellite is in the scene, so exactly one should come out.
    assert_eq!(
        result.acquired.len(),
        1,
        "false acquisitions: {:?}",
        result.acquired
    );
}

/// Acquisition is evidence of a known-code correlation, and says so. Nothing downstream can
/// mistake it for a blind measurement.
#[test]
fn the_result_records_that_the_codebook_led() {
    let (with_sv, _) = scene();
    let book = PrnCodebook::subset(&[TRUTH_PRN]).expect("PRN in range");
    let led = KnownCodeLed::with_codebook(&book);
    let cfg = AcquisitionConfig {
        sample_rate_hz: SAMPLE_RATE_HZ,
        noncoherent_blocks: BLOCKS,
        ..Default::default()
    };

    let result = acquire(&led, &book, &with_sv, &cfg).expect("acquisition runs");
    assert_eq!(
        result.evidence,
        hk_gnss::AcquisitionEvidence::KnownCodeCorrelation {
            codebook: "gps-l1-ca@is-gps-200"
        }
    );
}

/// An empty sky yields nothing. The correlator must not manufacture satellites out of noise,
/// or the "exception" would become a licence to invent signals.
#[test]
fn nothing_is_acquired_from_noise_alone() {
    let (_, noise_only) = scene();

    let book = PrnCodebook::subset(&[1, 5, 11, 17, 24]).expect("PRNs in range");
    let led = KnownCodeLed::with_codebook(&book);
    let cfg = AcquisitionConfig {
        sample_rate_hz: SAMPLE_RATE_HZ,
        noncoherent_blocks: BLOCKS,
        ..Default::default()
    };

    let result = acquire(&led, &book, &noise_only, &cfg).expect("acquisition runs");
    assert!(
        result.acquired.is_empty(),
        "acquired satellites from pure noise: {:?}",
        result.acquired
    );
}

/// The wrong code finds nothing in a scene that plainly contains a satellite — the codes are
/// near-orthogonal, which is what makes despreading selective rather than a general-purpose
/// signal finder.
#[test]
fn a_satellite_that_is_not_there_is_not_found() {
    let (with_sv, _) = scene();

    let absent: Vec<u8> = (1..=32u8).filter(|&p| p != TRUTH_PRN).take(8).collect();
    let book = PrnCodebook::subset(&absent).expect("PRNs in range");
    let led = KnownCodeLed::with_codebook(&book);
    let cfg = AcquisitionConfig {
        sample_rate_hz: SAMPLE_RATE_HZ,
        noncoherent_blocks: BLOCKS,
        ..Default::default()
    };

    let result = acquire(&led, &book, &with_sv, &cfg).expect("acquisition runs");
    assert!(
        result.acquired.is_empty(),
        "wrong codes acquired something: {:?}",
        result.acquired
    );
}
