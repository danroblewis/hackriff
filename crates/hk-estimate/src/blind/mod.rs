//! C14 blind symbol estimation (T-011): symbol rate with trust, harmonic alternatives and
//! evidence; a coarse family label (C15 family scores) or `unknown` with reasons; FSK deviation.
//!
//! A port of the spike S5 prototype `classify_and_estimate`
//! (`spikes/s5-blind-estimation/REPORT.md` §2 steps 3–7, §4, §5 "T-011"). Input is a
//! [`NormalisedSnippet`] (recentred at the C13 CFO, flat to ±0.75·OBW99, cut to the burst
//! extent) or any [`BlindInput`] window.
//!
//! 1. **Envelope.** RMS envelope over `fs/OBW` samples; 2-means levels, contrast and level
//!    changes. A high-contrast input with few level changes is a multi-frame box: analyse the
//!    longest on-segment (S5 pitfall 9).
//! 2. **Four cyclic lines in two independent groups**, each from a locally whitened
//!    periodogram ([`lines`]): envelope group |x|² and |d env|²; phase group delay-multiply
//!    `x[n]x*[n−D]` (D = round(fs/(2·OBW))) and |d IF|² (IF smoothed over fs/(2·OBW)).
//!    Agreement counts only across groups: |x_c²| ≡ |x|² is not a second method (pitfall 1).
//!    A line counts at ≥ 12 dB; line-only trust needs ≥ 14 dB from both groups. Search
//!    `[OBW/50, min(1.2·OBW, fs/2.5)]` — **pinned by the signal and the receiver, never by the
//!    record** (T-327), as is the whitening block width.
//! 3. **Family scores** in [0, 1]: OOK (two envelope levels, low level < 2.5 σ of the noise,
//!    contrast ≥ 8 dB, ≥ 12 changes); FSK (centre IF of the top candidates bimodal: J ≥ 2.5
//!    saturating at 4.5, valley < 0.6, occupancy > 10 %, periodicity < 0.95, pitfall 8); BPSK
//!    (x² line unique: second x² line < 0.6 of the first, pitfall 7); QPSK (x⁴ line). A bare
//!    carrier is `unknown`. A label needs SNR_ext ≥ 8 dB and confidence ≥ 0.5.
//! 4. **Rate consensus** ([`consensus`]) over candidates {f/4, f/3, f/2, f, 2f} and run-length
//!    seeds, with the guarded transition least squares ([`transitions`]) for FSK/OOK-like
//!    signals; LS-confirmed candidates outrank lines (pitfall 6).
//! 5. **Rate trust** = consensus trust, digital structure (a family score ≥ 0.5 or an ok fit),
//!    not a bare carrier, and SNR_ext ≥ 0 dB. Family trust is separate (pitfall 10).
//! 6. **Deviation** (FSK label + trusted LS rate): IF averaged over ±T/4 at the LS symbol
//!    centres of each segment, symbols whose neighbours share the decision, median |v − mid|.
//!    The settled level means feed [`crate::params::cfo_from_fsk_levels`].
//!
//! **Constants against T-010's OBW99.** S5 zeroed negative PSD bins, so its OBW99 read 10–55 %
//! wide on the 915 MHz bursts (+38 % at 10 dB); T-010's unclipped OBW99 is narrower. On the
//! sweep signals (test `blind_constants_against_t010_obw`) T-010 OBW99 / clean OBW99 is
//! 0.87–1.08 for FSK, GFSK and PSK at 12–30 dB and 0.59–1.0 for NRZ OOK (its sinc² tails vanish
//! into the noise). The OBW multipliers are kept because what each constant is for still holds
//! with the narrower OBW: the rate bound 1.2·OBW stays ≥ 1.25 Rs (GFSK h = 0.5, PSK α = 0.35)
//! and ≥ 3.3 Rs (OOK); D = round(fs/(2·OBW)) stays within 0.11–0.48 T, and a narrower OBW only
//! moves D towards the T/2 optimum. **One constant is re-derived:** S5's "±0.75·OBW" channel
//! filter is a `firwin` Hamming design whose −6 dB point is 0.75·OBW, applied with `filtfilt`
//! (passband ≈ 0.6·OBW, stopband ≈ 0.9·OBW). The Kaiser filter here puts its mid-transition,
//! not its passband edge, at 0.75·OBW, and is applied to every input: a flat ±0.75·OBW passband
//! doubled the IF noise and cost GFSK h = 0.5 its transition fits at 20 dB.
//!
//! **Below the C13 bandwidth floor** (long weak signals: the RDS subcarrier at 2 dB, a long BPSK
//! burst at 3 dB) OBW99 abstains and [`normalise`] cannot run. [`BlindEstimator::prepare`] then
//! recentres the snippet itself and scales the constants with the detection-box width
//! ([`BlindReason::BandwidthFromBox`]); the trust rules are unchanged. On RDS the trusted windows
//! do not change for box widths of 3.5–5 kHz.
//!
//! **Cost** (release, dev Mac): 2.9 ms mean per synthetic burst, 5.3 ms mean / 16 ms max per
//! 915 MHz burst at 1.25 Msps, 4 ms per 0.25 s RDS window. Each result reports its own time.

pub mod consensus;
pub mod family;
pub(crate) mod lines;
pub mod receiver;
pub mod transitions;
pub(crate) mod util;

use std::time::Instant;

use hk_core::ProvenanceHandle;
use hk_model::{EstimatedParams, Provenance};
use num_complex::{Complex, Complex32};
use serde::{Deserialize, Serialize};

pub use consensus::{LineSupport, RateCandidate};
pub use family::FskCentreStats;
pub use receiver::{CaptureState, ReceiverLine, ReceiverLines, SurveyConfig};
pub use transitions::{FitFailure, LsGuards, TransitionFit};

use crate::dsp::{ChannelFilter, mix_into};
use crate::estimate::{Estimate, Evidence, Method, Reason};
use crate::normalise::{NormaliseConfig, NormalisedSnippet, normalise};
use crate::params::ParameterSet;
use crate::snippet::{ChannelSnippet, EstimateError};
use consensus::{Consensus, Fitter, Thresholds};
use family::{longest_run, median5, symbol_centre_stats};
use lines::{Plans, carrier_line, complex_line, spectral_line};
use transitions::{rate_transitions_ls, runlength_unit};
use util::{inst_freq, kmeans2, mean, median, moving_avg, quantile, std};

/// Estimator id and version recorded in every [`SymbolParameters`].
pub const BLIND_ESTIMATOR_VERSION: &str = "hk-estimate/c14@0.1.0";

/// Feature series a cyclic line comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LineMethod {
    /// |x|² (envelope group).
    EnvelopeSquare = 0,
    /// |d envelope|² (envelope group).
    EnvelopeDiff = 1,
    /// Delay-multiply `x[n]·x*[n−D]` (phase group).
    DelayMultiply = 2,
    /// |d instantaneous frequency|² (phase group).
    IfDiff = 3,
}

impl LineMethod {
    /// All four, in evidence order.
    pub const ALL: [LineMethod; 4] = [
        LineMethod::EnvelopeSquare,
        LineMethod::EnvelopeDiff,
        LineMethod::DelayMultiply,
        LineMethod::IfDiff,
    ];

    /// The independent method group. |x_c²| would be [`LineGroup::Envelope`] too (it *is*
    /// |x|²), which is why it is not a method (S5 pitfall 1).
    pub fn group(self) -> LineGroup {
        match self {
            LineMethod::EnvelopeSquare | LineMethod::EnvelopeDiff => LineGroup::Envelope,
            LineMethod::DelayMultiply | LineMethod::IfDiff => LineGroup::Phase,
        }
    }
}

