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

/// How the acquire/reject bar is set.
///
/// **A bare peak-to-mean ratio is not a portable setting, so it is not the default way to say
/// this.** The statistic being thresholded is the *maximum* over the search, and the size of the
/// search is decided by the sample rate (code-phase cells are the samples in one 1 ms code
/// period) and the Doppler grid — not by whoever writes the number down. The same ratio that is
/// strict over a 2046-cell profile is meaningless over a 20 000-cell one: a fixed 2.5 returns
/// **all 32 satellites out of pure noise** at 4 Msps and above, and a dead constellation then
/// reports as an intact one. A capability that always says "fine" is worse than one that is
/// absent, because absence is visible.
///
/// So the setting a caller owns is the **error rate it will tolerate**, which is portable; the
/// ratio is derived from it and the search's own geometry (see [`acquisition_threshold`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AcquisitionThreshold {
    /// Probability that a noise-only search reports *this* satellite. The peak-to-mean bar is
    /// derived per search from the cell count and non-coherent block count.
    ///
    /// Multiply by the codebook size for the chance of any false acquisition in one search:
    /// `1e-4` over 32 PRNs is about one spurious satellite in 300 dwells.
    FalseAlarm(f64),
    /// A fixed peak-to-mean ratio, for experiments and for reproducing a published number.
    ///
    /// [`acquire`] **refuses** a ratio that is unsound for the geometry it is given rather than
    /// running with it — see [`AcquireError::ThresholdUnsound`]. A wrong number that runs is
    /// exactly the failure this type exists to prevent.
    PeakToMean(f32),
}

impl Default for AcquisitionThreshold {
    fn default() -> Self {
        Self::FalseAlarm(DEFAULT_FALSE_ALARM)
    }
}

/// Per-satellite false-alarm probability used when a caller does not state one.
///
/// Over a 32-PRN codebook that is ~3 × 10⁻³ per dwell for *any* spurious satellite: rare enough
/// that a reported constellation can be believed, loose enough not to cost sensitivity — the
/// derived bar for the production dwell (2.046 Msps, ±5 kHz in 500 Hz steps, 4 blocks) is ≈ 7.1,
/// while a satellite 20 dB under the noise floor reaches ≈ 17.
pub const DEFAULT_FALSE_ALARM: f64 = 1.0e-4;

/// The loosest per-satellite false-alarm probability a *fixed* bar may imply and still be a
/// detector at all: at 0.5 a noise-only search is as likely to report the satellite as not.
///
/// This is deliberately far looser than [`DEFAULT_FALSE_ALARM`]. The refusal is meant to catch a
/// bar that cannot work, not to relitigate a caller's operating point.
const UNSOUND_ABOVE_P_FA: f64 = 0.5;

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
    /// How the acquire/reject bar is set. Defaults to a derived bar at
    /// [`DEFAULT_FALSE_ALARM`], which is correct at every sample rate.
    pub threshold: AcquisitionThreshold,
}

impl Default for AcquisitionConfig {
    fn default() -> Self {
        Self {
            sample_rate_hz: 2_046_000.0,
            doppler_max_hz: 5_000.0,
            doppler_step_hz: 250.0,
            coherent_ms: 1,
            noncoherent_blocks: 4,
            threshold: AcquisitionThreshold::default(),
        }
    }
}

impl AcquisitionConfig {
    /// Chances this search gives noise to produce the maximum that is thresholded.
    ///
    /// Two axes, and **both** come from the caller's rate and grid rather than anything it types:
    ///
    /// * **Code phase.** The profile is `samples_per_code · coherent_ms` long but repeats every
    ///   `samples_per_code` (the replica is the code tiled), so the distinct cells are the
    ///   samples in one 1 ms period — 2046 at the minimum rate, 4000 at 4 Msps, 20 000 at
    ///   20 Msps.
    /// * **Doppler.** The peak is maximised over the Doppler grid too, so every bin searched is
    ///   another look.
    ///
    /// **Every cell counts as an independent look, and that is deliberate.** Neighbouring code
    /// cells and neighbouring Doppler bins *are* correlated, so the true false-alarm rate is
    /// lower than this model's — the count is an over-estimate, which makes the derived bar an
    /// upper bound rather than a hopeful one. Discounting the correlation would buy a little
    /// sensitivity for a claim that is hard to justify and fails in the direction that hurts:
    /// an under-set bar reports satellites that are not there, and a GNSS capability that says
    /// "fine" when the sky is empty is worse than no capability at all. Measured against the real
    /// correlator, a discounted count came out about 5× optimistic; this one does not.
    ///
    /// Returns `None` when the rate cannot support acquisition at all; [`acquire`] reports that
    /// as [`AcquireError::SampleRate`].
    pub fn search_cells(&self) -> Option<usize> {
        let per_ms = self.sample_rate_hz / 1000.0;
        let samples_per_code = per_ms.round() as usize;
        if !per_ms.is_finite()
            || (per_ms - per_ms.round()).abs() > 1e-6
            || samples_per_code < 2 * CODE_LENGTH
        {
            return None;
        }
        // The grid is counted, not built: a caller may reach this before `acquire` has validated
        // the step, and a zero or non-finite step would ask for an unbounded vector.
        if !self.doppler_grid_is_usable() {
            return None;
        }
        let bins = 2 * (self.doppler_max_hz / self.doppler_step_hz).floor() as usize + 1;
        Some(samples_per_code.saturating_mul(bins))
    }

