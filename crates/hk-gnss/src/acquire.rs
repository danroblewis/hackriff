//! FFT parallel code-phase acquisition: the known-code-led search that is C36's exception.
//!
//! For one satellite, acquisition asks two questions at once — *what code phase* and *what
//! Doppler*. The code-phase axis is searched in one shot by correlating in the frequency domain
//! over a whole 1 ms code period:
//!
//! ```text
//! corr = IFFT( FFT(x · e^{−j2πf_d t}) · conj(FFT(replica)) )
//! ```
//!
//! and the Doppler axis is searched by repeating that over a grid of `f_d`. The peak of
//! `|corr|²` gives the code delay; its height against the rest of the profile says whether the
//! satellite is there at all.
//!
//! Coherent integration over 1 ms yields about `10·log₁₀(1023) ≈ 30 dB` of processing gain,
//! which is what pulls a signal sitting 20–30 dB under the floor up to a usable peak. Longer
//! coherent integration is limited by the 20 ms navigation bit period, so this searches
//! `coherent_ms` (≤ 10) coherently and accumulates the remaining blocks non-coherently.
//!
//! **This module is the exception.** See the crate docs for why it cannot be reached from the
//! blind detection path.

use hk_dsp::fft::{CpuFft, FftBackend};
use num_complex::Complex32;

use crate::prn::{CHIP_RATE_HZ, CODE_LENGTH, PrnCodebook};

/// Witness that a caller is deliberately on the known-signal-**led** path.
///
/// Its only constructor takes a [`PrnCodebook`], so every call site that acquires GNSS names the
/// exception in its own signature and is greppable. This is a marker for review, not the
/// enforcement — the enforcement is that the blind-detection crates do not depend on this crate
/// at all (crate docs, and `tests/blind_path_boundary.rs`).
#[derive(Debug)]
pub struct KnownCodeLed {
    codebook: &'static str,
}

impl KnownCodeLed {
    /// The only way to obtain the witness: you must already hold the published codes.
    pub fn with_codebook(codebook: &PrnCodebook) -> Self {
        Self {
            codebook: codebook.name(),
        }
    }

    /// The code set that led the search.
    pub fn codebook(&self) -> &'static str {
        self.codebook
    }
}

/// How an acquisition was reached. There is exactly one variant, and it exists so that a GNSS
/// result can never be mistaken downstream for a blind detection.
// Serialize only: `codebook` is a `&'static str` naming a compiled-in code set, which has no
// meaningful deserialised form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum AcquisitionEvidence {
    /// Despread against a published code set. **Known-signal-led, not blindly detected.**
    KnownCodeCorrelation {
        /// Identifier of the codebook, recorded as provenance.
        codebook: &'static str,
    },
}

/// Search settings.
#[derive(Clone, Copy, Debug)]
pub struct AcquisitionConfig {
    /// Sample rate of the supplied IQ.
    pub sample_rate_hz: f64,
    /// Widest Doppler searched either side of centre. ±5 kHz covers satellite motion; a
    /// receiver without a TCXO needs more, because its own clock error dominates.
    pub doppler_max_hz: f64,
    /// Doppler grid step. A step of `1/(2·T_coh)` costs under 2 dB at the bin edge.
    pub doppler_step_hz: f64,
    /// Code periods integrated coherently (1..=10; the navigation bit is 20 ms).
    pub coherent_ms: usize,
    /// Coherent blocks accumulated non-coherently on top.
    pub noncoherent_blocks: usize,
    /// Peak-to-mean ratio of the correlation profile above which a satellite counts as acquired.
    pub threshold_ratio: f32,
}

impl Default for AcquisitionConfig {
    fn default() -> Self {
        Self {
            sample_rate_hz: 2_046_000.0,
            doppler_max_hz: 5_000.0,
            doppler_step_hz: 250.0,
            coherent_ms: 1,
            noncoherent_blocks: 4,
            threshold_ratio: 2.5,
        }
    }
}

/// One acquired satellite.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SvAcquisition {
    /// Which satellite.
    pub prn: u8,
    /// Doppler at the peak, Hz.
    pub doppler_hz: f64,
    /// Code delay at the peak, in chips (0..1023).
    pub code_phase_chips: f64,
    /// Peak-to-mean ratio of the correlation profile. The detection statistic.
    pub peak_ratio: f32,
    /// Carrier-to-noise density **estimated** from the peak ratio, dB-Hz. Unverified against a
    /// reference receiver; see the crate docs.
    pub cn0_dbhz: f32,
}

/// The outcome of one search over a codebook.
#[derive(Clone, Debug)]
pub struct AcquisitionResult {
    /// Satellites whose peak ratio cleared the threshold, strongest first.
    pub acquired: Vec<SvAcquisition>,
    /// Every satellite searched, whether or not it cleared the threshold.
    pub searched: Vec<SvAcquisition>,
    /// How this result was reached. Always known-code correlation.
    pub evidence: AcquisitionEvidence,
}