/// Independent evidence groups.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LineGroup {
    /// Envelope features.
    Envelope = 0,
    /// Phase / frequency features.
    Phase = 1,
}

/// The strongest whitened line of one feature series.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CyclicLine {
    /// Feature series.
    pub method: LineMethod,
    /// Its group.
    pub group: LineGroup,
    /// Line frequency, Hz (`None`: series too short or range too small).
    pub freq_hz: Option<f64>,
    /// Whitened significance, dB.
    pub significance_db: f64,
    /// One-sigma frequency uncertainty, Hz.
    pub sigma_hz: Option<f64>,
    /// This line's whitening block had to be widened past the pinned
    /// [`BlindConfig::whiten_block_obw`] because the record held too few independent cells, so
    /// [`CyclicLine::significance_db`] was **not** measured at C14's pinned geometry (T-327).
    #[serde(default)]
    pub whiten_clamped: bool,
    /// A capture artefact outscored this line and was excluded (T-373): without the exclusion
    /// [`SymbolParameters::excluded_cyclic_hz`] would have been reported here instead. The
    /// artefact belongs to the receiver, not the emission, so this line is the honest answer —
    /// but a genuine emission whose rate sits on a comb member is suppressed the same way, and
    /// this flag is how that case is visible rather than silent.
    #[serde(default)]
    pub artefact_suppressed: bool,
}

/// Coarse modulation family (C15 family scores).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Family {
    /// No label: see [`SymbolParameters::reasons`].
    #[default]
    Unknown,
    /// On-off keying.
    Ook,
    /// Frequency-shift keying (2-level, incl. GFSK/MSK-like).
    Fsk,
    /// Binary PSK (and biphase / DSB-like).
    Bpsk,
    /// Quadrature PSK.
    Qpsk,
}

/// Why something is unknown, untrusted or qualified.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlindReason {
    /// SNR_ext below the family floor (or the rate floor, see the rate estimate).
    LowSnr,
    /// No SNR was supplied: nothing can be trusted or labelled.
    NoSnr,
    /// Too few samples.
    TooShort,
    /// Invalid input (rate, OBW).
    InvalidInput,
    /// The box holds several frames; the longest on-segment was analysed.
    MultiSegment,
    /// No cyclic line or ok transition fit.
    NoCyclicLine,
    /// Candidates exist but the rate trust rule failed.
    WeakRateConsensus,
    /// Lines agree but nothing says the signal is digital (no family score ≥ 0.5, no fit).
    NoDigitalStructure,
    /// A bare carrier.
    UnmodulatedCarrier,
    /// Family scores are too close or too low.
    AmbiguousFamily,
    /// C13 gave no OBW99 (below its bandwidth floor): the detection-box bandwidth bounds the
    /// search and scales the filters instead. The trust rules are unchanged.
    BandwidthFromBox,
    /// A cyclic line contributed by the **capture chain** outscored every line kept, and was
    /// excluded (T-373). What is reported is the runner-up. See
    /// [`SymbolParameters::excluded_cyclic_hz`].
    CaptureArtefact,
}

/// Where the bandwidth scaling C14's constants came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObwSource {
    /// C13 OBW99.
    #[default]
    Obw99,
    /// The detection box width (C13 abstained on OBW99).
    DetectionBox,
}

/// Family scores in [0, 1].
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FamilyScores {
    /// OOK.
    pub ook: f64,
    /// FSK.
    pub fsk: f64,
    /// BPSK.
    pub bpsk: f64,
    /// QPSK.
    pub qpsk: f64,
}

impl FamilyScores {
    fn ranked(&self) -> [(Family, f64); 4] {
        let mut v = [
            (Family::Ook, self.ook),
            (Family::Fsk, self.fsk),
            (Family::Bpsk, self.bpsk),
            (Family::Qpsk, self.qpsk),
        ];
        // Stable: ties keep OOK, FSK, BPSK, QPSK order (Python `max` over the dict).
        v.sort_by(|a, b| b.1.total_cmp(&a.1));
        v
    }
}

/// Features behind the family scores.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FamilyFeatures {
    /// Envelope level contrast, dB.
    pub contrast_db: f64,
    /// Low-level share of the envelope.
    pub low_fraction: f64,
    /// Envelope level changes (after a 5-sample majority filter).
    pub envelope_changes: usize,
    /// Envelope coefficient of variation over the on-samples.
    pub envelope_cv: f64,
    /// Low envelope level over the noise RMS.
    pub low_level_noise_sigmas: f64,
    /// Best FSK symbol-centre statistics.
    pub fsk: Option<FskCentreStats>,
    /// x line coherence.
    pub c1: f64,
    /// x² line coherence.
    pub c2: f64,
    /// x² second line / first.
    pub x2_second_line: f64,
    /// x⁴ line coherence.
    pub c4: f64,
}

/// Settings. Defaults are the S5 constants (see the [module docs](self) for their
/// re-derivation against T-010's OBW99).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BlindConfig {
    /// Channel filter ±(this × OBW99) applied when the input is wider.
    pub channel_filter_obw: f64,
    /// Line significance that counts, dB.
    pub line_count_db: f64,
    /// Line significance for line-only trust (both groups), dB.
    pub line_trust_db: f64,
    /// Lower rate bound as a fraction of OBW99 — **the whole lower edge** since T-327.
    ///
    /// It used to be `max(rate_min_cells·fs/n, obw·rate_min_obw)`, so the band searched was a
    /// function of how long the record was: on the dev grid the floor moved 212 → 27 Hz (`am`),
    /// 377 → 129 (`nbfm`), 1965 → 246 (most keyed classes) between an eighth of the window and
    /// all of it, and `rate_range_hz` — a reported field — moved with it. The resolution term that
    /// caused that is real but belongs to the **transform**: it is now `DC_GUARD_NATIVE_BINS`
    /// inside [`lines::spectral_line`], where it limits which bins can be *reported* without
    /// moving the band, the candidate set, or the reported range.
    ///
    /// The surviving bound is physical. For every modulation C14 covers the occupied bandwidth is
    /// within a small factor of the symbol rate (docs/04 §4.5; the module docs above measure
    /// OBW99/Rs at 1.25 for GFSK h = 0.5 and PSK α = 0.35, up to 3.3 for NRZ OOK), so 1/50 leaves
    /// better than 15× margin past the widest ratio in the taxonomy. Raise it and a genuinely slow
    /// emission inside a wide detection box goes unseen — C14 reports no line rather than its
    /// rate; lower it and the search runs down into the feature series' own near-DC continuum
    /// (AGC, fading, envelope drift), which is where the periodogram is least white, and buys
    /// false lines there.
    pub rate_min_obw: f64,
    /// Whitening block width as a fraction of OBW99 (T-327).
    ///
    /// Pinned to the signal, never to the record: see [`lines`] for the two bounds that meet in
    /// it and for what happens when a record is too short to hold the statistical minimum.
    pub whiten_block_obw: f64,
    /// Upper rate bound as a multiple of OBW99.
    pub rate_max_obw: f64,
    /// Upper rate bound as a fraction of fs.
    pub rate_max_fs: f64,
    /// Overrides the lower rate bound, Hz.
    pub rate_min_hz: Option<f64>,
    /// Overrides `rate_max_obw·OBW` (the fs bound still applies), Hz.
    pub rate_max_hz: Option<f64>,
    /// Transition-fit guards.
    pub ls: LsGuards,
    /// Direct lines needed without an ok fit on FSK/OOK-like signals.
    pub lines_without_fit: usize,
    /// SNR_ext floor for a family label, dB.
    pub family_snr_db: f64,
    /// SNR_ext floor for a trusted rate, dB.
    pub rate_snr_db: f64,
    /// Minimum family confidence.
    pub family_min_confidence: f64,
    /// Candidates reported.
    pub top_k: usize,
    /// Samples per OBW99 for [`BlindConfig::normalise_config`] (and the snippet rate to request).
    pub samples_per_obw: f64,
    /// Minimum input samples.
    pub min_samples: usize,
    /// Native periodogram bins excluded either side of each capture-artefact comb member (T-373),
    /// or **0 to exclude nothing**.
    ///
    /// Default [`lines::ARTEFACT_GUARD_NATIVE_BINS`]. It is a width in *bins*, not in Hz, because
    /// the thing being removed is one line of this transform: see the constant for why 4 is the
    /// narrowest width that actually removes it. Zero is the control setting — it makes C14 read
    /// the capture's own comb as the emission's structure again, which is what the T-373
    /// regression test uses to prove it has teeth.
    pub artefact_guard_bins: f64,
}