    /// A Doppler grid that can be enumerated at all: a positive, finite step and a finite,
    /// non-negative span.
    fn doppler_grid_is_usable(&self) -> bool {
        self.doppler_step_hz.is_finite()
            && self.doppler_step_hz > 0.0
            && self.doppler_max_hz.is_finite()
            && self.doppler_max_hz >= 0.0
    }

    /// The peak-to-mean ratio this search will actually apply, and the reason it is that number.
    ///
    /// Derives it for [`AcquisitionThreshold::FalseAlarm`]; validates it for
    /// [`AcquisitionThreshold::PeakToMean`], refusing a bar that noise clears more often than not.
    pub fn peak_to_mean_bar(&self) -> Result<f32, AcquireError> {
        if !self.doppler_grid_is_usable() {
            return Err(AcquireError::Config("doppler grid must be positive"));
        }
        let cells = self
            .search_cells()
            .ok_or_else(|| AcquireError::SampleRate {
                rate_hz: self.sample_rate_hz.round() as i64,
                per_ms: self.sample_rate_hz / 1000.0,
            })?;
        let blocks = self.noncoherent_blocks.max(1);
        match self.threshold {
            AcquisitionThreshold::FalseAlarm(p) => {
                if !(p.is_finite() && p > 0.0 && p < 1.0) {
                    return Err(AcquireError::Config(
                        "false-alarm probability must be in (0, 1)",
                    ));
                }
                Ok(acquisition_threshold(cells, blocks, p))
            }
            AcquisitionThreshold::PeakToMean(given) => {
                let floor = acquisition_threshold(cells, blocks, UNSOUND_ABOVE_P_FA);
                if !given.is_finite() || given < floor {
                    return Err(AcquireError::ThresholdUnsound {
                        given,
                        floor,
                        cells,
                        blocks,
                        sample_rate_hz: self.sample_rate_hz.round() as i64,
                    });
                }
                Ok(given)
            }
        }
    }
}

