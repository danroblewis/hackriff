//! The C13 estimator: one [`ChannelSnippet`] → one [`ParameterSet`].
//!
//! Order of work (S5 §2 step 2, §5 T-010):
//!
//! 1. **Welch PSD of the box** (Hann, 50 % overlap, S5 FFT-length rule) restricted to the flat
//!    DDC passband.
//! 2. **N0.** Caller floor if given; else the mean density of the pads that pass a flatness
//!    test (a pad holding part of the burst shows its spectral shape) and agree with each
//!    other; else the in-channel sidebands outside the box. The **mean**, never the median.
//! 3. **Presence.** The integrated noise-subtracted power must be ≥ `presence_z` standard
//!    errors, or the 5-bin-smoothed peak must clear `√(2 ln M) + peak_z_margin`. Otherwise every
//!    signal estimate abstains with `low_snr` (noise-only snippets give no CFO).
//! 4. **OBW99** between the 0.5 % and 99.5 % cumulative points of the 5-bin-smoothed,
//!    noise-subtracted (negative bins zeroed) PSD; **x-dB** bandwidths on the same trace.
//! 5. **SNR** = unclipped in-band power / (N0·OBW99) over the box. Bias: see the crate docs.
//! 6. **CFO centroid**, then the **burst extent**: recentre, ±0.75·OBW channel filter, moving
//!    average of |y|² over `2·fs/OBW` samples, first→last above N0·ENBW + 6 dB **among the runs
//!    that hold a significant sample** (T-876, [`EstimatorConfig::extent_pfa`]); each edge then
//!    sits where the moving average crosses half-way to the burst's on-level, which is where the
//!    burst starts or ends (T-887, [`half_level_edges`]). **SNR over the extent** with the box's
//!    OBW99 band.
//! 7. **Hinted CFO** on the filtered extent: x² line (DSB/BPSK), x⁴ line (QPSK), FSK cluster
//!    mid-point. Power-of-M without a hint is only a shape feature.
//! 8. **Shape features**, flags, RF centre (optionally ppm-corrected).

use std::ops::Range;
use std::time::Instant;

use hk_model::EstimatedParams;
use num_complex::Complex32;
use serde::{Deserialize, Serialize};

use crate::ESTIMATOR_VERSION;
use crate::dsp::{
    ChannelFilter, HANN_MANY_VAR, HANN_SMOOTH5_VAR, LineHit, LineSearch, Psd, Workspace,
    choose_nfft, db, kmeans_1d, mix_into, power_line, prev_pow2, smooth,
};
use crate::estimate::{Estimate, Evidence, Method, Reason};
use crate::snippet::ChannelSnippet;

/// Modulation-family hint that unlocks family-specific CFO estimators.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "family", rename_all = "kebab-case")]
pub enum FamilyHint {
    /// No hint: spectral centroid only.
    #[default]
    Unknown,
    /// Double-sideband suppressed carrier, BPSK, biphase (RDS): x² line.
    Dsb,
    /// QPSK / 4-QAM: x⁴ line.
    Qpsk,
    /// M-level FSK: mid-point of the outer instantaneous-frequency clusters.
    Fsk {
        /// Number of frequency levels (≥ 2).
        levels: u32,
    },
}

/// A caller-supplied noise density (e.g. a C08 floor), in snippet units (FS²/Hz; the DDC keeps
/// the source density).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct NoiseReference {
    /// Density, linear FS²/Hz.
    pub density: f64,
    /// One-sigma uncertainty, dB.
    pub sigma_db: f64,
}

impl NoiseReference {
    /// From dBFS/Hz.
    pub fn from_dbfs_per_hz(dbfs_per_hz: f64, sigma_db: f64) -> Self {
        Self {
            density: 10f64.powf(dbfs_per_hz / 10.0),
            sigma_db,
        }
    }
}

/// Optional inputs from elsewhere in the chain.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Hints {
    /// Family hint (C14/C15 or a prior).
    pub family: FamilyHint,
    /// Noise floor to use instead of the pads.
    pub noise: Option<NoiseReference>,
    /// Receiver clock error (C05), ppm; enables [`ParameterSet::rf_center_corrected_hz`].
    pub clock_ppm: Option<f64>,
}

/// Estimator settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EstimatorConfig {
    /// Occupied-bandwidth power fraction β.
    pub obw_fraction: f64,
    /// x-dB bandwidth levels, dB.
    pub xdb_levels_db: Vec<f64>,
    /// Presence: integrated-power z-score threshold.
    pub presence_z: f64,
    /// Presence: margin over `√(2 ln M)` for the smoothed-peak z-score.
    pub peak_z_margin: f64,
    /// Clip fraction above which amplitudes are untrusted (docs/04 §10.4).
    pub clip_fraction_max: f64,
    /// Burst extent threshold over the filtered noise power, dB. Sets the extent's **edges**.
    pub extent_threshold_db: f64,
    /// False-alarm probability, over the whole snippet, of the test that decides which runs above
    /// `extent_threshold_db` belong to the burst (T-876). A run counts only if some sample of its
    /// moving average clears the level that noise alone reaches with this probability anywhere in
    /// the snippet (the `Gamma(K)` law of a `K`-effective-average window, Bonferroni over the
    /// snippet's windows), or if it lies within one window of a run that does. A 6 dB crossing is
    /// a ~10⁻⁴ event per window of noise, so a few-thousand-sample snippet carried one often
    /// enough that `first..last` ran milliseconds past the burst into noise.
    #[serde(default = "default_extent_pfa")]
    pub extent_pfa: f64,
    /// Channel filter passband as a fraction of OBW99 (±).
    pub channel_filter_obw: f64,
    /// Floor on spectral-line significance thresholds, dB.
    pub line_min_significance_db: f64,
    /// False-alarm probability for adaptive line thresholds.
    pub line_pfa: f64,
    /// A second line at or above this amplitude ratio makes a line ambiguous.
    pub line_unique_ratio: f64,
    /// Longest extent used by a hinted power-of-M CFO line, samples.
    pub cfo_line_max_samples: usize,
    /// Longest extent used by shape-feature line searches, samples.
    pub feature_line_max_samples: usize,
    /// Minimum Fisher ratio between the outer FSK clusters.
    pub fsk_min_fisher: f64,
    /// OBW at or above this fraction of the flat passband means the signal fills the snippet.
    pub fills_band_fraction: f64,
    /// Floor for bandwidths and centroid: the integrated in-band power must stand this many
    /// standard errors above the noise (including the N0 uncertainty). The OBW99 tails hold
    /// 0.5 % of the power each, so they are only resolved well above presence. 100 ≈ 8 dB
    /// in-band SNR for a 3 ms, 250 kHz-wide 915 MHz burst; long integrations reach it at lower
    /// SNR. Below the floor SNR is still reported and hinted line estimators still run.
    pub min_band_z: f64,
}

impl Default for EstimatorConfig {
    fn default() -> Self {
        Self {
            obw_fraction: 0.99,
            xdb_levels_db: vec![3.0, 6.0, 26.0],
            presence_z: 5.0,
            peak_z_margin: 3.0,
            clip_fraction_max: 1e-4,
            extent_threshold_db: 6.0,
            extent_pfa: default_extent_pfa(),
            channel_filter_obw: 0.75,
            line_min_significance_db: 12.0,
            line_pfa: 1e-3,
            line_unique_ratio: 0.6,
            cfo_line_max_samples: 1 << 20,
            feature_line_max_samples: 1 << 15,
            fsk_min_fisher: 2.5,
            fills_band_fraction: 0.9,
            min_band_z: 50.0,
        }
    }
}

fn default_extent_pfa() -> f64 {
    1e-3
}

/// One x-dB bandwidth.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct XdbBandwidth {
    /// x, dB.
    pub level_db: f64,
    /// Bandwidth, Hz.
    pub bandwidth_hz: Estimate,
}

/// Every CFO estimator's result (Hz, relative to the snippet centre).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CfoEstimates {
    /// Spectral centroid (always attempted).
    pub centroid: Estimate,
    /// x² line (with [`FamilyHint::Dsb`]).
    pub square_line: Estimate,
    /// x⁴ line (with [`FamilyHint::Qpsk`]).
    pub fourth_power_line: Estimate,
    /// FSK cluster mid-point (with [`FamilyHint::Fsk`]).
    pub fsk_midpoint: Estimate,
}