impl Default for BlindConfig {
    fn default() -> Self {
        Self {
            channel_filter_obw: 0.75,
            line_count_db: 12.0,
            line_trust_db: 14.0,
            rate_min_obw: 1.0 / 50.0,
            whiten_block_obw: 1.0 / 8.0,
            rate_max_obw: 1.2,
            rate_max_fs: 1.0 / 2.5,
            rate_min_hz: None,
            rate_max_hz: None,
            ls: LsGuards::default(),
            lines_without_fit: 3,
            family_snr_db: 8.0,
            rate_snr_db: 0.0,
            family_min_confidence: 0.5,
            top_k: 3,
            samples_per_obw: 6.0,
            min_samples: 64,
            artefact_guard_bins: lines::ARTEFACT_GUARD_NATIVE_BINS,
        }
    }
}

impl BlindConfig {
    /// The normalisation C14 wants: flat ±0.75·OBW99 (the S5 channel filter), `samples_per_obw`
    /// per OBW99, extent only. The rate is capped at the snippet rate: extract with
    /// [`SnippetConfig::min_rate_hz`](crate::SnippetConfig::min_rate_hz) ≥
    /// `samples_per_obw × box bandwidth`.
    pub fn normalise_config(&self) -> NormaliseConfig {
        NormaliseConfig {
            samples_per_obw: self.samples_per_obw,
            bandwidth_obw: 2.0 * self.channel_filter_obw,
            extent_only: true,
            stopband_db: 60.0,
        }
    }
}

/// One analysis window.
#[derive(Clone, Copy, Debug)]
pub struct BlindInput<'a> {
    /// Samples, recentred (CFO removed).
    pub samples: &'a [Complex32],
    /// Sample rate, Hz.
    pub sample_rate_hz: f64,
    /// C13 OBW99 (or the box width, see `obw_source`), Hz.
    pub obw_hz: f64,
    /// Where `obw_hz` came from.
    pub obw_source: ObwSource,
    /// C13 in-band SNR over the extent, dB (`None`: nothing is trusted or labelled).
    pub snr_ext_db: Option<f64>,
    /// Noise power per sample in the channel, in sample units (`None`: 10th percentile of the
    /// smoothed power, which is biased high on continuous signals).
    pub noise_power: Option<f64>,
    /// Flat channel bandwidth of the samples, Hz; wider than 2.1 × 0.75·OBW99 gets filtered.
    pub channel_bandwidth_hz: f64,
    /// Offset of the sample centre from the snippet centre, Hz (added to FSK levels).
    pub center_offset_hz: f64,
    /// Provenance of the capture these samples came from, when known (T-373).
    ///
    /// C14 reads exactly one thing from it: the periodic artefacts the capture chain stamps into
    /// the stream ([`hk_model::Provenance::cyclic_artefacts`]), whose comb it must not report as
    /// the emission's own structure. `None` (a hand-built window, a synthetic series) excludes
    /// nothing, which is the honest default — an unrecorded artefact is not an excluded one.
    pub capture: Option<&'a Provenance>,
}

impl<'a> BlindInput<'a> {
    /// A normalised snippet's samples with its C13 values: OBW99 and extent SNR (box SNR if
    /// the extent abstained), noise `N0·scale²·channel bandwidth`. `None` without OBW99.
    pub fn from_normalised(n: &'a NormalisedSnippet) -> Option<Self> {
        let obw = n.params.obw99_hz.value()?;
        let snr = n
            .params
            .snr_extent_db
            .value()
            .or_else(|| n.params.snr_box_db.value());
        let noise = n
            .params
            .noise_density
            .value()
            .map(|n0| n0 * n.power_scale * n.power_scale * n.channel_bandwidth_hz);
        Some(Self {
            samples: &n.samples,
            sample_rate_hz: n.sample_rate_hz,
            obw_hz: obw,
            obw_source: ObwSource::Obw99,
            snr_ext_db: snr,
            noise_power: noise,
            channel_bandwidth_hz: n.channel_bandwidth_hz,
            center_offset_hz: n.cfo_applied_hz,
            capture: Some(n.provenance.get()),
        })
    }

    /// The same window restricted to `range` samples.
    pub fn window(&self, range: std::ops::Range<usize>) -> Self {
        Self {
            samples: &self.samples[range],
            ..*self
        }
    }
}

/// An owned C14 input prepared from a snippet (see [`BlindEstimator::prepare`]).
#[derive(Clone, Debug)]
pub struct BlindWindow {
    /// Samples, recentred.
    pub samples: Vec<Complex32>,
    /// Sample rate, Hz.
    pub sample_rate_hz: f64,
    /// Bandwidth scaling the constants, Hz.
    pub obw_hz: f64,
    /// Where it came from.
    pub obw_source: ObwSource,
    /// SNR over the extent (box SNR when the extent SNR abstained), dB.
    pub snr_ext_db: Option<f64>,
    /// Noise power per sample in the channel.
    pub noise_power: Option<f64>,
    /// Channel bandwidth of the samples, Hz.
    pub channel_bandwidth_hz: f64,
    /// Offset of the sample centre from the snippet centre, Hz.
    pub center_offset_hz: f64,
    /// Source stream index of sample 0.
    pub source_index: f64,
    /// Source samples per window sample.
    pub source_per_sample: f64,
    /// Provenance of the capture the snippet came from (T-373): see [`BlindInput::capture`].
    pub capture: Option<ProvenanceHandle>,
}

impl BlindWindow {
    /// The window as a [`BlindInput`] (use [`BlindInput::window`] for sub-windows).
    pub fn input(&self) -> BlindInput<'_> {
        BlindInput {
            samples: &self.samples,
            sample_rate_hz: self.sample_rate_hz,
            obw_hz: self.obw_hz,
            obw_source: self.obw_source,
            snr_ext_db: self.snr_ext_db,
            noise_power: self.noise_power,
            channel_bandwidth_hz: self.channel_bandwidth_hz,
            center_offset_hz: self.center_offset_hz,
            capture: self.capture.as_ref().map(ProvenanceHandle::get),
        }
    }
}

/// Rate trust and its gates.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RateTrust {
    /// Final: the rate may be used as authoritative.
    pub trusted: bool,
    /// The consensus trust rule passed.
    pub consensus: bool,
    /// Digital structure (family score ≥ 0.5 or an ok fit).
    pub digital_structure: bool,
    /// Independent groups with a direct line on the winner.
    pub groups: Vec<LineGroup>,
    /// Groups with a direct line ≥ the trust threshold.
    pub strong_groups: Vec<LineGroup>,
}