/// The peak-to-mean bar for a search of `cells` cells accumulated over `blocks`
/// non-coherent blocks, at a target per-satellite false-alarm probability `p_fa`.
///
/// **This has to be computed, not fixed, and getting it wrong is not subtle.** Each cell is
/// another chance for noise to clear a fixed bar, and the cell count is the sample rate's
/// business, not the caller's. Left at a fixed 2.5 over a 4000-cell profile, acquisition returns
/// **all 32 satellites out of pure noise**, which would make every dwell report an intact
/// constellation and quietly disable the jamming assessment that reads it.
///
/// The normalised profile cell is Gamma(`blocks`)/`blocks`, whose upper tail is **exact** for
/// integer `k = blocks`:
///
/// ```text
/// P(X > x) = e^{−k x} · Σ_{j=0}^{k−1} (k x)^j / j!
/// ```
///
/// and the bar is the `x` where `cells · P(X > x) = p_fa`. Solved by bisection — the tail is
/// monotone. T-322 used only the leading `j = k−1` term, which understates the tail; the whole
/// sum is barely more work and removes a known bias in the optimistic direction.
///
/// Prefer [`AcquisitionConfig::peak_to_mean_bar`], which supplies `cells` from the search's own
/// geometry. This form is public so the derivation can be tabulated and tested directly.
pub fn acquisition_threshold(cells: usize, blocks: usize, p_fa: f64) -> f32 {
    let k = blocks.max(1);
    let kf = k as f64;
    let n = cells.max(1) as f64;
    let target = (p_fa.clamp(1e-12, 0.5) / n).ln();
    // ln P(X > x), by log-sum-exp so large k·x does not overflow the polynomial.
    let ln_tail = |x: f64| {
        let ln_kx = (kf * x).ln();
        let mut ln_fact = 0.0f64;
        let mut max = f64::NEG_INFINITY;
        let terms: Vec<f64> = (0..k)
            .map(|j| {
                if j > 0 {
                    ln_fact += (j as f64).ln();
                }
                let t = j as f64 * ln_kx - ln_fact;
                max = max.max(t);
                t
            })
            .collect();
        let sum: f64 = terms.iter().map(|t| (t - max).exp()).sum();
        -kf * x + max + sum.ln()
    };
    let (mut lo, mut hi) = (0.0f64, 200.0f64);
    for _ in 0..80 {
        let mid = 0.5 * (lo + hi);
        if ln_tail(mid) > target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (0.5 * (lo + hi)) as f32
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
    /// The peak-to-mean bar actually applied, derived or validated. Recorded because the number
    /// is a function of the rate and grid, so "which bar was this?" is not answerable from the
    /// config alone.
    pub threshold_ratio: f32,
    /// Cells the maximum was taken over — code phases × Doppler bins — which is what set the bar.
    pub search_cells: usize,
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
    /// An explicit [`AcquisitionThreshold::PeakToMean`] bar that noise clears at will.
    ///
    /// **Refused rather than defaulted.** A default is a value someone can override without
    /// noticing; a refusal is a conversation. The failure this prevents is silent and *inverted*:
    /// a too-low bar acquires the whole constellation from noise, so the dwell reports an intact
    /// GNSS service and the jamming assessment reading it never fires.
    #[error(
        "peak-to-mean bar {given} is unsound for this search: at {sample_rate_hz} Hz the maximum \
         is taken over {cells} cells in {blocks} non-coherent block(s), where noise alone \
         clears it more often than not. The least defensible bar here is {floor:.2}. \
         A ratio is only meaningful against a cell count the sample rate decides — set \
         AcquisitionThreshold::FalseAlarm and let the bar be derived."
    )]
    ThresholdUnsound {
        /// The ratio the caller fixed.
        given: f32,
        /// The bar at a per-satellite false-alarm probability of 0.5 — the coin-flip line.
        floor: f32,
        /// Cells searched at this rate and Doppler grid: code phases × Doppler bins.
        cells: usize,
        /// Non-coherent blocks accumulated.
        blocks: usize,
        /// The rate that decided the cell count, rounded for display.
        sample_rate_hz: i64,
    },
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

    // The bar, before any correlating is done: derived from this search's own geometry, or — for
    // an explicitly fixed ratio — checked against it and refused if noise would clear it.
    let search_cells = cfg.search_cells().unwrap_or(samples_per_code);
    let threshold_ratio = cfg.peak_to_mean_bar()?;

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
        .filter(|s| s.peak_ratio >= threshold_ratio)
        .collect();
    acquired.sort_by(|a, b| b.peak_ratio.total_cmp(&a.peak_ratio));

    Ok(AcquisitionResult {
        acquired,
        searched,
        threshold_ratio,
        search_cells,
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

    /// **The defect this module's threshold type exists to make unreachable.**
    ///
    /// The cell count is the rate's business: 2046 at the minimum, 4000 at 4 Msps, 20 000 at
    /// 20 Msps. A bar of 2.5 is below the coin-flip line at *every* one of them, so it is not a
    /// detector anywhere — which is why it must not be a value anyone can hold.
    #[test]
    fn the_bar_rises_with_the_search_and_2_5_is_under_the_floor_at_every_rate() {
        let mut last = 0.0f32;
        for rate in [2_046_000.0, 4.0e6, 20.0e6] {
            let cfg = AcquisitionConfig {
                sample_rate_hz: rate,
                ..Default::default()
            };
            let cells = cfg.search_cells().expect("an acquirable rate");
            let bar = cfg
                .peak_to_mean_bar()
                .expect("the default bar is derivable");
            let floor = acquisition_threshold(cells, cfg.noncoherent_blocks, 0.5);
            eprintln!(
                "{:.3} Msps: {cells} cells, derived bar {bar:.2}, coin-flip floor {floor:.2}",
                rate / 1e6
            );
            assert!(
                bar > last,
                "the bar must rise with the rate: {last} -> {bar}"
            );
            assert!(
                floor > 2.5,
                "2.5 is above the coin-flip floor {floor} at {rate} Hz — then the old default \
                 was not the bug T-322 measured"
            );
            last = bar;
        }
        // Stricter false alarm, higher bar; more non-coherent averaging, lower bar.
        assert!(acquisition_threshold(4000, 4, 1e-6) > acquisition_threshold(4000, 4, 1e-3));
        assert!(acquisition_threshold(4000, 16, 1e-3) < acquisition_threshold(4000, 4, 1e-3));
    }

    /// The tail approximation must actually hit the false-alarm rate it claims. Draws Gamma(k)/k
    /// profiles from a deterministic generator and counts how often the maximum clears the bar.
    #[test]
    fn the_bar_delivers_about_the_false_alarm_rate_it_promises() {
        const CELLS: usize = 4000;
        const BLOCKS: usize = 4;
        let bar = f64::from(acquisition_threshold(CELLS, BLOCKS, 1e-3));
        let mut state = 0x243f_6a88_85a3_08d3u64;
        let mut unit = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            ((state >> 11) as f64 / (1u64 << 53) as f64).max(1e-15)
        };
        let mut fired = 0;
        const TRIALS: usize = 2000;
        for _ in 0..TRIALS {
            let mut peak = 0.0f64;
            for _ in 0..CELLS {
                // Gamma(k,1) as a sum of k exponentials, normalised to mean 1.
                let g: f64 = (0..BLOCKS).map(|_| -unit().ln()).sum();
                peak = peak.max(g / BLOCKS as f64);
            }
            if peak > bar {
                fired += 1;
            }
        }
        let rate = fired as f64 / TRIALS as f64;
        assert!(
            rate < 1e-2,
            "{fired}/{TRIALS} noise-only profiles cleared {bar}: {rate}"
        );
    }

    /// **The refusal.** A fixed ratio that noise clears at will is an error, not a setting — and
    /// the same number is fine at a geometry that earns it.
    #[test]
    fn an_unsound_fixed_bar_is_refused_rather_than_run() {
        let book = PrnCodebook::subset(&[1]).unwrap();
        let led = KnownCodeLed::with_codebook(&book);
        let cfg = AcquisitionConfig {
            sample_rate_hz: 4.0e6,
            threshold: AcquisitionThreshold::PeakToMean(2.5),
            ..Default::default()
        };
        let iq = vec![Complex32::default(); 4000 * 4];
        let err = acquire(&led, &book, &iq, &cfg).unwrap_err();
        let AcquireError::ThresholdUnsound {
            given,
            floor,
            cells,
            ..
        } = err
        else {
            panic!("expected a refusal, got {err:?}");
        };
        assert_eq!(given, 2.5);
        assert!(
            floor > given,
            "floor {floor} must exceed the refused {given}"
        );
        assert_eq!(
            cells,
            4000 * 41,
            "4000 code cells x the 41 Doppler bins searched"
        );
        // Printed so the message a caller would see is on the record.
        eprintln!("{}", acquire(&led, &book, &iq, &cfg).unwrap_err());

        // The same fixed form, at a bar the geometry supports, runs.
        let sound = AcquisitionConfig {
            threshold: AcquisitionThreshold::PeakToMean(12.0),
            ..cfg
        };
        assert!(acquire(&led, &book, &iq, &sound).is_ok());
    }

    /// The refusal is about the *search*, not about the number: a bar sound at one rate can be
    /// unsound at another, which is the whole reason a bare ratio is not portable.
    #[test]
    fn the_same_ratio_can_be_sound_at_one_rate_and_unsound_at_another() {
        let at = |rate: f64, r: f32| {
            AcquisitionConfig {
                sample_rate_hz: rate,
                threshold: AcquisitionThreshold::PeakToMean(r),
                ..Default::default()
            }
            .peak_to_mean_bar()
        };
        // Pick a ratio between the two rates' floors rather than hard-coding one, so the test
        // states the property and not a number that would drift with the derivation.
        let floor_of = |rate: f64| {
            let cfg = AcquisitionConfig {
                sample_rate_hz: rate,
                ..Default::default()
            };
            acquisition_threshold(cfg.search_cells().unwrap(), cfg.noncoherent_blocks, 0.5)
        };
        let (low, high) = (floor_of(2_046_000.0), floor_of(20.0e6));
        assert!(high > low, "a ten-times-larger search must cost more");
        let between = 0.5 * (low + high);
        eprintln!("coin-flip floors: 2.046 Msps {low:.2}, 20 Msps {high:.2}; trying {between:.2}");
        assert!(at(2_046_000.0, between).is_ok());
        assert!(
            at(20.0e6, between).is_err(),
            "{between} must not survive a ten-times-larger search"
        );
    }

    /// A false-alarm probability outside (0, 1) is a mistake, not a clamp.
    #[test]
    fn a_meaningless_false_alarm_rate_is_refused() {
        for p in [0.0, 1.0, -1.0, f64::NAN] {
            let cfg = AcquisitionConfig {
                threshold: AcquisitionThreshold::FalseAlarm(p),
                ..Default::default()
            };
            assert!(
                matches!(cfg.peak_to_mean_bar(), Err(AcquireError::Config(_))),
                "p_fa {p} should be refused"
            );
        }
    }

    /// Doppler is part of the search, so it is part of the bar: a wider grid is more chances for
    /// noise, and the derivation must know that without being told.
    #[test]
    fn a_wider_doppler_search_raises_the_bar() {
        let narrow = AcquisitionConfig {
            doppler_max_hz: 1_000.0,
            ..Default::default()
        };
        let wide = AcquisitionConfig {
            doppler_max_hz: 10_000.0,
            ..Default::default()
        };
        assert!(narrow.search_cells() < wide.search_cells());
        assert!(narrow.peak_to_mean_bar().unwrap() < wide.peak_to_mean_bar().unwrap());
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