/// The burst extent inside the snippet.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Extent {
    /// First snippet sample.
    pub start: usize,
    /// One past the last snippet sample.
    pub end: usize,
    /// Source stream index of `start`.
    pub source_start: f64,
    /// Source stream index of `end`.
    pub source_end: f64,
    /// Signal already present at the snippet start (the burst began earlier).
    pub truncated_start: bool,
    /// Signal still present at the snippet end.
    pub truncated_end: bool,
}

/// Spectral-shape features for the classifier (C15). `None` when not computable.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShapeFeatures {
    /// Spectral flatness (geometric / arithmetic mean) of the noise-subtracted PSD within OBW99:
    /// ≈ 1 flat (noise-like, OFDM), → 0 peaky.
    pub flatness: Option<f64>,
    /// Roll-off `(B₋₂₆ − B₋₃) / B₋₃` (0 = brick wall).
    pub rolloff: Option<f64>,
    /// Symmetry `P = (P_L − P_U) / (P_L + P_U)` about the OBW99 mid-point (≈ ±1 for SSB-like).
    pub symmetry: Option<f64>,
    /// Peaks in the smoothed noise-subtracted PSD within 20 dB of the maximum and with ≥ 6 dB
    /// prominence.
    pub peak_count: Option<u32>,
    /// Strongest discrete line of the signal itself (carrier, pilot), offset from the snippet
    /// centre, Hz.
    pub carrier_line: Estimate,
    /// x² line significance, dB.
    pub square_line_db: Option<f64>,
    /// x² line coherence.
    pub square_coherence: Option<f64>,
    /// x⁴ line significance, dB.
    pub fourth_line_db: Option<f64>,
    /// x⁴ line coherence.
    pub fourth_coherence: Option<f64>,
}

/// Names of [`ShapeFeatures::vector`] entries.
pub const SHAPE_FEATURE_NAMES: [&str; 9] = [
    "flatness",
    "rolloff",
    "symmetry",
    "peak_count",
    "carrier_line_db",
    "square_line_db",
    "square_coherence",
    "fourth_line_db",
    "fourth_coherence",
];

impl ShapeFeatures {
    fn empty(reason: Reason) -> Self {
        Self {
            flatness: None,
            rolloff: None,
            symmetry: None,
            peak_count: None,
            carrier_line: Estimate::abstain(Method::CarrierLine, reason),
            square_line_db: None,
            square_coherence: None,
            fourth_line_db: None,
            fourth_coherence: None,
        }
    }

    /// The features as a fixed vector (`NaN` for missing), in [`SHAPE_FEATURE_NAMES`] order.
    pub fn vector(&self) -> [f32; 9] {
        let f = |v: Option<f64>| v.map_or(f32::NAN, |x| x as f32);
        [
            f(self.flatness),
            f(self.rolloff),
            f(self.symmetry),
            f(self.peak_count.map(f64::from)),
            f(self.carrier_line.evidence().significance_db),
            f(self.square_line_db),
            f(self.square_coherence),
            f(self.fourth_line_db),
            f(self.fourth_coherence),
        ]
    }
}

/// Conditions that qualify the estimates (C05 suspect flags pass through here).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EstimateFlags {
    /// Clip fraction above `clip_fraction_max`: SNR abstains.
    pub clipped: bool,
    /// Clipped fraction of the source samples over box + pads.
    pub clip_fraction: f64,
    /// The provenance says the front end was overloaded (IMD suspect).
    pub overload: bool,
    /// The box reaches the edge of the source band.
    pub edge: bool,
    /// Extracted across ±fs/2: absolute frequencies are known modulo fs.
    pub nyquist_wrapped: bool,
    /// The extraction guard band was narrowed.
    pub guard_clamped: bool,
    /// The box extends beyond the available samples.
    pub box_truncated: bool,
    /// Pads rejected as carrying signal or disagreeing.
    pub pads_rejected: u32,
    /// More than one separated emission in the passband.
    pub multi_signal: bool,
    /// The occupied band fills the snippet passband.
    pub fills_band: bool,
}

/// C13 output for one snippet (docs/07 §2.9 bandwidth/SNR fields; §2.14 `EstimatedParams`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParameterSet {
    /// Estimator id and version.
    pub version: String,
    /// Snippet sample rate, Hz.
    pub sample_rate_hz: f64,
    /// Box Welch FFT length (0 when too short).
    pub nfft: usize,
    /// Box Welch segments averaged.
    pub segments: u32,
    /// Noise density N0, linear FS²/Hz.
    pub noise_density: Estimate,
    /// 99 % occupied bandwidth, Hz.
    pub obw99_hz: Estimate,
    /// x-dB bandwidths.
    pub xdb_bandwidths: Vec<XdbBandwidth>,
    /// In-band SNR over the box, dB.
    pub snr_box_db: Estimate,
    /// In-band SNR over the burst extent, dB.
    pub snr_extent_db: Estimate,
    /// Selected CFO relative to the snippet centre, Hz: the hinted method when it measured,
    /// else the centroid (all attempts are in [`ParameterSet::cfo`]).
    pub cfo_hz: Estimate,
    /// All CFO estimators.
    pub cfo: CfoEstimates,
    /// RF centre = snippet centre + selected CFO, receiver frame, Hz.
    pub rf_center_hz: Estimate,
    /// RF centre corrected by the hinted clock ppm, Hz.
    pub rf_center_corrected_hz: Estimate,
    /// Burst extent.
    pub extent: Option<Extent>,
    /// Burst duration, s.
    pub duration_s: Estimate,
    /// Shape features.
    pub shape: ShapeFeatures,
    /// Conditions.
    pub flags: EstimateFlags,
    /// Estimation time, µs (excludes extraction).
    pub cost_us: u64,
}

impl ParameterSet {
    fn abstained(
        snip: &ChannelSnippet,
        levels: &[f64],
        flags: EstimateFlags,
        reason: Reason,
    ) -> Self {
        let a = |m| Estimate::abstain(m, reason);
        Self {
            version: ESTIMATOR_VERSION.into(),
            sample_rate_hz: snip.sample_rate_hz,
            nfft: 0,
            segments: 0,
            noise_density: a(Method::NoisePad),
            obw99_hz: a(Method::Obw99),
            xdb_bandwidths: levels
                .iter()
                .map(|&level_db| XdbBandwidth {
                    level_db,
                    bandwidth_hz: a(Method::XdbBandwidth),
                })
                .collect(),
            snr_box_db: a(Method::SnrBox),
            snr_extent_db: a(Method::SnrExtent),
            cfo_hz: a(Method::CfoCentroid),
            cfo: CfoEstimates {
                centroid: a(Method::CfoCentroid),
                square_line: a(Method::CfoSquareLine),
                fourth_power_line: a(Method::CfoFourthPowerLine),
                fsk_midpoint: a(Method::CfoFskMidpoint),
            },
            rf_center_hz: a(Method::RfCenter),
            rf_center_corrected_hz: a(Method::RfCenterCorrected),
            extent: None,
            duration_s: a(Method::Duration),
            shape: ShapeFeatures::empty(reason),
            flags,
            cost_us: 0,
        }
    }

    /// The data-model parameters this estimate supplies (docs/07 §2.14): CFO and bandwidth.
    pub fn estimated_params(&self) -> EstimatedParams {
        EstimatedParams {
            cfo_hz: self.cfo_hz.value(),
            bandwidth_hz: self.obw99_hz.value(),
            ..Default::default()
        }
    }

    /// The x-dB bandwidth at `level_db`, if configured.
    pub fn xdb(&self, level_db: f64) -> Option<&Estimate> {
        self.xdb_bandwidths
            .iter()
            .find(|x| (x.level_db - level_db).abs() < 1e-9)
            .map(|x| &x.bandwidth_hz)
    }
}

/// Mid-point CFO from FSK level estimates (the T-011 hook: symbol-centre cluster centres and
/// their standard errors, Hz relative to the snippet centre). Uses the outermost levels.
pub fn cfo_from_fsk_levels(levels_hz: &[f64], sigmas_hz: &[f64]) -> Estimate {
    let method = Method::CfoFskLevels;
    if levels_hz.len() < 2
        || levels_hz.len() != sigmas_hz.len()
        || levels_hz.iter().chain(sigmas_hz).any(|v| !v.is_finite())
    {
        return Estimate::abstain(method, Reason::InvalidInput);
    }
    let (mut lo, mut hi) = (0, 0);
    for (i, &v) in levels_hz.iter().enumerate() {
        if v < levels_hz[lo] {
            lo = i;
        }
        if v > levels_hz[hi] {
            hi = i;
        }
    }
    if lo == hi {
        return Estimate::abstain(method, Reason::NoClusters);
    }
    Estimate::measured(
        0.5 * (levels_hz[lo] + levels_hz[hi]),
        0.5 * (sigmas_hz[lo].powi(2) + sigmas_hz[hi].powi(2)).sqrt(),
        method,
    )
}