/// C14 output for one window (C14 `SymbolParameters`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SymbolParameters {
    /// Estimator id and version.
    pub version: String,
    /// Sample rate, Hz.
    pub sample_rate_hz: f64,
    /// Samples analysed (after multi-segment selection).
    pub samples: usize,
    /// SNR_ext used for the gates, dB.
    pub snr_ext_db: Option<f64>,
    /// Symbol rate, Bd: measured **only when trusted**; otherwise abstained with
    /// `untrusted` / `no_line` / `low_snr` / `too_short` and the best untrusted candidate is in
    /// [`SymbolParameters::candidates`].
    pub symbol_rate_bd: Estimate,
    /// Trust gates.
    pub rate_trust: RateTrust,
    /// Ranked candidates (top-k), trusted or not.
    pub candidates: Vec<RateCandidate>,
    /// ×½ and ×2 of the winning candidate, Bd (harmonics are ambiguous: always offered).
    pub harmonic_alternatives_bd: Vec<f64>,
    /// Samples per symbol at the trusted rate.
    pub sps: Option<f64>,
    /// The four raw cyclic lines.
    pub lines: Vec<CyclicLine>,
    /// The winning candidate's transition fit, if ok.
    pub transition_fit: Option<TransitionFit>,
    /// Search range, Hz. A function of OBW99 and the sample rate only: two readings of one emitter
    /// searched the same band however long each was watched for (T-327).
    pub rate_range_hz: (f64, f64),
    /// Cyclic comb fundamentals excluded from the search as capture artefacts, Hz (T-373).
    ///
    /// Derived from the capture's own provenance and sample rate — never a constant — and listed
    /// here so a reader can see what this measurement refused to look at. Every harmonic of each
    /// entry inside [`SymbolParameters::rate_range_hz`] was excluded. Empty when the capture
    /// records no artefact, or when the record was too short to resolve the comb.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded_cyclic_hz: Vec<f64>,
    /// Cyclic lines excluded from the search because this receiver was **measured** to contribute
    /// them, Hz, ascending (T-394).
    ///
    /// The frequencies of [`ReceiverLines`] that fall inside [`SymbolParameters::rate_range_hz`].
    /// Unlike [`SymbolParameters::excluded_cyclic_hz`] these are not derived from a record naming
    /// an artefact: each was seen at one frequency in channels of this same capture that hold no
    /// emission, which is what makes it the receiver's (see [`receiver`]). Empty when no survey
    /// was in force for this window, when the survey belongs to another device/tune/gain state, or
    /// when it would have notched more than a quarter of the search band.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded_receiver_hz: Vec<f64>,
    /// Family label (`Unknown` unless SNR_ext ≥ floor and confidence ≥ floor).
    pub family: Family,
    /// Family confidence `best · (1 − 0.7·second)`.
    pub family_confidence: f64,
    /// Scores.
    pub family_scores: FamilyScores,
    /// Features.
    pub family_features: FamilyFeatures,
    /// No line and a continuous IF: probably analog.
    pub analog_likely: bool,
    /// FSK deviation, Hz (FSK label with a trusted transition-fit rate).
    pub deviation_hz: Estimate,
    /// Modulation index h.
    pub mod_index_h: Estimate,
    /// FSK level means (settled symbols), Hz relative to the snippet centre, low then high.
    pub fsk_levels_hz: Vec<f64>,
    /// Mid-point CFO from the levels ([`crate::params::cfo_from_fsk_levels`]), relative to the
    /// snippet centre.
    pub cfo_fsk_levels: Estimate,
    /// Reasons (unknown, untrusted, qualifications).
    pub reasons: Vec<BlindReason>,
    /// Estimation time, µs.
    pub cost_us: u64,
}

impl SymbolParameters {
    fn empty(fs: f64, n: usize, snr: Option<f64>, reason: Reason) -> Self {
        Self {
            version: BLIND_ESTIMATOR_VERSION.into(),
            sample_rate_hz: fs,
            samples: n,
            snr_ext_db: snr,
            symbol_rate_bd: Estimate::abstain(Method::SymbolRateLines, reason),
            rate_trust: RateTrust::default(),
            candidates: Vec::new(),
            harmonic_alternatives_bd: Vec::new(),
            sps: None,
            lines: Vec::new(),
            transition_fit: None,
            rate_range_hz: (f64::NAN, f64::NAN),
            excluded_cyclic_hz: Vec::new(),
            excluded_receiver_hz: Vec::new(),
            family: Family::Unknown,
            family_confidence: 0.0,
            family_scores: FamilyScores::default(),
            family_features: FamilyFeatures::default(),
            analog_likely: false,
            deviation_hz: Estimate::abstain(Method::FskDeviation, reason),
            mod_index_h: Estimate::abstain(Method::ModulationIndex, reason),
            fsk_levels_hz: Vec::new(),
            cfo_fsk_levels: Estimate::abstain(Method::CfoFskLevels, reason),
            reasons: Vec::new(),
            cost_us: 0,
        }
    }

    /// True when the rate is trusted.
    pub fn rate_trusted(&self) -> bool {
        self.rate_trust.trusted
    }

    /// The best candidate rate, trusted or not.
    pub fn best_candidate_bd(&self) -> Option<f64> {
        self.candidates.first().map(|c| c.rate_bd)
    }

    /// Fills the data-model fields this estimate supplies (docs/07 §2.14): symbol rate and
    /// deviation when measured, modulation order for a labelled family.
    pub fn apply_to(&self, p: &mut EstimatedParams) {
        if let Some(r) = self.symbol_rate_bd.value() {
            p.symbol_rate_hz = Some(r);
        }
        if let Some(d) = self.deviation_hz.value() {
            p.deviation_hz = Some(d);
        }
        p.mod_order = match self.family {
            Family::Unknown => p.mod_order,
            Family::Ook | Family::Fsk | Family::Bpsk => Some(2),
            Family::Qpsk => Some(4),
        };
    }
}

/// The C14 estimator. Caches FFT plans across windows.
#[derive(Default)]
pub struct BlindEstimator {
    config: BlindConfig,
    plans: Plans,
    /// Cyclic lines **measured** to be this receiver's (T-394), applied to every window captured
    /// through the same device, tune and gain state.
    ///
    /// It lives on the estimator rather than on [`BlindInput`] because it describes the receiver
    /// being looked through, not the window being looked at: one survey serves every box of a
    /// capture, and it is checked against each window's own provenance before it is used
    /// ([`ReceiverLines::applies_to`]) so a survey never crosses a device, a retune or a gain step.
    receiver: Option<ReceiverLines>,
}

enum LsMode<'a> {
    Fsk {
        fi: &'a [f64],
        mid: f64,
        valid: &'a [bool],
    },
    Ook {
        env: &'a [f64],
        thr: f64,
    },
}

struct LsFitter<'a> {
    mode: LsMode<'a>,
    fs: f64,
    guards: LsGuards,
}

impl Fitter for LsFitter<'_> {
    fn fit(&mut self, seed_bd: f64) -> TransitionFit {
        self.fit_inner(self.fs / seed_bd)
    }
}

impl LsFitter<'_> {
    fn fit_inner(&self, t0: f64) -> TransitionFit {
        match &self.mode {
            LsMode::Fsk { fi, mid, valid } => {
                let v = moving_avg(fi, ((0.4 * t0) as usize).max(1));
                rate_transitions_ls(&v, self.fs, *mid, t0, Some(valid), &self.guards)
            }
            LsMode::Ook { env, thr } => {
                rate_transitions_ls(env, self.fs, *thr, t0, None, &self.guards)
            }
        }
    }
}

impl BlindEstimator {
    /// An estimator with `config`.
    pub fn new(config: BlindConfig) -> Self {
        Self {
            config,
            plans: Plans::default(),
            receiver: None,
        }
    }

    /// Settings.
    pub fn config(&self) -> &BlindConfig {
        &self.config
    }