/// Why a search could not run.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum AcquireError {
    /// The sample rate must give a whole number of samples per code period and at least two
    /// samples per chip.
    #[error(
        "sample rate {rate_hz} Hz gives {per_ms} samples per 1 ms code period; need a whole number >= 2046"
    )]
    SampleRate {
        /// The offending rate, rounded for display.
        rate_hz: i64,
        /// Samples per code period at that rate.
        per_ms: f64,
    },
    /// Not enough IQ for the requested integration.
    #[error("need {need} samples for {blocks} block(s) of {coherent_ms} ms, got {got}")]
    TooShort {
        /// Samples required.
        need: usize,
        /// Samples supplied.
        got: usize,
        /// Blocks requested.
        blocks: usize,
        /// Coherent length requested.
        coherent_ms: usize,
    },
    /// Config out of range.
    #[error("{0}")]
    Config(&'static str),
}

/// Searches `iq` for every satellite in `codebook`.
///
/// Requires a [`KnownCodeLed`] witness: this is the known-signal-**led** path, the documented
/// exception to blind-first. The result is never a blind detection and carries
/// [`AcquisitionEvidence`] saying so.
pub fn acquire(
    led: &KnownCodeLed,
    codebook: &PrnCodebook,
    iq: &[Complex32],
    cfg: &AcquisitionConfig,
) -> Result<AcquisitionResult, AcquireError> {
    if cfg.coherent_ms == 0 || cfg.coherent_ms > 10 {
        return Err(AcquireError::Config("coherent_ms must be 1..=10"));
    }
    if cfg.noncoherent_blocks == 0 {
        return Err(AcquireError::Config("noncoherent_blocks must be >= 1"));
    }
    if cfg.doppler_step_hz <= 0.0 || cfg.doppler_max_hz < 0.0 {
        return Err(AcquireError::Config("doppler grid must be positive"));
    }

    let per_ms = cfg.sample_rate_hz / 1000.0;
    let samples_per_code = per_ms.round() as usize;
    if (per_ms - per_ms.round()).abs() > 1e-6 || samples_per_code < 2 * CODE_LENGTH {
        return Err(AcquireError::SampleRate {
            rate_hz: cfg.sample_rate_hz.round() as i64,
            per_ms,
        });
    }

    let n = samples_per_code * cfg.coherent_ms;
    let need = n * cfg.noncoherent_blocks;
    if iq.len() < need {
        return Err(AcquireError::TooShort {
            need,
            got: iq.len(),
            blocks: cfg.noncoherent_blocks,
            coherent_ms: cfg.coherent_ms,
        });
    }

    let mut fft = CpuFft::new(n);
    let doppler_bins = doppler_grid(cfg);
    let t_coh_s = cfg.coherent_ms as f64 * 1.0e-3;

    // Doppler-wiped copies of each block, planned once and reused for every PRN: the expensive
    // per-satellite work is then one multiply plus one inverse transform per bin.
    let mut wiped: Vec<Vec<Complex32>> =
        Vec::with_capacity(doppler_bins.len() * cfg.noncoherent_blocks);
    for &f_d in &doppler_bins {
        for b in 0..cfg.noncoherent_blocks {
            let block = &iq[b * n..(b + 1) * n];
            let mut buf: Vec<Complex32> = block
                .iter()
                .enumerate()
                .map(|(i, &s)| {
                    let phase = -2.0 * std::f64::consts::PI * f_d * (i as f64) / cfg.sample_rate_hz;
                    s * Complex32::from_polar(1.0, phase as f32)
                })
                .collect();
            fft.forward(&mut buf);
            wiped.push(buf);
        }
    }

    let mut searched = Vec::with_capacity(codebook.len());
    let mut profile = vec![0f32; n];
    let mut work = vec![Complex32::default(); n];

    for code in codebook.codes() {
        // Sampled replica, repeated over the coherent window, transformed and conjugated.
        let mut replica: Vec<Complex32> = (0..n)
            .map(|i| {
                let chip = ((i as f64) * CHIP_RATE_HZ / cfg.sample_rate_hz) as usize % CODE_LENGTH;
                Complex32::new(f32::from(code.chips()[chip]), 0.0)
            })
            .collect();
        fft.forward(&mut replica);
        for r in replica.iter_mut() {
            *r = r.conj();
        }

        let mut best = SvAcquisition {
            prn: code.prn(),
            doppler_hz: 0.0,
            code_phase_chips: 0.0,
            peak_ratio: 0.0,
            cn0_dbhz: f32::NEG_INFINITY,
        };

        for (d, &f_d) in doppler_bins.iter().enumerate() {
            profile.fill(0.0);
            for b in 0..cfg.noncoherent_blocks {
                let spectrum = &wiped[d * cfg.noncoherent_blocks + b];
                for ((w, &s), &r) in work.iter_mut().zip(spectrum.iter()).zip(replica.iter()) {
                    *w = (s * r).conj();
                }
                // IFFT via the conjugate identity; the 1/N scaling is common to every bin and
                // cancels in the peak-to-mean ratio, so it is left off.
                fft.forward(&mut work);
                for (p, w) in profile.iter_mut().zip(work.iter()) {
                    *p += w.norm_sqr();
                }
            }

            let (idx, peak) = profile
                .iter()
                .enumerate()
                .fold(
                    (0usize, 0f32),
                    |acc, (i, &v)| {
                        if v > acc.1 { (i, v) } else { acc }
                    },
                );
            // Mean excludes a guard of ±1 chip around the peak so the peak does not inflate its
            // own noise reference.
            let guard = (cfg.sample_rate_hz / CHIP_RATE_HZ).ceil() as usize;
            let mut sum = 0f64;
            let mut count = 0usize;
            for (i, &v) in profile.iter().enumerate() {
                let d_idx = (i as isize - idx as isize)
                    .unsigned_abs()
                    .min(n - (i as isize - idx as isize).unsigned_abs());
                if d_idx > guard {
                    sum += f64::from(v);
                    count += 1;
                }
            }
            let mean = if count > 0 { sum / count as f64 } else { 0.0 };
            let ratio = if mean > 0.0 {
                (f64::from(peak) / mean) as f32
            } else {
                0.0
            };

            if ratio > best.peak_ratio {
                let chips = (idx as f64) * CHIP_RATE_HZ / cfg.sample_rate_hz % CODE_LENGTH as f64;
                best = SvAcquisition {
                    prn: code.prn(),
                    doppler_hz: f_d,
                    code_phase_chips: chips,
                    peak_ratio: ratio,
                    cn0_dbhz: cn0_from_ratio(ratio, t_coh_s),
                };
            }
        }
        searched.push(best);
    }

    let mut acquired: Vec<SvAcquisition> = searched
        .iter()
        .copied()
        .filter(|s| s.peak_ratio >= cfg.threshold_ratio)
        .collect();
    acquired.sort_by(|a, b| b.peak_ratio.total_cmp(&a.peak_ratio));

    Ok(AcquisitionResult {
        acquired,
        searched,
        evidence: AcquisitionEvidence::KnownCodeCorrelation {
            codebook: led.codebook(),
        },
    })
}