/// The extent's significance level over its edge threshold: a noise-only moving average of `win`
/// samples of noise filtered to `enbw_frac`·fs is `Gamma(K)` with `K` its effective number of
/// independent averages (sinc autocorrelation of a band-limited process), and the level is the one
/// it exceeds with probability `pfa` over the snippet's `n / win` windows. `rel`, the N0 estimate's
/// relative standard error, widens it by two of its sigmas: an N0 read low is exactly what lets
/// noise clear a fixed level.
fn extent_seed_factor(win: usize, enbw_frac: f64, n: usize, pfa: f64, rel: f64) -> f64 {
    let w = win as f64;
    let b = enbw_frac.clamp(1e-6, 1.0);
    let lag_sum: f64 = (1..win)
        .map(|l| {
            let a = std::f64::consts::PI * b * l as f64;
            2.0 * (w - l as f64) * (a.sin() / a).powi(2)
        })
        .sum();
    let k = (w * w / (w + lag_sum)).clamp(1.0, w);
    let tests = (n as f64 / w).max(1.0);
    let t = hk_dsp::floor::gamma::mean_threshold(k, (pfa / tests).clamp(1e-300, 0.5));
    t * (1.0 + 2.0 * rel.max(0.0))
}

/// First and last sample of the burst's moving average `ma` above `thr`, counting only the runs
/// above `thr` that reach `seed` somewhere (or that lie within one window of a run already
/// counted). Noise alone clears `thr` now and then in a long snippet; it essentially never clears
/// `seed`, so an isolated noise blip milliseconds from the burst no longer sets its end (T-876).
/// When no run reaches `seed` — a burst too weak for any window of it to be significant on its
/// own — every run counts, as before: there is nothing firmer to measure the edges from.
fn burst_bounds(
    n: usize,
    win: usize,
    thr: f64,
    seed: f64,
    ma: &impl Fn(usize) -> f64,
) -> (Option<usize>, Option<usize>) {
    // Runs above `thr`, each with whether it holds a seed.
    let mut runs: Vec<(usize, usize, bool)> = Vec::new();
    let mut open: Option<(usize, bool)> = None;
    for i in 0..n {
        let v = ma(i);
        match (&mut open, v > thr) {
            (None, true) => open = Some((i, v > seed)),
            (Some((_, seeded)), true) => *seeded |= v > seed,
            (Some((s, seeded)), false) => {
                runs.push((*s, i - 1, *seeded));
                open = None;
            }
            (None, false) => {}
        }
    }
    if let Some((s, seeded)) = open {
        runs.push((s, n - 1, seeded));
    }
    let seeded: Vec<usize> = (0..runs.len()).filter(|&r| runs[r].2).collect();
    let (Some(&r0), Some(&r1)) = (seeded.first(), seeded.last()) else {
        return (runs.first().map(|r| r.0), runs.last().map(|r| r.1));
    };
    // Bridge outward across gaps no longer than one window.
    let (mut a, mut b) = (r0, r1);
    while a > 0 && runs[a].0 - runs[a - 1].1 <= win {
        a -= 1;
    }
    while b + 1 < runs.len() && runs[b + 1].0 - runs[b].1 <= win {
        b += 1;
    }
    (Some(runs[a].0), Some(runs[b].1))
}

/// The burst's edges, `start..end` in snippet samples, from the run `first..=last` of the centred
/// moving average `ma` above the edge threshold (T-887).
///
/// **Each edge is where `ma` crosses half-way between the noise and the burst's on-level.** `ma(i)`
/// averages `win` samples centred on `i`, so across a step from noise `p_noise` to `p_noise + P`
/// at sample `B` it ramps linearly over `B − win/2 ..= B + win/2` and passes the half-way level
/// exactly at `B`, at any SNR: noise only adds jitter to the crossing, never bias. The on-level is
/// the median of `ma` over the run (robust to a ragged edge and, for a keyed emission, low rather
/// than high, which moves the edge outward — the safe side).
///
/// Until T-887 the edges were `first − win/2` and `last + win/2`. `first` is where `ma` clears
/// `p_noise` + 6 dB, and at any useful SNR that takes only a sliver of the window on the burst,
/// so `first` already sat about `win/2` *before* the burst — and a second `win/2` was then taken
/// off. Every extent ran about one full window into the noise on each side: measured on the
/// mock-SDR `fsk_burst_train` scene, −44 and +56 source samples at 500 kS/s on a 23.3 ms burst,
/// which the classifier's normalised snippet carried as ~4 noise-only samples at each end of
/// ~1200. An envelope sample at the noise floor is `|a/ā − 1| ≈ 1`, so those 8 samples alone
/// doubled `sigma_aa` (0.083 against 0.025 over the same snippet with its edges dropped), and
/// the classifier's `2fsk` density, fitted on edge-free dev-grid records, put every row at z ≈ +4.
/// The bias is a fixed number of samples, so it is the short bursts it costs most.
///
/// The half-level edges never widen the extent: they are clamped inside the old
/// `first − win/2 .. last + win/2 + 1`, which remains the bound at an SNR so low that the
/// on-level barely clears the threshold.
fn half_level_edges(
    n: usize,
    win: usize,
    first: usize,
    last: usize,
    p_noise: f64,
    ma: &impl Fn(usize) -> f64,
) -> (usize, usize) {
    let lo = first.saturating_sub(win / 2);
    let hi = (last + win / 2 + 1).min(n);
    let mut run: Vec<f64> = (first..=last).map(ma).collect();
    run.sort_by(f64::total_cmp);
    let on = run[run.len() / 2];
    if !(on.is_finite() && on > p_noise) {
        return (lo, hi);
    }
    let half = p_noise + 0.5 * (on - p_noise);
    let start = (lo..=last).find(|&i| ma(i) >= half).unwrap_or(lo);
    let end = (first..hi)
        .rev()
        .find(|&i| ma(i) >= half)
        .map_or(hi, |i| i + 1);
    if end <= start {
        return (lo, hi);
    }
    (start, end)
}

struct Noise {
    n0: f64,
    rel: f64,
    estimate: Estimate,
    rejected: u32,
}

struct PadStat {
    n0: f64,
    rel: f64,
    flat_z: f64,
    flat_thr: f64,
    samples: usize,
}

/// The C13 estimator. Caches Welch engines, FFT plans and scratch buffers across snippets.
#[derive(Default)]
pub struct ParamEstimator {
    config: EstimatorConfig,
    ws: Workspace,
    mixed: Vec<Complex32>,
    filtered: Vec<Complex32>,
}

impl ParamEstimator {
    /// An estimator with `config`.
    pub fn new(config: EstimatorConfig) -> Self {
        Self {
            config,
            ..Default::default()
        }
    }

    /// Settings.
    pub fn config(&self) -> &EstimatorConfig {
        &self.config
    }

    /// Estimates the parameters of `snip`. See the [module docs](self).
    pub fn estimate(&mut self, snip: &ChannelSnippet, hints: &Hints) -> ParameterSet {
        let started = Instant::now();
        let mut out = self.estimate_inner(snip, hints);
        out.cost_us = started.elapsed().as_micros() as u64;
        out
    }