    /// Measures this receiver's own cyclic lines over a capture window and keeps them
    /// ([`receiver::survey`], T-394). Returns whether the survey produced a set.
    ///
    /// A survey that abstains — too short a window, too few channels holding nothing — **clears**
    /// what was held rather than leaving a stale one in place: an exclusion that no longer
    /// describes the receiver is worse than none, because it is invisible.
    pub fn survey_receiver_lines<T: hk_dsp::IqSample>(
        &mut self,
        info: hk_dsp::InputInfo<'_>,
        samples: &[T],
        cfg: &SurveyConfig,
    ) -> bool {
        self.receiver = receiver::survey(&mut self.plans, info, samples, cfg);
        self.receiver.is_some()
    }

    /// The measured receiver lines in force, if any.
    pub fn receiver_lines(&self) -> Option<&ReceiverLines> {
        self.receiver.as_ref()
    }

    /// Sets (or clears) the measured receiver lines directly — for a caller that surveyed
    /// elsewhere, and for the controls that prove this exclusion has teeth.
    pub fn set_receiver_lines(&mut self, lines: Option<ReceiverLines>) {
        self.receiver = lines;
    }

    /// Prepares a snippet for C14. With a measured OBW99 and CFO: [`normalise`] with
    /// [`BlindConfig::normalise_config`]. Otherwise (C13 below its bandwidth floor, e.g. a long
    /// weak subcarrier): the snippet recentred at the CFO if measured (else the box centre), cut
    /// to the extent (else the box), with the detection-box width standing in for OBW99
    /// ([`ObwSource::DetectionBox`], reported as [`BlindReason::BandwidthFromBox`]). SNR is the
    /// extent SNR, else the box SNR; without either nothing is trusted.
    pub fn prepare(
        &self,
        snip: &ChannelSnippet,
        params: &ParameterSet,
    ) -> Result<BlindWindow, EstimateError> {
        if params.obw99_hz.is_measured() && params.cfo_hz.is_measured() {
            let n = normalise(snip, params, &self.config.normalise_config())?;
            let i = BlindInput::from_normalised(&n).ok_or(EstimateError::NoOutput)?;
            return Ok(BlindWindow {
                samples: n.samples.clone(),
                sample_rate_hz: i.sample_rate_hz,
                obw_hz: i.obw_hz,
                obw_source: i.obw_source,
                snr_ext_db: i.snr_ext_db,
                noise_power: i.noise_power,
                channel_bandwidth_hz: i.channel_bandwidth_hz,
                center_offset_hz: i.center_offset_hz,
                source_index: n.time.source_index,
                source_per_sample: n.time.source_per_output,
                capture: Some(n.provenance.clone()),
            });
        }
        let fs = snip.sample_rate_hz;
        let (obw, source) = match params.obw99_hz.value() {
            Some(o) => (o, ObwSource::Obw99),
            None if snip.box_bandwidth_hz > 0.0 => (snip.box_bandwidth_hz, ObwSource::DetectionBox),
            None => {
                return Err(EstimateError::Unmeasured {
                    what: "obw99_hz",
                    reason: params.obw99_hz.reason().unwrap_or(Reason::Upstream),
                });
            }
        };
        let cfo = params.cfo_hz.value().unwrap_or(0.0);
        if cfo.abs() >= 0.49 * fs {
            return Err(EstimateError::InvalidRequest(format!(
                "CFO {cfo} Hz outside ±{} Hz",
                fs / 2.0
            )));
        }
        let (s0, s1) = params
            .extent
            .map_or((snip.box_range.start, snip.box_range.end), |e| {
                (e.start, e.end)
            });
        let s1 = s1.min(snip.samples.len());
        let s0 = s0.min(s1);
        let mut mixed = Vec::new();
        mix_into(&snip.samples, cfo, fs, &mut mixed);
        let band = 2.0 * snip.passband_hz;
        Ok(BlindWindow {
            samples: mixed[s0..s1].to_vec(),
            sample_rate_hz: fs,
            obw_hz: obw,
            obw_source: source,
            snr_ext_db: params
                .snr_extent_db
                .value()
                .or_else(|| params.snr_box_db.value()),
            noise_power: params.noise_density.value().map(|n0| n0 * band),
            channel_bandwidth_hz: band,
            center_offset_hz: cfo,
            source_index: snip.source_index_of(s0),
            source_per_sample: snip.time.source_per_output,
            capture: Some(snip.provenance.clone()),
        })
    }

    /// [`BlindEstimator::prepare`] then [`BlindEstimator::estimate`]; a snippet that cannot be
    /// prepared gives an all-abstained result (`upstream`).
    pub fn estimate_snippet(
        &mut self,
        snip: &ChannelSnippet,
        params: &ParameterSet,
    ) -> SymbolParameters {
        let started = Instant::now();
        let mut out = match self.prepare(snip, params) {
            Ok(w) => self.estimate(&w.input()),
            Err(_) => {
                let mut out = SymbolParameters::empty(
                    snip.sample_rate_hz,
                    snip.samples.len(),
                    None,
                    Reason::Upstream,
                );
                out.reasons.push(BlindReason::InvalidInput);
                out
            }
        };
        out.cost_us = started.elapsed().as_micros() as u64;
        out
    }

    /// Estimates a normalised snippet. Without a measured OBW99 everything abstains
    /// (`upstream`).
    pub fn estimate_normalised(&mut self, n: &NormalisedSnippet) -> SymbolParameters {
        match BlindInput::from_normalised(n) {
            Some(input) => self.estimate(&input),
            None => {
                let mut out = SymbolParameters::empty(
                    n.sample_rate_hz,
                    n.samples.len(),
                    None,
                    Reason::Upstream,
                );
                out.reasons.push(BlindReason::InvalidInput);
                out
            }
        }
    }

    /// Estimates one window. See the [module docs](self).
    pub fn estimate(&mut self, input: &BlindInput<'_>) -> SymbolParameters {
        let started = Instant::now();
        let mut out = self.estimate_inner(input);
        out.cost_us = started.elapsed().as_micros() as u64;
        out
    }