/// The Doppler bins searched, always including 0.
fn doppler_grid(cfg: &AcquisitionConfig) -> Vec<f64> {
    let steps = (cfg.doppler_max_hz / cfg.doppler_step_hz).floor() as i64;
    (-steps..=steps)
        .map(|k| k as f64 * cfg.doppler_step_hz)
        .collect()
}

/// C/N0 estimated from the correlation peak-to-mean ratio, dB-Hz.
///
/// `C/N0 ≈ 10·log₁₀((R − 1) / T_coh)`. A coarse estimator: it assumes the off-peak profile is
/// noise-only and ignores front-end bandwidth and quantisation, so it is reported as an estimate
/// and is **unverified against a reference receiver**.
fn cn0_from_ratio(ratio: f32, t_coh_s: f64) -> f32 {
    let excess = f64::from(ratio) - 1.0;
    if excess <= 0.0 {
        return f32::NEG_INFINITY;
    }
    (10.0 * (excess / t_coh_s).log10()) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn witness_records_the_codebook() {
        let book = PrnCodebook::gps_l1_ca();
        let led = KnownCodeLed::with_codebook(&book);
        assert_eq!(led.codebook(), "gps-l1-ca@is-gps-200");
    }

    #[test]
    fn evidence_is_always_known_code_led() {
        // The enum has one variant by design: a GNSS acquisition can never be recorded as a
        // blind detection.
        let e = AcquisitionEvidence::KnownCodeCorrelation {
            codebook: "gps-l1-ca@is-gps-200",
        };
        let AcquisitionEvidence::KnownCodeCorrelation { codebook } = e;
        assert_eq!(codebook, "gps-l1-ca@is-gps-200");
    }

    #[test]
    fn rejects_a_rate_that_undersamples_the_chips() {
        let book = PrnCodebook::subset(&[1]).unwrap();
        let led = KnownCodeLed::with_codebook(&book);
        let cfg = AcquisitionConfig {
            sample_rate_hz: 1_023_000.0,
            ..Default::default()
        };
        let err = acquire(&led, &book, &[Complex32::default(); 4096], &cfg).unwrap_err();
        assert!(matches!(err, AcquireError::SampleRate { .. }), "{err:?}");
    }

    #[test]
    fn rejects_short_input() {
        let book = PrnCodebook::subset(&[1]).unwrap();
        let led = KnownCodeLed::with_codebook(&book);
        let cfg = AcquisitionConfig::default();
        let err = acquire(&led, &book, &[Complex32::default(); 100], &cfg).unwrap_err();
        assert!(matches!(err, AcquireError::TooShort { .. }), "{err:?}");
    }

    #[test]
    fn doppler_grid_is_symmetric_and_contains_zero() {
        let cfg = AcquisitionConfig {
            doppler_max_hz: 1000.0,
            doppler_step_hz: 250.0,
            ..Default::default()
        };
        let g = doppler_grid(&cfg);
        assert_eq!(g.len(), 9);
        assert!(g.contains(&0.0));
        assert_eq!(g.first(), Some(&-1000.0));
        assert_eq!(g.last(), Some(&1000.0));
    }
}