    fn estimate_inner(&mut self, snip: &ChannelSnippet, hints: &Hints) -> ParameterSet {
        let cfg = self.config.clone();
        let fs = snip.sample_rate_hz;
        let x = &snip.samples;
        let flags = EstimateFlags {
            clipped: snip.clip_fraction() > cfg.clip_fraction_max,
            clip_fraction: snip.clip_fraction(),
            overload: snip.provenance.overload,
            edge: snip.flags.edge,
            nyquist_wrapped: snip.flags.nyquist_wrapped,
            guard_clamped: snip.flags.guard_clamped,
            box_truncated: snip.flags.box_truncated,
            ..Default::default()
        };
        let levels = cfg.xdb_levels_db.clone();
        let too_short = ParameterSet::abstained(snip, &levels, flags, Reason::TooShort);
        let (b0, b1) = (snip.box_range.start, snip.box_range.end.min(x.len()));
        if b1 <= b0 {
            return too_short;
        }
        let Some(nfft) = choose_nfft(b1 - b0) else {
            return too_short;
        };
        let psd = self.ws.welch(&x[b0..b1], fs, nfft);
        let fp = snip.passband_hz * 0.98;
        let pb = psd.bins(-fp, fp);
        if pb.len() < 8 {
            return too_short;
        }

        // 2. Noise.
        let Some(noise) = self.noise(snip, &psd, pb.clone(), hints) else {
            let mut out = ParameterSet::abstained(snip, &levels, flags, Reason::NoNoiseReference);
            out.nfft = nfft;
            out.segments = psd.segments;
            return out;
        };
        let mut flags = flags;
        flags.pads_rejected = noise.rejected;
        let (n0, rel) = (noise.n0, noise.rel);
        let k = psd.k_eff();
        let df = psd.df();
        let m = pb.len();
        let freqs: Vec<f64> = pb.clone().map(|i| psd.freq(i)).collect();
        let s: Vec<f64> = psd.p[pb.clone()].iter().map(|v| v - n0).collect();
        let pos: Vec<f64> = s.iter().map(|v| v.max(0.0)).collect();
        let mut ss = Vec::new();
        smooth(&pos, 5, &mut ss);
        let mut su = Vec::new();
        smooth(&s, 5, &mut su);

        // 3. Presence.
        let sigma5 = n0 * (HANN_SMOOTH5_VAR / k + rel * rel).sqrt();
        let peak_z = su.iter().copied().fold(f64::MIN, f64::max) / sigma5;
        let s_total = s.iter().sum::<f64>() * df;
        let sigma_total =
            n0 * df * (HANN_MANY_VAR * m as f64 / k + (m as f64 * rel).powi(2)).sqrt();
        let z_total = s_total / sigma_total;
        let peak_thr = (2.0 * (m as f64).ln()).sqrt() + cfg.peak_z_margin;
        let presence = Evidence {
            significance_db: Some(db(z_total.max(peak_z).max(1e-3))),
            bins: Some(m as u32),
            ..Default::default()
        };
        let base = |reason, flags| {
            let mut out = ParameterSet::abstained(snip, &levels, flags, reason);
            out.nfft = nfft;
            out.segments = psd.segments;
            out.noise_density = noise.estimate;
            out
        };
        if !(z_total >= cfg.presence_z || peak_z >= peak_thr) {
            let mut out = base(Reason::LowSnr, flags);
            out.snr_box_db = out.snr_box_db.with_evidence(presence);
            out.cfo.centroid = out.cfo.centroid.with_evidence(presence);
            return out;
        }
        let total: f64 = su.iter().sum();
        if total <= 0.0 {
            return base(Reason::LowSnr, flags);
        }

        // 4. OBW99 and x-dB. Deviation from S5: the cumulative runs over the *unclipped*
        // smoothed noise-subtracted PSD. S5 zeroed negative bins, which adds the positive half of
        // the noise to the tails and widens OBW99 with the snippet width and at low SNR (+38 %
        // at 10 dB in a 2.5× snippet); unclipped noise is zero-mean, and at high SNR both agree.
        let tail = (1.0 - cfg.obw_fraction) / 2.0;
        let (lo, hi) = cumulative_edges(&su, tail);
        let obw = (hi - lo + 1) as f64 * df;
        let (wlo, whi) = cumulative_edges(&su, tail / 2.0);
        let (nlo, nhi) = cumulative_edges(&su, tail * 2.0);
        let obw_sigma = df.max(0.25 * ((whi - wlo) as f64 - (nhi - nlo) as f64) * df);
        flags.fills_band = obw >= cfg.fills_band_fraction * 2.0 * fp;
        let touches = lo == 0 || hi == m - 1;
        let peak = ss.iter().copied().fold(0.0, f64::max);
        let noise_floor_ss = 3.0 * sigma5;
        flags.multi_signal = multi_signal(&ss, noise_floor_ss.max(peak * 1e-3));
        let mut out = base(Reason::Upstream, flags);
        let obw_ev = Evidence {
            bins: Some((hi - lo + 1) as u32),
            significance_db: Some(db(z_total.max(1e-3))),
            threshold_db: Some(db(cfg.min_band_z)),
            ..Default::default()
        };
        out.obw99_hz = if flags.fills_band {
            Estimate::abstain(Method::Obw99, Reason::FillsBand)
        } else if touches {
            Estimate::abstain(Method::Obw99, Reason::EdgeOfBand)
        } else {
            Estimate::measured(obw, obw_sigma, Method::Obw99)
        }
        .with_evidence(obw_ev);
        for x_db in &mut out.xdb_bandwidths {
            x_db.bandwidth_hz = xdb_bandwidth(&ss, df, x_db.level_db, noise_floor_ss, peak);
        }

        // 5. SNR over the box.
        let mb = (hi - lo + 1) as f64;
        let band_b = mb * df;
        let psig = s[lo..=hi].iter().sum::<f64>() * df;
        let sig_p = n0 * df * (HANN_MANY_VAR * mb / k + (mb * rel).powi(2)).sqrt();
        out.snr_box_db = snr_estimate(
            Method::SnrBox,
            psig,
            sig_p,
            n0,
            rel,
            band_b,
            flags,
            out.obw99_hz.reason(),
        )
        .with_evidence(Evidence {
            bins: Some(mb as u32),
            samples: Some((b1 - b0) as u64),
            ..Default::default()
        });

        // Floor: below `min_band_z` report the SNR and nothing wider.
        if z_total < cfg.min_band_z && out.obw99_hz.is_measured() {
            out.obw99_hz = Estimate::abstain(Method::Obw99, Reason::LowSnr).with_evidence(obw_ev);
            for x_db in &mut out.xdb_bandwidths {
                x_db.bandwidth_hz = Estimate::abstain(Method::XdbBandwidth, Reason::LowSnr);
            }
        }

        // 6. Centroid, with unclipped weights (zero-mean noise does not pull it to the centre).
        let wsum: f64 = su[lo..=hi].iter().sum();
        let fc = if wsum > 0.0 {
            su[lo..=hi]
                .iter()
                .zip(&freqs[lo..=hi])
                .map(|(w, f)| w * f)
                .sum::<f64>()
                / wsum
        } else {
            0.0
        };
        let var_fc = s[lo..=hi]
            .iter()
            .zip(&freqs[lo..=hi])
            .map(|(sv, f)| (f - fc).powi(2) * (sv + n0).powi(2) / k)
            .sum::<f64>()
            / (wsum * wsum).max(1e-300)
            + df * df / 12.0;
        out.cfo.centroid = match out.obw99_hz.reason() {
            Some(r) => Estimate::abstain(Method::CfoCentroid, r),
            None if wsum <= 0.0 => Estimate::abstain(Method::CfoCentroid, Reason::LowSnr),
            None => Estimate::measured(fc, var_fc.sqrt(), Method::CfoCentroid),
        }
        .with_evidence(obw_ev);

        // Shape features from the PSD.
        let sym_mid = 0.5 * (freqs[lo] + freqs[hi]);
        let (mut pl, mut pu) = (0.0, 0.0);
        for (w, f) in ss[lo..=hi].iter().zip(&freqs[lo..=hi]) {
            if *f < sym_mid {
                pl += w;
            } else if *f > sym_mid {
                pu += w;
            }
        }
        let measured_band = out.obw99_hz.is_measured();
        if measured_band {
            let floor = 0.01 * n0;
            let vals = s[lo..=hi].iter().map(|v| v.max(floor));
            let (mut ln_sum, mut sum) = (0.0, 0.0);
            for v in vals {
                ln_sum += v.ln();
                sum += v;
            }
            out.shape.flatness = Some((ln_sum / mb).exp() / (sum / mb));
            out.shape.symmetry = (pl + pu > 0.0).then(|| (pl - pu) / (pl + pu));
            out.shape.peak_count = Some(peak_count(&ss[lo..=hi], noise_floor_ss));
            if let (Some(b3), Some(b26)) = (
                out.xdb(3.0).and_then(Estimate::value),
                out.xdb(26.0).and_then(Estimate::value),
            ) {
                out.shape.rolloff = Some((b26 - b3) / b3);
            }
        }

        // Without a measured OBW99 (below the floor, too wide, at the snippet edge: e.g. a
        // subcarrier next to other MPX content) the extent and the line estimators still run,
        // on the box width around the snippet centre; their own significance tests gate them.
        let (band_hz, fc) = if measured_band {
            (obw, fc)
        } else {
            (snip.box_bandwidth_hz.clamp(8.0 * df, 1.8 * fp), 0.0)
        };

        // 6b. Extent: recentre, channel filter, envelope threshold.
        mix_into(x, fc, fs, &mut self.mixed);
        let filter = ChannelFilter::new(fs, cfg.channel_filter_obw * band_hz);
        let enbw = match &filter {
            Some(f) => {
                f.apply(&self.mixed, &mut self.filtered);
                f.enbw_hz
            }
            None => {
                self.filtered.clear();
                self.filtered.extend_from_slice(&self.mixed);
                fs
            }
        };
        let y = &self.filtered;
        let n = y.len();
        let win = ((2.0 * fs / band_hz).round() as usize).clamp(8, n.max(8));
        let thr = n0 * enbw * 10f64.powf(cfg.extent_threshold_db / 10.0);
        let mut prefix = Vec::with_capacity(n + 1);
        prefix.push(0.0f64);
        let mut acc = 0.0;
        for v in y {
            acc += f64::from(v.norm_sqr());
            prefix.push(acc);
        }
        let ma = |i: usize| {
            let a = i.saturating_sub(win / 2);
            let b = (a + win).min(n);
            (prefix[b] - prefix[a]) / (b - a).max(1) as f64
        };
        let (first, last) = burst_bounds(
            n,
            win,
            thr,
            thr * extent_seed_factor(win, enbw / fs, n, cfg.extent_pfa, rel),
            &ma,
        );
        let extent = match (first, last) {
            (Some(f), Some(l)) if l >= f => {
                let (e0, e1) = half_level_edges(n, win, f, l, n0 * enbw, &ma);
                Some(Extent {
                    start: e0,
                    end: e1,
                    source_start: snip.source_index_of(e0),
                    source_end: snip.source_index_of(e1),
                    truncated_start: f <= win / 2,
                    truncated_end: l + win / 2 + 1 >= n,
                })
            }
            _ => None,
        };
        out.extent = extent;
        let Some(ext) = extent else {
            out.snr_extent_db = Estimate::abstain(Method::SnrExtent, Reason::LowSnr);
            out.duration_s = Estimate::abstain(Method::Duration, Reason::LowSnr);
            self.finish_cfo(&mut out, snip, hints, fc, &[]);
            return out;
        };
        out.duration_s = Estimate::measured(
            (ext.end - ext.start) as f64 / fs,
            win as f64 / fs / 2.0,
            Method::Duration,
        )
        .with_evidence(Evidence {
            samples: Some((ext.end - ext.start) as u64),
            ..Default::default()
        });

        // SNR over the extent, in the box's OBW99 band.
        out.snr_extent_db = match choose_nfft(ext.end - ext.start) {
            _ if !measured_band => Estimate::abstain(Method::SnrExtent, Reason::Upstream),
            None => Estimate::abstain(Method::SnrExtent, Reason::TooShort),
            Some(nfe) => {
                let pe = self.ws.welch(&x[ext.start..ext.end], fs, nfe);
                let band = pe.bins(freqs[lo] - df / 2.0, freqs[hi] + df / 2.0);
                if band.len() < 3 {
                    Estimate::abstain(Method::SnrExtent, Reason::TooShort)
                } else {
                    let me = band.len() as f64;
                    let dfe = pe.df();
                    let ke = pe.k_eff();
                    let ps = pe.p[band].iter().map(|v| v - n0).sum::<f64>() * dfe;
                    let sp = n0 * dfe * (HANN_MANY_VAR * me / ke + (me * rel).powi(2)).sqrt();
                    snr_estimate(Method::SnrExtent, ps, sp, n0, rel, me * dfe, flags, None)
                        .with_evidence(Evidence {
                            bins: Some(me as u32),
                            samples: Some((ext.end - ext.start) as u64),
                            ..Default::default()
                        })
                }
            }
        };

        // 7–8. Lines, hinted CFO, carrier line.
        let seg: Vec<Complex32> = self.filtered[ext.start..ext.end].to_vec();
        let on: Vec<bool> = (ext.start..ext.end).map(|i| ma(i) > thr).collect();
        self.lines_and_cfo(&mut out, snip, hints, fc, band_hz, &seg, &on);
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn lines_and_cfo(
        &mut self,
        out: &mut ParameterSet,
        snip: &ChannelSnippet,
        hints: &Hints,
        fc: f64,
        obw: f64,
        seg: &[Complex32],
        on: &[bool],
    ) {
        let cfg = &self.config;
        let fs = snip.sample_rate_hz;
        let search = |power: u32, hinted: bool, half_width: f64| LineSearch {
            power,
            f_lo: -(f64::from(power) * half_width).min(0.49 * fs),
            f_hi: (f64::from(power) * half_width).min(0.49 * fs),
            min_significance_db: cfg.line_min_significance_db,
            pfa: cfg.line_pfa,
            max_samples: if hinted {
                cfg.cfo_line_max_samples
            } else {
                cfg.feature_line_max_samples
            },
        };
        let want_sq = hints.family == FamilyHint::Dsb;
        let want_q4 = hints.family == FamilyHint::Qpsk;
        let unique = cfg.line_unique_ratio;
        let s2 = search(2, want_sq, 0.5 * obw);
        let s4 = search(4, want_q4, 0.5 * obw);
        let s1 = search(1, false, 0.5 * obw);
        let sq = power_line(&mut self.ws, seg, fs, s2);
        let q4 = power_line(&mut self.ws, seg, fs, s4);
        let c1 = power_line(&mut self.ws, seg, fs, s1);
        out.shape.square_line_db = sq.map(|h| h.significance_db);
        out.shape.square_coherence = sq.map(|h| h.coherence);
        out.shape.fourth_line_db = q4.map(|h| h.significance_db);
        out.shape.fourth_coherence = q4.map(|h| h.coherence);
        out.cfo.square_line = if want_sq {
            line_estimate(sq, 2, fc, Method::CfoSquareLine, unique)
        } else {
            not_requested(Method::CfoSquareLine, sq)
        };
        out.cfo.fourth_power_line = if want_q4 {
            line_estimate(q4, 4, fc, Method::CfoFourthPowerLine, unique)
        } else {
            not_requested(Method::CfoFourthPowerLine, q4)
        };
        // A carrier line need not be unique (AM sidebands, pilots): no uniqueness test.
        out.shape.carrier_line = line_estimate(c1, 1, fc, Method::CarrierLine, f64::INFINITY);
        out.cfo.fsk_midpoint = match hints.family {
            FamilyHint::Fsk { levels } => {
                fsk_midpoint(seg, on, fs, obw, fc, levels.max(2), cfg.fsk_min_fisher)
            }
            _ => Estimate::abstain(Method::CfoFskMidpoint, Reason::NotRequested),
        };
        self.finish_cfo(out, snip, hints, fc, seg);
    }

    fn finish_cfo(
        &self,
        out: &mut ParameterSet,
        snip: &ChannelSnippet,
        hints: &Hints,
        _fc: f64,
        _seg: &[Complex32],
    ) {
        let hinted = match hints.family {
            FamilyHint::Unknown => out.cfo.centroid,
            FamilyHint::Dsb => out.cfo.square_line,
            FamilyHint::Qpsk => out.cfo.fourth_power_line,
            FamilyHint::Fsk { .. } => out.cfo.fsk_midpoint,
        };
        out.cfo_hz = if hinted.is_measured() || !out.cfo.centroid.is_measured() {
            hinted
        } else {
            out.cfo.centroid
        };
        match out.cfo_hz {
            Estimate::Measured { value, sigma, .. } => {
                let rf = snip.rf_frequency_hz(value);
                out.rf_center_hz = Estimate::measured(rf, sigma, Method::RfCenter);
                out.rf_center_corrected_hz = match hints.clock_ppm {
                    Some(ppm) => Estimate::measured(
                        crate::clock::correct_frequency_hz(rf, ppm),
                        sigma,
                        Method::RfCenterCorrected,
                    ),
                    None => Estimate::abstain(Method::RfCenterCorrected, Reason::NoCalibration),
                };
            }
            Estimate::Abstained { .. } => {
                out.rf_center_hz = Estimate::abstain(Method::RfCenter, Reason::Upstream);
                out.rf_center_corrected_hz =
                    Estimate::abstain(Method::RfCenterCorrected, Reason::Upstream);
            }
        }
    }

    fn noise(
        &mut self,
        snip: &ChannelSnippet,
        psd: &Psd,
        pb: Range<usize>,
        hints: &Hints,
    ) -> Option<Noise> {
        let x = &snip.samples;
        let fs = snip.sample_rate_hz;
        let fp = snip.passband_hz * 0.98;
        let (b0, b1) = (snip.box_range.start, snip.box_range.end.min(x.len()));
        let mut stats: Vec<PadStat> = [&x[..b0], &x[b1..]]
            .into_iter()
            .filter_map(|pad| pad_stat(&mut self.ws, pad, fs, fp, psd.nfft))
            .collect();
        let before = stats.len();
        stats.retain(|p| p.flat_z <= p.flat_thr);
        if stats.len() == 2 {
            let (a, b) = (&stats[0], &stats[1]);
            let tol = 4.0 * (a.rel * a.rel + b.rel * b.rel).sqrt() + 0.05;
            if (a.n0 / b.n0).ln().abs() > tol {
                let drop = if a.n0 > b.n0 { 0 } else { 1 };
                stats.remove(drop);
            }
        }
        let rejected = (before - stats.len()) as u32;

        if let Some(r) = hints.noise {
            if r.density.is_finite() && r.density > 0.0 {
                let rel = (r.sigma_db.abs() / 4.343).max(1e-4);
                return Some(Noise {
                    n0: r.density,
                    rel,
                    estimate: Estimate::measured(r.density, r.density * rel, Method::NoiseCaller),
                    rejected,
                });
            }
        }
        if !stats.is_empty() {
            let wsum: f64 = stats.iter().map(|p| 1.0 / (p.rel * p.rel)).sum();
            let n0 = stats.iter().map(|p| p.n0 / (p.rel * p.rel)).sum::<f64>() / wsum;
            let rel = (1.0 / wsum).sqrt();
            let samples: usize = stats.iter().map(|p| p.samples).sum();
            return Some(Noise {
                n0,
                rel,
                estimate: Estimate::measured(n0, n0 * rel, Method::NoisePad).with_evidence(
                    Evidence {
                        samples: Some(samples as u64),
                        ..Default::default()
                    },
                ),
                rejected,
            });
        }
        // Sidebands: passband bins outside the box (10 % guard).
        let half_box = 0.55 * snip.box_bandwidth_hz;
        let k = psd.k_eff();
        let side = |range: Range<usize>| -> Option<(f64, f64, usize)> {
            let bins: Vec<f64> = range.map(|i| psd.p[i]).collect();
            (bins.len() >= 8).then(|| {
                let mean = bins.iter().sum::<f64>() / bins.len() as f64;
                (
                    mean,
                    (HANN_MANY_VAR / (bins.len() as f64 * k)).sqrt(),
                    bins.len(),
                )
            })
        };
        let lower = side(pb.start..psd.bins(-fp, -half_box).end.min(pb.end));
        let upper = side(psd.bins(half_box, fp).start.max(pb.start)..pb.end);
        let (n0, rel, bins) = match (lower, upper) {
            (Some(a), Some(b)) => {
                if (a.0 / b.0).ln().abs() <= 4.0 * (a.1 * a.1 + b.1 * b.1).sqrt() + 0.05 {
                    let wa = 1.0 / (a.1 * a.1);
                    let wb = 1.0 / (b.1 * b.1);
                    (
                        (a.0 * wa + b.0 * wb) / (wa + wb),
                        (1.0 / (wa + wb)).sqrt(),
                        a.2 + b.2,
                    )
                } else if a.0 < b.0 {
                    a
                } else {
                    b
                }
            }
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => return None,
        };
        Some(Noise {
            n0,
            rel,
            estimate: Estimate::measured(n0, n0 * rel, Method::NoiseSideband).with_evidence(
                Evidence {
                    bins: Some(bins as u32),
                    ..Default::default()
                },
            ),
            rejected,
        })
    }
}

fn pad_stat(
    ws: &mut Workspace,
    pad: &[Complex32],
    fs: f64,
    fp: f64,
    nfft_box: usize,
) -> Option<PadStat> {
    if pad.len() < 128 {
        return None;
    }
    let n = nfft_box.min(prev_pow2(pad.len() / 2));
    if n < 64 {
        return None;
    }
    let psd = ws.welch(pad, fs, n);
    let pb = psd.bins(-fp, fp);
    let m = pb.len();
    if m < 8 {
        return None;
    }
    let p = &psd.p[pb];
    let mean = p.iter().sum::<f64>() / m as f64;
    if mean <= 0.0 {
        return None;
    }
    let k = psd.k_eff();
    let w = (m / 16).max(5);
    let mut sm = Vec::new();
    smooth(p, w, &mut sm);
    let max = sm.iter().copied().fold(0.0, f64::max);
    let sigma_w = (HANN_MANY_VAR / (w as f64 * k)).sqrt();
    Some(PadStat {
        n0: mean,
        rel: (HANN_MANY_VAR / (m as f64 * k)).sqrt(),
        flat_z: (max / mean - 1.0) / sigma_w,
        flat_thr: (2.0 * (m as f64).ln()).sqrt() + 3.0,
        samples: pad.len(),
    })
}

/// Indices of the `tail` and `1 − tail` cumulative points.
fn cumulative_edges(v: &[f64], tail: f64) -> (usize, usize) {
    let total: f64 = v.iter().sum();
    let mut acc = 0.0;
    let mut lo = 0;
    for (i, &x) in v.iter().enumerate() {
        acc += x;
        if acc >= tail * total {
            lo = i;
            break;
        }
    }
    let mut acc = 0.0;
    let mut hi = v.len() - 1;
    for (i, &x) in v.iter().enumerate().rev() {
        acc += x;
        if acc >= tail * total {
            hi = i;
            break;
        }
    }
    (lo, hi.max(lo))
}

#[allow(clippy::too_many_arguments)]
fn snr_estimate(
    method: Method,
    psig: f64,
    sigma_psig: f64,
    n0: f64,
    rel: f64,
    band_hz: f64,
    flags: EstimateFlags,
    band_reason: Option<Reason>,
) -> Estimate {
    if flags.clipped {
        return Estimate::abstain(method, Reason::Clipped);
    }
    if let Some(r) = band_reason {
        return Estimate::abstain(method, r);
    }
    if psig.partial_cmp(&(2.0 * sigma_psig)) != Some(std::cmp::Ordering::Greater) {
        return Estimate::abstain(method, Reason::LowSnr);
    }
    let noise = n0 * band_hz;
    let rel_s = ((sigma_psig / psig).powi(2) + (rel * (1.0 + noise / psig)).powi(2)).sqrt();
    Estimate::measured(
        db(psig / noise),
        10.0 / std::f64::consts::LN_10 * rel_s,
        method,
    )
}

fn xdb_bandwidth(ss: &[f64], df: f64, level_db: f64, noise_floor: f64, peak: f64) -> Estimate {
    let method = Method::XdbBandwidth;
    let level = peak * 10f64.powf(-level_db / 10.0);
    if level <= noise_floor {
        return Estimate::abstain(method, Reason::LowSnr);
    }
    let lo = ss.iter().position(|&v| v >= level);
    let hi = ss.iter().rposition(|&v| v >= level);
    match (lo, hi) {
        (Some(l), Some(h)) if l > 0 && h + 1 < ss.len() => {
            Estimate::measured((h - l + 1) as f64 * df, df, method).with_evidence(Evidence {
                bins: Some((h - l + 1) as u32),
                ..Default::default()
            })
        }
        (Some(_), Some(_)) => Estimate::abstain(method, Reason::FillsBand),
        _ => Estimate::abstain(method, Reason::LowSnr),
    }
}

fn line_estimate(
    hit: Option<LineHit>,
    power: u32,
    fc: f64,
    method: Method,
    unique: f64,
) -> Estimate {
    let Some(h) = hit else {
        return Estimate::abstain(method, Reason::TooShort);
    };
    let evidence = line_evidence(&h);
    let p = f64::from(power);
    if !h.significant() {
        Estimate::abstain(method, Reason::NoLine)
    } else if h.second_ratio >= unique {
        Estimate::abstain(method, Reason::AmbiguousLine)
    } else {
        Estimate::measured(fc + h.freq_hz / p, h.sigma_hz / p, method)
    }
    .with_evidence(evidence)
}

fn not_requested(method: Method, hit: Option<LineHit>) -> Estimate {
    let e = Estimate::abstain(method, Reason::NotRequested);
    match hit {
        Some(h) => e.with_evidence(line_evidence(&h)),
        None => e,
    }
}

fn line_evidence(h: &LineHit) -> Evidence {
    Evidence {
        significance_db: Some(h.significance_db),
        threshold_db: Some(h.threshold_db),
        coherence: Some(h.coherence),
        second_line_ratio: Some(h.second_ratio),
        samples: Some(h.samples as u64),
        ..Default::default()
    }
}

/// FSK centre: smoothed instantaneous frequency of the recentred, filtered extent where the
/// envelope is on; k-means into `levels` clusters gives a coarse mid-point and the Fisher gate.
///
/// **2-FSK: settled levels.** Mid-crossings (with hysteresis) mark the transitions; the unit
/// interval T is the mean of the short crossing intervals; only samples ≥ T from every
/// crossing (inside runs whose neighbouring bits share the decision, S5's deviation reference
/// without symbol timing) are averaged per level, and the centre is the mid-point of the two
/// settled levels (two passes). Cluster means, modes and the histogram's mirror point all move
/// with the bit imbalance because transition samples weigh differently in each level (−660,
/// −140 and −340 Hz on the GFSK h = 4 synthetic); settled samples do not.
///
/// **Otherwise** (M > 2, or too few settled samples): the mirror-symmetry point of the IF
/// histogram (cross-correlation of the part above the split with the mirrored part below),
/// with σ from four time blocks.
fn fsk_midpoint(
    seg: &[Complex32],
    on: &[bool],
    fs: f64,
    obw: f64,
    fc: f64,
    levels: u32,
    min_fisher: f64,
) -> Estimate {
    let method = Method::CfoFskMidpoint;
    if seg.len() < 65 {
        return Estimate::abstain(method, Reason::TooShort);
    }
    let scale = fs / std::f64::consts::TAU;
    let fi: Vec<f64> = seg
        .windows(2)
        .map(|w| f64::from((w[1] * w[0].conj()).arg()) * scale)
        .collect();
    let l = ((fs / (2.0 * obw)).round() as usize).max(1);
    let mut fis = Vec::new();
    smooth(&fi, l, &mut fis);
    let values: Vec<f64> = fis
        .iter()
        .zip(on.iter().skip(1))
        .filter(|&(_, &o)| o)
        .map(|(&v, _)| v)
        .collect();
    if values.len() < 64 {
        return Estimate::abstain(method, Reason::TooShort);
    }
    let clusters = kmeans_1d(&values, levels as usize, 50);
    let k = clusters.len();
    let (c_lo, v_lo, n_lo) = clusters[0];
    let (c_hi, v_hi, n_hi) = clusters[k - 1];
    let fisher = (c_hi - c_lo).powi(2) / (v_lo + v_hi).max(1e-30);
    let evidence = Evidence {
        significance_db: Some(db(fisher.max(1e-6))),
        threshold_db: Some(db(min_fisher)),
        samples: Some(values.len() as u64),
        ..Default::default()
    };
    if fisher < min_fisher || n_lo < 16 || n_hi < 16 {
        return Estimate::abstain(method, Reason::NoClusters).with_evidence(evidence);
    }
    let coarse = 0.5 * (c_lo + c_hi);
    let corr = (l as f64).max(fs / obw);
    if k == 2 {
        if let Some((mid, sigma, n)) = settled_midpoint(&fis, on, coarse, 0.5 * (c_hi - c_lo), corr)
        {
            return Estimate::measured(fc + mid, sigma, method).with_evidence(Evidence {
                samples: Some(n as u64),
                ..evidence
            });
        }
    }
    let mut sorted = values.clone();
    sorted.sort_by(f64::total_cmp);
    let q = |p: f64| sorted[(p * (sorted.len() - 1) as f64).round() as usize];
    let (lo, hi) = (q(0.005), q(0.995));
    let span = 0.3 * (c_hi - c_lo);
    let Some(centre) = mirror_centre(&values, lo, hi, coarse, span) else {
        return Estimate::abstain(method, Reason::NoClusters).with_evidence(evidence);
    };
    let blocks = 4;
    let len = values.len() / blocks;
    let parts: Vec<f64> = (0..blocks)
        .filter_map(|j| mirror_centre(&values[j * len..(j + 1) * len], lo, hi, centre, span))
        .collect();
    let bin = (hi - lo) / MIRROR_BINS as f64;
    let sigma = if parts.len() >= 3 {
        let m = parts.iter().sum::<f64>() / parts.len() as f64;
        let var = parts.iter().map(|p| (p - m).powi(2)).sum::<f64>() / (parts.len() - 1) as f64;
        (var / parts.len() as f64).sqrt()
    } else {
        span
    };
    Estimate::measured(fc + centre, sigma.max(bin / 4.0), method).with_evidence(evidence)
}

/// Settled-level 2-FSK mid-point (see [`fsk_midpoint`]). `fis[i]` is the IF between samples
/// `i` and `i + 1`, so its envelope flag is `on[i + 1]`. Returns `(mid, σ, samples used)`.
fn settled_midpoint(
    fis: &[f64],
    on: &[bool],
    mid0: f64,
    half_sep: f64,
    corr: f64,
) -> Option<(f64, f64, usize)> {
    let n = fis.len().min(on.len().saturating_sub(1));
    let mut mid = mid0;
    let mut result = None;
    let mut crossings = Vec::new();
    for _ in 0..2 {
        let h = 0.3 * half_sep;
        crossings.clear();
        crossings.push(0usize);
        let mut state = 0i8;
        let mut last_cross = 0usize;
        for i in 0..n {
            if !on[i + 1] {
                if state != 0 {
                    crossings.push(i);
                    state = 0;
                }
                continue;
            }
            if i > 0 && (fis[i - 1] - mid) * (fis[i] - mid) <= 0.0 {
                last_cross = i;
            }
            let s = if fis[i] > mid + h {
                1
            } else if fis[i] < mid - h {
                -1
            } else {
                0
            };
            if s != 0 && s != state {
                if state != 0 {
                    crossings.push(last_cross.max(*crossings.last().unwrap_or(&0)));
                }
                state = s;
            }
        }
        crossings.push(n);
        let mut intervals: Vec<usize> = crossings
            .windows(2)
            .map(|w| w[1] - w[0])
            .filter(|&d| d > 0)
            .collect();
        if intervals.len() < 6 {
            return result;
        }
        intervals.sort_unstable();
        let q20 = intervals[intervals.len() / 5] as f64;
        let short: Vec<f64> = intervals
            .iter()
            .map(|&d| d as f64)
            .filter(|&d| d <= 1.5 * q20)
            .collect();
        let t = short.iter().sum::<f64>() / short.len() as f64;
        if t < 2.0 {
            return result;
        }
        let (mut s_hi, mut q_hi, mut n_hi) = (0.0, 0.0, 0usize);
        let (mut s_lo, mut q_lo, mut n_lo) = (0.0, 0.0, 0usize);
        let mut k = 0;
        for (i, &v) in fis.iter().enumerate().take(n) {
            while k + 1 < crossings.len() && crossings[k + 1] <= i {
                k += 1;
            }
            let prev = crossings[k];
            let next = crossings.get(k + 1).copied().unwrap_or(n);
            if !on[i + 1] || ((i - prev) as f64) < t || ((next - i) as f64) < t {
                continue;
            }
            if v > mid {
                s_hi += v;
                q_hi += v * v;
                n_hi += 1;
            } else {
                s_lo += v;
                q_lo += v * v;
                n_lo += 1;
            }
        }
        if n_hi < 8 || n_lo < 8 {
            return result;
        }
        let m_hi = s_hi / n_hi as f64;
        let m_lo = s_lo / n_lo as f64;
        let v_hi = (q_hi / n_hi as f64 - m_hi * m_hi).max(0.0);
        let v_lo = (q_lo / n_lo as f64 - m_lo * m_lo).max(0.0);
        let sigma = 0.5 * (v_hi * corr / n_hi as f64 + v_lo * corr / n_lo as f64).sqrt();
        mid = 0.5 * (m_hi + m_lo);
        result = Some((mid, sigma, n_hi + n_lo));
    }
    result
}

const MIRROR_BINS: usize = 256;

/// Mirror-symmetry centre of `values` in `[lo, hi]`, searched within `coarse ± span`: two
/// passes, each splitting the histogram at the current estimate and maximising
/// `Σ U(c + x)·L(c − x)` over a half-bin grid, refined parabolically.
fn mirror_centre(values: &[f64], lo: f64, hi: f64, coarse: f64, span: f64) -> Option<f64> {
    let w = (hi - lo) / MIRROR_BINS as f64;
    if !(w > 0.0 && span > 0.0) {
        return None;
    }
    let mut h = [0.0f64; MIRROR_BINS];
    let mut n = 0;
    for &v in values {
        if v >= lo && v <= hi {
            h[(((v - lo) / w) as usize).min(MIRROR_BINS - 1)] += 1.0;
            n += 1;
        }
    }
    if n < 32 {
        return None;
    }
    let half = w / 2.0;
    let mut centre = coarse;
    for _ in 0..2 {
        // Bin i has centre lo + (i + 0.5)·w; bins i and j mirror about lo + (i + j + 1)·w/2,
        // the half-bin grid point p = i + j + 1.
        let split = (((centre - lo) / w).round().max(0.0) as usize).min(MIRROR_BINS);
        let score = |p: usize| -> f64 {
            let mut s = 0.0;
            for (i, &hu) in h.iter().enumerate().skip(split) {
                if i + 1 > p {
                    break;
                }
                let j = p - 1 - i;
                if j < split && hu > 0.0 {
                    s += hu * h[j];
                }
            }
            s
        };
        let p_lo = (((centre - span - lo) / half).floor().max(1.0)) as usize;
        let p_hi = (((centre + span - lo) / half).ceil() as usize).min(2 * MIRROR_BINS - 1);
        if p_hi <= p_lo + 1 {
            return None;
        }
        let scores: Vec<f64> = (p_lo..=p_hi).map(score).collect();
        let (k, &best) = scores
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))?;
        if best <= 0.0 {
            return None;
        }
        let d = if k > 0 && k + 1 < scores.len() {
            let (a, b, c) = (scores[k - 1], scores[k], scores[k + 1]);
            let den = a - 2.0 * b + c;
            if den < 0.0 {
                (0.5 * (a - c) / den).clamp(-0.5, 0.5)
            } else {
                0.0
            }
        } else {
            0.0
        };
        centre = lo + ((p_lo + k) as f64 + d) * half;
    }
    Some(centre)
}