    fn estimate_inner(&mut self, input: &BlindInput<'_>) -> SymbolParameters {
        let cfg = self.config.clone();
        let fs = input.sample_rate_hz;
        let obw = input.obw_hz;
        let snr = input.snr_ext_db.filter(|s| s.is_finite());
        if !(fs.is_finite() && fs > 0.0 && obw.is_finite() && obw > 0.0) {
            let mut out =
                SymbolParameters::empty(fs, input.samples.len(), snr, Reason::InvalidInput);
            out.reasons.push(BlindReason::InvalidInput);
            return out;
        }
        if input.samples.len() < cfg.min_samples {
            let mut out = SymbolParameters::empty(fs, input.samples.len(), snr, Reason::TooShort);
            out.reasons.push(BlindReason::TooShort);
            return out;
        }
        let mut reasons = Vec::new();
        if input.obw_source == ObwSource::DetectionBox {
            reasons.push(BlindReason::BandwidthFromBox);
        }
        let snr_v = match snr {
            None => {
                reasons.push(BlindReason::NoSnr);
                f64::NEG_INFINITY
            }
            Some(s) => {
                if s < cfg.family_snr_db {
                    reasons.push(BlindReason::LowSnr);
                }
                s
            }
        };

        // Channel filter with its −6 dB point at ±0.75·OBW, always. S5's `lowpass` is a
        // `firwin(8·fs/cutoff taps)` Hamming design at cutoff 0.75·OBW applied with `filtfilt`:
        // passband ≈ 0.6·OBW, stopband ≈ 0.9·OBW. The Kaiser [`ChannelFilter`] has its stopband
        // at 4/3 × the passband, so a passband of 6/7 × 0.75·OBW puts its mid-transition at
        // 0.75·OBW. A flat ±0.75·OBW passband (the normalisation) passes up to ~2× the noise:
        // spurious slicer transitions on Gaussian FSK edges, weaker envelope lines on RDS.
        let pass = cfg.channel_filter_obw * obw * 6.0 / 7.0;
        let filtered;
        let mut noise_power = input.noise_power;
        let x_all: &[Complex32] = match ChannelFilter::new(fs, pass) {
            Some(f) => {
                let mut y = Vec::new();
                f.apply(input.samples, &mut y);
                if let Some(np) = noise_power.as_mut() {
                    *np *= (f.enbw_hz / input.channel_bandwidth_hz.max(1e-9)).min(1.0);
                }
                filtered = y;
                &filtered
            }
            None => input.samples,
        };

        // 1. Envelope levels.
        let p: Vec<f64> = x_all.iter().map(|s| f64::from(s.norm_sqr())).collect();
        let a_len = ((fs / obw) as usize).max(1); // int(win·fs/2) with win = 2/OBW
        let a_all: Vec<f64> = moving_avg(&p, a_len).into_iter().map(f64::sqrt).collect();
        let nz = noise_power
            .filter(|v| v.is_finite() && *v > 0.0)
            .unwrap_or_else(|| quantile(&moving_avg(&p, ((2.0 * fs / obw) as usize).max(1)), 0.1));
        let (lv, lab) = kmeans2(&a_all);
        let (ilo, ihi) = if lv[0] == lv[1] {
            (0u8, 0u8)
        } else if lv[0] < lv[1] {
            (0, 1)
        } else {
            (1, 0)
        };
        let level = |k: u8| {
            let m: Vec<f64> = a_all
                .iter()
                .zip(&lab)
                .filter(|&(_, &l)| l == k)
                .map(|(&v, _)| v)
                .collect();
            if m.is_empty() {
                lv[k as usize]
            } else {
                median(&m)
            }
        };
        let (lo, hi) = (level(ilo), level(ihi));
        let contrast_db = 20.0 * (hi / lo.max(1e-9)).log10();
        let mut labs = median5(&lab);
        let n_env_tr = labs.windows(2).filter(|w| w[0] != w[1]).count();
        let (mut g0, mut g1) = (0, x_all.len());
        if contrast_db > 6.0 && n_env_tr < 12 {
            reasons.push(BlindReason::MultiSegment);
            let mask: Vec<bool> = labs.iter().map(|&l| l == ihi).collect();
            let (s0, s1) = longest_run(&mask, ((2.0 * fs / obw) as usize).max(2));
            if s1 - s0 >= 64 {
                (g0, g1) = (s0, s1);
                labs = vec![ihi; s1 - s0];
            }
        }
        let xe = &x_all[g0..g1];
        let a = &a_all[g0..g1];
        let n = xe.len();
        let p_lo = labs.iter().filter(|&&l| l == ilo).count() as f64 / labs.len() as f64;
        let on: Vec<bool> = if contrast_db > 6.0 && n_env_tr >= 12 {
            labs.iter().map(|&l| l == ihi).collect()
        } else {
            vec![true; n]
        };
        let n_on = on.iter().filter(|&&o| o).count();
        let xon: Vec<Complex32> = if n_on > 64 {
            xe.iter()
                .zip(&on)
                .filter(|(_, o)| **o)
                .map(|(s, _)| *s)
                .collect()
        } else {
            xe.to_vec()
        };
        let mags: Vec<f64> = xon.iter().map(|s| f64::from(s.norm())).collect();
        let env_cv = std(&mags) / (mean(&mags) + 1e-12);
        let fi = inst_freq(xe, fs);
        let fis = moving_avg(&fi, ((fs / obw / 2.0) as usize).max(1));
        let on1 = &on[1..];

        // 2. Raw cyclic lines. Both edges of the band and the whitening width are pinned by the
        // signal and the receiver; none of them may be a function of `n` (T-327).
        let f_min = cfg.rate_min_hz.unwrap_or(obw * cfg.rate_min_obw);
        let whiten_hz = cfg.whiten_block_obw * obw;
        let f_max = cfg
            .rate_max_hz
            .unwrap_or(cfg.rate_max_obw * obw)
            .min(cfg.rate_max_fs * fs);
        // T-373: cyclic combs this capture's own chain contributes. Both the fundamentals and the
        // notch width come from the capture and the transform — the fundamentals from the
        // recorded artefact period against the capture's sample rate, the width from this
        // record's native bin. Nothing here knows a frequency.
        let native_hz = fs / n as f64;
        let guard_hz = cfg.artefact_guard_bins.max(0.0) * native_hz;
        // The spacing test is against the *widest* guard the comb will apply inside this band, not
        // the transform's floor: a comb that drifts is notched more generously at its top harmonic
        // than at its fundamental (T-382), and it is that width the quarter-band rule must judge.
        let combs: Vec<hk_model::CyclicComb> = input
            .capture
            .map(Provenance::cyclic_combs)
            .unwrap_or_default()
            .into_iter()
            .filter(|_| guard_hz > 0.0)
            .filter(|c| c.fundamental_hz.is_finite() && c.fundamental_hz > 0.0)
            .filter(|c| {
                let in_band = (f_max / c.fundamental_hz).floor().max(1.0);
                let top = c.harmonics.map_or(in_band, |m| in_band.min(f64::from(m)));
                c.fundamental_hz >= 4.0 * (guard_hz + top * c.fundamental_hz * c.drift.max(0.0))
            })
            .collect();
        let excluded_cyclic_hz: Vec<f64> = combs.iter().map(|c| c.fundamental_hz).collect();
        // T-394: cyclic lines this receiver was **measured** to contribute — seen at one frequency
        // in channels of this same capture that hold no emission. Every entry of `combs` above is
        // a frequency someone wrote down; every entry here is one the capture was measured to
        // carry, which is the only form of this that generalises past one fixture.
        //
        // A survey is device-local physics, so it is applied only to a window captured through the
        // same device, tune and gain state (T-259/T-305), and only when the window carries the
        // provenance to check that against.
        let receiver_guard_hz = self
            .receiver
            .as_ref()
            .map_or(0.0, |r| r.guard_hz(native_hz));
        let receiver_lines: &[receiver::ReceiverLine] = self
            .receiver
            .as_ref()
            .filter(|r| input.capture.is_some_and(|p| r.applies_to(p)))
            .map(|r| r.in_band(f_min, f_max))
            .unwrap_or_default();
        // The same quarter-band rule the combs obey: a notch removing more than a quarter of the
        // search band is deleting the band rather than an artefact, and a survey that produced one
        // is measuring something other than lines.
        let notched: f64 = receiver_lines
            .iter()
            .map(|l| 2.0 * (l.half_width_hz + receiver_guard_hz))
            .sum();
        let receiver_lines = if notched > 0.25 * (f_max - f_min) {
            &[][..]
        } else {
            receiver_lines
        };
        let excluded_receiver_hz: Vec<f64> = receiver_lines.iter().map(|l| l.freq_hz).collect();
        let excluded = lines::Excluded {
            combs: &combs,
            guard_hz,
            receiver: receiver_lines,
            receiver_guard_hz,
            receiver_max_half_hz: receiver_lines
                .iter()
                .map(|l| l.half_width_hz)
                .fold(0.0, f64::max),
        };
        let d = ((fs / obw / 2.0).round() as usize).max(1);
        let env_sq: Vec<f64> = xe.iter().map(|s| f64::from(s.norm_sqr())).collect();
        let env_diff: Vec<f64> = a.windows(2).map(|w| (w[1] - w[0]).powi(2)).collect();
        let dm: Vec<Complex<f64>> = (d..n)
            .map(|i| {
                let v = xe[i] * xe[i - d].conj();
                Complex::new(f64::from(v.re), f64::from(v.im))
            })
            .collect();
        let if_diff: Vec<f64> = fis
            .windows(2)
            .zip(&on1[1..])
            .map(|(w, &o)| if o { (w[1] - w[0]).powi(2) } else { 0.0 })
            .collect();
        let plans = &mut self.plans;
        let mk = |m: LineMethod, l: Option<lines::RawLine>| CyclicLine {
            method: m,
            group: m.group(),
            freq_hz: l.map(|l| l.freq_hz),
            significance_db: l.map_or(0.0, |l| l.significance_db),
            sigma_hz: l.map(|l| l.sigma_hz),
            whiten_clamped: l.is_some_and(|l| l.whiten_clamped),
            artefact_suppressed: l.is_some_and(|l| l.artefact_suppressed),
        };
        let raw_lines = vec![
            mk(
                LineMethod::EnvelopeSquare,
                spectral_line(plans, &env_sq, fs, f_min, f_max, whiten_hz, excluded),
            ),
            mk(
                LineMethod::EnvelopeDiff,
                spectral_line(plans, &env_diff, fs, f_min, f_max, whiten_hz, excluded),
            ),
            mk(
                LineMethod::DelayMultiply,
                complex_line(plans, &dm, fs, f_min, f_max, whiten_hz, excluded),
            ),
            mk(
                LineMethod::IfDiff,
                spectral_line(plans, &if_diff, fs, f_min, f_max, whiten_hz, excluded),
            ),
        ];

        // 3. Family scores. FSK: symbol-centre IF of the top candidates.
        let on_fis: Vec<f64> = if on1.iter().filter(|&&o| o).count() > 64 {
            fis.iter()
                .zip(on1)
                .filter(|(_, o)| **o)
                .map(|(v, _)| *v)
                .collect()
        } else {
            fis.clone()
        };
        let (q_all, _) = kmeans2(&on_fis);
        let mid_all = 0.5 * (q_all[0] + q_all[1]);
        let seed_fsk = runlength_unit(&fis, fs, mid_all, f_max, Some(on1));
        let mut cand: Vec<&CyclicLine> = raw_lines
            .iter()
            .filter(|l| l.freq_hz.is_some() && l.significance_db >= cfg.line_count_db)
            .collect();
        cand.sort_by(|a, b| b.significance_db.total_cmp(&a.significance_db));
        let cvals: Vec<f64> = cand
            .iter()
            .take(3)
            .filter_map(|l| l.freq_hz)
            .chain(seed_fsk)
            .filter(|c| (f_min..=f_max).contains(c))
            .collect();
        let fsk_best = cvals
            .iter()
            .filter_map(|&c| symbol_centre_stats(&fi, fs, c, on1))
            .max_by(|a, b| {
                let k = |s: &FskCentreStats| s.fisher_j * if s.valley < 0.6 { 1.0 } else { 0.3 };
                k(a).total_cmp(&k(b))
            });
        let mut fsk_score = fsk_best.map_or(0.0, |s| {
            ((s.fisher_j - 2.5) / 2.0).clamp(0.0, 1.0)
                * if s.occupancy > 0.1 { 1.0 } else { 0.3 }
                * if s.separation_hz > 0.1 * obw {
                    1.0
                } else {
                    0.2
                }
                * if env_cv < 0.35 { 1.0 } else { 0.4 }
                * if s.valley < 0.6 { 1.0 } else { 0.15 }
                * if s.periodicity > 0.95 { 0.1 } else { 1.0 }
        });
        let noise_rms = nz.sqrt();
        let mut ook_score = 0.0;
        if p_lo > 0.08 && p_lo < 0.92 && n_env_tr >= 12 {
            ook_score = ((contrast_db - 8.0) / 6.0).clamp(0.0, 1.0)
                * if lo < 2.5 * noise_rms { 1.0 } else { 0.4 };
        }
        if ook_score >= 0.8 {
            fsk_score *= 0.3;
        } else if fsk_score >= 0.5 {
            ook_score *= 0.3;
        }
        let (c1, _, _) = carrier_line(plans, &xon, fs, 1);
        let (c2, _, u2) = carrier_line(plans, &xon, fs, 2);
        let (c4, _, _) = carrier_line(plans, &xon, fs, 4);
        let bpsk_score = ((c2 - 0.25) / 0.25).clamp(0.0, 1.0)
            * if c1 < 0.5 * c2 { 1.0 } else { 0.3 }
            * if u2 < 0.6 { 1.0 } else { 0.2 };
        let qpsk_score = ((c4 - 0.2) / 0.2).clamp(0.0, 1.0) * if c2 < 0.5 * c4 { 1.0 } else { 0.2 };
        let scores = FamilyScores {
            ook: ook_score,
            fsk: fsk_score,
            bpsk: bpsk_score,
            qpsk: qpsk_score,
        };
        let ranked = scores.ranked();
        let conf = ranked[0].1 * (1.0 - 0.7 * ranked[1].1);
        let carrier_only = c1 > 0.7 && ook_score < 0.3;
        let digital = ranked[0].1 >= 0.5 && !carrier_only;
        let mut family = Family::Unknown;
        let mut analog_likely = false;
        if carrier_only {
            reasons.push(BlindReason::UnmodulatedCarrier);
        } else if conf >= cfg.family_min_confidence && snr_v >= cfg.family_snr_db {
            family = ranked[0].0;
        } else {
            if conf < cfg.family_min_confidence {
                reasons.push(BlindReason::AmbiguousFamily);
            }
            analog_likely = env_cv < 0.25 && ranked[0].1 < 0.2;
        }

        // 4. Rate consensus.
        let fsk_like =
            family == Family::Fsk || (family == Family::Unknown && fsk_score >= ook_score.max(0.3));
        let ook_like =
            family == Family::Ook || (family == Family::Unknown && ook_score > fsk_score.max(0.3));
        let mut seeds = Vec::new();
        let mut fitter = if fsk_like && !carrier_only {
            seeds.extend(seed_fsk);
            Some(LsFitter {
                mode: LsMode::Fsk {
                    fi: &fi,
                    mid: mid_all,
                    valid: on1,
                },
                fs,
                guards: cfg.ls,
            })
        } else if ook_like {
            let thr = 0.5 * (lo + hi);
            seeds.extend(runlength_unit(a, fs, thr, f_max, None));
            Some(LsFitter {
                mode: LsMode::Ook { env: a, thr },
                fs,
                guards: cfg.ls,
            })
        } else {
            None
        };
        let th = Thresholds {
            count_db: cfg.line_count_db,
            trust_db: cfg.line_trust_db,
            lines_without_fit: cfg.lines_without_fit,
        };
        let cons: Consensus = rate_consensus_dyn(
            &raw_lines,
            f_min,
            f_max,
            fitter.as_mut().map(|f| f as &mut dyn Fitter),
            &seeds,
            &th,
        );
        let fit = cons.fit.clone();
        let consensus_trusted = cons.trusted && !carrier_only;
        let structure = digital || fit.is_some();
        let snr_ok = snr_v >= cfg.rate_snr_db;
        let trusted = consensus_trusted && structure && snr_ok;
        if cons.candidates.is_empty() {
            reasons.push(BlindReason::NoCyclicLine);
        } else if !cons.trusted {
            reasons.push(BlindReason::WeakRateConsensus);
        }
        if cons.trusted && !structure {
            reasons.push(BlindReason::NoDigitalStructure);
        }
        if consensus_trusted
            && structure
            && !snr_ok
            && !reasons.contains(&BlindReason::LowSnr)
            && snr.is_some()
        {
            reasons.push(BlindReason::LowSnr);
        }

        let mut out = SymbolParameters::empty(fs, n, snr, Reason::Upstream);
        out.rate_range_hz = (f_min, f_max);
        // T-373: what the search refused to look at, and whether refusing changed the answer.
        if raw_lines.iter().any(|l| l.artefact_suppressed) {
            reasons.push(BlindReason::CaptureArtefact);
        }
        out.excluded_cyclic_hz = excluded_cyclic_hz;
        out.excluded_receiver_hz = excluded_receiver_hz;
        out.lines = raw_lines;
        out.family = family;
        out.family_confidence = conf;
        out.family_scores = scores;
        out.family_features = FamilyFeatures {
            contrast_db,
            low_fraction: p_lo,
            envelope_changes: n_env_tr,
            envelope_cv: env_cv,
            low_level_noise_sigmas: lo / noise_rms.max(1e-30),
            fsk: fsk_best,
            c1,
            c2,
            x2_second_line: u2,
            c4,
        };
        out.analog_likely = analog_likely;
        out.rate_trust = RateTrust {
            trusted,
            consensus: cons.trusted,
            digital_structure: structure,
            groups: cons.groups.clone(),
            strong_groups: cons.strong_groups.clone(),
        };
        out.harmonic_alternatives_bd = cons
            .centre_bd
            .map(|c| vec![0.5 * c, 2.0 * c])
            .unwrap_or_default();
        out.transition_fit = fit.clone();
        let best = cons.candidates.first().cloned();
        out.candidates = cons.candidates.into_iter().take(cfg.top_k.max(1)).collect();
        let method = if fit.is_some() {
            Method::SymbolRateTransitions
        } else {
            Method::SymbolRateLines
        };
        let evidence = Evidence {
            significance_db: best.as_ref().and_then(|b| {
                b.direct
                    .iter()
                    .chain(&b.harmonic)
                    .map(|s| s.significance_db)
                    .reduce(f64::max)
            }),
            threshold_db: Some(cfg.line_trust_db),
            samples: Some(n as u64),
            ..Default::default()
        };
        out.symbol_rate_bd = match &best {
            None => Estimate::abstain(method, Reason::NoLine),
            Some(b) if trusted => {
                let sigma = if b.sigma_bd.is_finite() && b.sigma_bd > 0.0 {
                    b.sigma_bd
                } else {
                    1e-3 * b.rate_bd
                };
                out.sps = Some(fs / b.rate_bd);
                Estimate::measured(b.rate_bd, sigma, method)
            }
            Some(_) if consensus_trusted && structure && !snr_ok => {
                Estimate::abstain(method, Reason::LowSnr)
            }
            Some(_) => Estimate::abstain(method, Reason::Untrusted),
        }
        .with_evidence(evidence);
        out.reasons = reasons;

        // 6. FSK deviation.
        let dev_reason = if family != Family::Fsk {
            Some(Reason::NotApplicable)
        } else if !(trusted && fit.is_some()) {
            Some(Reason::Upstream)
        } else {
            None
        };
        if let Some(r) = dev_reason {
            out.deviation_hz = Estimate::abstain(Method::FskDeviation, r);
            out.mod_index_h = Estimate::abstain(Method::ModulationIndex, r);
            out.cfo_fsk_levels = Estimate::abstain(Method::CfoFskLevels, r);
        } else if let (Some(f), Some(rate)) = (fit.as_ref(), out.symbol_rate_bd.value()) {
            deviation(&mut out, f, &fi, &on, rate, input.center_offset_hz);
        }
        out
    }
}