/// Two or more runs above `thr` (gaps < 3 bins bridged), each with ≥ 5 % of the power.
fn multi_signal(ss: &[f64], thr: f64) -> bool {
    let total: f64 = ss.iter().filter(|&&v| v >= thr).sum();
    if total <= 0.0 {
        return false;
    }
    let mut runs = 0;
    let mut acc = 0.0;
    let mut gap = usize::MAX;
    for &v in ss {
        if v >= thr {
            if gap >= 3 && acc > 0.0 {
                if acc >= 0.05 * total {
                    runs += 1;
                }
                acc = 0.0;
            }
            acc += v;
            gap = 0;
        } else {
            gap = gap.saturating_add(1);
        }
    }
    if acc >= 0.05 * total {
        runs += 1;
    }
    runs >= 2
}

/// Peaks within 20 dB of the maximum, above `floor`, with ≥ 6 dB prominence.
fn peak_count(ss: &[f64], floor: f64) -> u32 {
    let peak = ss.iter().copied().fold(0.0, f64::max);
    let min_height = (peak * 0.01).max(floor);
    let prominence = 10f64.powf(0.6);
    let n = ss.len();
    let mut count = 0;
    for i in 0..n {
        let v = ss[i];
        if v < min_height || (i > 0 && ss[i - 1] >= v) || (i + 1 < n && ss[i + 1] > v) {
            continue;
        }
        let side_min = |range: &mut dyn Iterator<Item = usize>| {
            let mut lowest = v;
            for j in range {
                if ss[j] > v {
                    break;
                }
                lowest = lowest.min(ss[j]);
            }
            lowest
        };
        let left = side_min(&mut (0..i).rev());
        let right = side_min(&mut (i + 1..n));
        let base = left.max(right).max(floor);
        if v >= base * prominence {
            count += 1;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fsk_levels_hook() {
        let e = cfo_from_fsk_levels(&[-9_000.0, 11_000.0], &[10.0, 10.0]);
        assert_eq!(e.value(), Some(1_000.0));
        assert!((e.sigma().unwrap() - 7.071).abs() < 0.01);
        assert_eq!(
            cfo_from_fsk_levels(&[1.0], &[1.0]).reason(),
            Some(Reason::InvalidInput)
        );
    }

    #[test]
    fn peaks_and_runs() {
        let mut v = vec![0.0; 100];
        for (i, x) in v.iter_mut().enumerate() {
            *x = (-((i as f64 - 30.0) / 4.0).powi(2)).exp()
                + (-((i as f64 - 70.0) / 4.0).powi(2)).exp();
        }
        assert_eq!(peak_count(&v, 1e-3), 2);
        assert!(multi_signal(&v, 1e-3));
        let single: Vec<f64> = (0..100)
            .map(|i| (-((i as f64 - 50.0) / 10.0).powi(2)).exp())
            .collect();
        assert_eq!(peak_count(&single, 1e-3), 1);
        assert!(!multi_signal(&single, 1e-3));
        assert!(cumulative_edges(&single, 0.005).0 < 50);
    }

    /// T-876: an isolated excursion over the edge threshold far from the burst no longer sets
    /// its end; one within a window of it is the burst's own ragged edge and still does.
    #[test]
    fn burst_bounds_ignore_isolated_noise_runs() {
        let (n, win, thr, seed) = (1000usize, 10usize, 1.0, 3.0);
        let mut v = vec![0.5; n];
        v[200..400].iter_mut().for_each(|x| *x = 10.0); // the burst
        v[405..408].iter_mut().for_each(|x| *x = 1.5); // ragged tail, within a window
        v[700..703].iter_mut().for_each(|x| *x = 1.5); // noise excursion, 300 samples later
        v[50..52].iter_mut().for_each(|x| *x = 2.0); // and one well before
        let ma = |i: usize| v[i];
        assert_eq!(burst_bounds(n, win, thr, seed, &ma), (Some(200), Some(407)));
        // The pre-T-876 rule (every run counts) ran from the early excursion to the late one.
        assert_eq!(burst_bounds(n, win, thr, thr, &ma), (Some(50), Some(702)));
        // A burst with no significant sample keeps that rule: nothing firmer to measure from.
        assert_eq!(burst_bounds(n, win, thr, 100.0, &ma), (Some(50), Some(702)));
        assert_eq!(burst_bounds(n, win, 20.0, 30.0, &ma), (None, None));
    }

    /// T-887: a centred moving average crosses half-way to the on-level exactly at a step, so the
    /// edges land on the burst at any SNR — not a window outside it, as `first − win/2` did.
    #[test]
    fn half_level_edges_land_on_the_burst_not_a_window_outside_it() {
        let (n, win) = (1000usize, 20usize);
        let (b, e) = (300usize, 700usize);
        for (p_noise, p_on) in [(1.0, 4.5), (1.0, 100.0), (1.0, 1e4)] {
            let x: Vec<f64> = (0..n)
                .map(|i| if (b..e).contains(&i) { p_noise + p_on } else { p_noise })
                .collect();
            let ma = |i: usize| {
                let a = i.saturating_sub(win / 2);
                let z = (a + win).min(n);
                x[a..z].iter().sum::<f64>() / (z - a) as f64
            };
            let thr = 4.0 * p_noise;
            let (first, last) = burst_bounds(n, win, thr, thr, &ma);
            let (f, l) = (first.unwrap(), last.unwrap());
            let (s, t) = half_level_edges(n, win, f, l, p_noise, &ma);
            assert!(s.abs_diff(b) <= 1 && t.abs_diff(e) <= 1, "{p_on}: {s}..{t}");
            // The rule it replaced ran a window into the noise at every useful SNR.
            if p_on >= 100.0 {
                assert!(b - f.saturating_sub(win / 2) >= win - 1, "{p_on}: first {f}");
            }
        }
        // Never wider than the old bounds, whatever the on-level.
        let ma = |i: usize| if (400..420).contains(&i) { 5.0 } else { 1.0 };
        let (s, t) = half_level_edges(n, win, 400, 419, 1.0, &ma);
        assert!(s >= 400 - win / 2 && t <= 419 + win / 2 + 1);
    }

    #[test]
    fn extent_seed_factor_grows_with_the_snippet_and_the_n0_error() {
        let f = |n, rel| extent_seed_factor(64, 0.2, n, 1e-3, rel);
        assert!(f(10_000, 0.0) > 1.0);
        assert!(f(100_000, 0.0) > f(10_000, 0.0));
        assert!(f(10_000, 0.05) > f(10_000, 0.0));
    }
}