fn rate_consensus_dyn(
    lines: &[CyclicLine],
    f_min: f64,
    f_max: f64,
    fitter: Option<&mut dyn Fitter>,
    seeds: &[f64],
    th: &Thresholds,
) -> Consensus {
    consensus::rate_consensus(lines, f_min, f_max, fitter, seeds, th)
}

/// FSK deviation at the LS symbol centres of each segment (see the [module docs](self)).
fn deviation(
    out: &mut SymbolParameters,
    fit: &TransitionFit,
    fi: &[f64],
    on: &[bool],
    rate: f64,
    center_offset_hz: f64,
) {
    let t = fit.period_samples;
    let len = fi.len();
    let nseg = fit.segment_offsets.len();
    let half = ((t / 4.0) as usize).max(1);
    let mut sm = Vec::new();
    for s in 0..nseg {
        let off = fit.segment_offsets[s];
        if !off.is_finite() {
            continue;
        }
        let b0 = if s == 0 {
            0.0
        } else {
            0.5 * (fit.segment_spans[s - 1].1 + fit.segment_spans[s].0)
        };
        let b1 = if s + 1 == nseg {
            len as f64
        } else {
            0.5 * (fit.segment_spans[s].1 + fit.segment_spans[s + 1].0)
        };
        let k0 = ((b0 - off - t / 2.0) / t).ceil();
        let mut k = k0;
        loop {
            let c = off + t / 2.0 + k * t;
            if c >= b1 || c >= (len - 1) as f64 {
                break;
            }
            k += 1.0;
            if c < 0.0 {
                continue;
            }
            let ci = c as usize;
            if ci + half >= len || !on[ci.min(on.len() - 1)] {
                continue;
            }
            let w = &fi[ci.saturating_sub(half)..=ci + half];
            sm.push(w.iter().sum::<f64>() / w.len() as f64);
        }
    }
    if sm.len() <= 8 {
        out.deviation_hz = Estimate::abstain(Method::FskDeviation, Reason::TooShort);
        out.mod_index_h = Estimate::abstain(Method::ModulationIndex, Reason::TooShort);
        out.cfo_fsk_levels = Estimate::abstain(Method::CfoFskLevels, Reason::TooShort);
        return;
    }
    let (q, _) = kmeans2(&sm);
    let mid = 0.5 * (q[0] + q[1]);
    let dec: Vec<bool> = sm.iter().map(|&v| v > mid).collect();
    let full: Vec<bool> = (0..sm.len())
        .map(|i| i > 0 && i + 1 < sm.len() && dec[i] == dec[i - 1] && dec[i] == dec[i + 1])
        .collect();
    let n_full = full.iter().filter(|&&f| f).count();
    let sel: Vec<usize> = if n_full >= 8 {
        (0..sm.len()).filter(|&i| full[i]).collect()
    } else {
        (0..sm.len()).collect()
    };
    let vals: Vec<f64> = sel.iter().map(|&i| (sm[i] - mid).abs()).collect();
    let dev = median(&vals);
    let dev_sigma = 1.2533 * std(&vals) / (vals.len() as f64).sqrt();
    let evidence = Evidence {
        samples: Some(vals.len() as u64),
        ..Default::default()
    };
    out.deviation_hz =
        Estimate::measured(dev, dev_sigma.max(1e-9), Method::FskDeviation).with_evidence(evidence);
    let rate_sigma = out.symbol_rate_bd.sigma().unwrap_or(0.0);
    let h = 2.0 * dev / rate;
    let h_sigma = h * ((dev_sigma / dev).powi(2) + (rate_sigma / rate).powi(2)).sqrt();
    out.mod_index_h =
        Estimate::measured(h, h_sigma.max(1e-9), Method::ModulationIndex).with_evidence(evidence);
    let level = |hi_side: bool| -> Option<(f64, f64)> {
        let v: Vec<f64> = sel
            .iter()
            .filter(|&&i| dec[i] == hi_side)
            .map(|&i| sm[i])
            .collect();
        (v.len() >= 2).then(|| (mean(&v), std(&v) / (v.len() as f64).sqrt()))
    };
    match (level(false), level(true)) {
        (Some((lo, slo)), Some((hi, shi))) => {
            out.fsk_levels_hz = vec![lo + center_offset_hz, hi + center_offset_hz];
            out.cfo_fsk_levels =
                crate::params::cfo_from_fsk_levels(&out.fsk_levels_hz, &[slo, shi])
                    .with_evidence(evidence);
        }
        _ => {
            out.cfo_fsk_levels = Estimate::abstain(Method::CfoFskLevels, Reason::NoClusters);
        }
    }
}
