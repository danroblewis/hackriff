//! [`FskReceiver`]: one detection box → snippet → C13 (FSK hint) → C14 → FSK demodulation,
//! with **prior-led trial demodulation** below the C14 trust floor (spike S5 §5 "T-013").

use hk_dsp::{InputInfo, IqSample};
use hk_estimate::blind::{BlindConfig, BlindEstimator, SymbolParameters};
use hk_estimate::framing::{FramingModel, locate_sync};
use hk_estimate::{
    FamilyHint, Hints, ParamEstimator, ParameterSet, SnippetConfig, SnippetExtractor,
    SnippetRequest,
};
use hk_model::{EstimatedParams, SampleTime, TimeRange, Timestamp};
use serde::{Deserialize, Serialize};

use super::demod::{FSK_DEMOD_VERSION, FskDemod, FskDemodConfig, FskDemodRequest, FskSymbols};
use crate::DemodError;

/// Standard symbol rates tried when nothing better is known, Bd.
pub const STANDARD_RATES_BD: &[f64] = &[
    1_200.0, 2_400.0, 4_800.0, 9_600.0, 19_200.0, 38_400.0, 50_000.0, 57_600.0, 76_800.0,
    100_000.0, 115_200.0, 150_000.0, 200_000.0, 250_000.0, 300_000.0,
];

/// Rate, deviation and channel raster from trusted C14 results of one emitter cluster.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClusterPrior {
    /// Symbol rate, Bd.
    pub rate_bd: f64,
    /// Deviation, Hz.
    pub deviation_hz: Option<f64>,
    /// Channel raster, Hz (receiver frame), used for the CFO when C13 abstains.
    pub raster_hz: Option<f64>,
    /// A raster channel centre, Hz (receiver frame).
    pub raster_origin_hz: Option<f64>,
    /// Trusted bursts behind it.
    pub support: usize,
    /// Where it came from, e.g. `trusted C14 median of 3 bursts`.
    pub source: String,
}

impl ClusterPrior {
    /// Groups the trusted C14 rates of `bursts` (within `tolerance`, relative) into priors,
    /// most supported first.
    pub fn from_trusted(bursts: &[FskBurst], tolerance: f64) -> Vec<ClusterPrior> {
        let mut rows: Vec<(f64, Option<f64>)> = bursts
            .iter()
            .filter(|b| b.seed.c14_trusted && b.seed.source == SeedSource::TrustedC14)
            .filter_map(|b| {
                let s = b.symbols.as_ref()?;
                Some((s.rate_bd, s.deviation_hz))
            })
            .collect();
        rows.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut groups: Vec<Vec<(f64, Option<f64>)>> = Vec::new();
        for r in rows {
            match groups.last_mut() {
                Some(g) if (r.0 / g[0].0 - 1.0).abs() <= tolerance => g.push(r),
                _ => groups.push(vec![r]),
            }
        }
        let median = |mut v: Vec<f64>| -> Option<f64> {
            if v.is_empty() {
                return None;
            }
            v.sort_by(f64::total_cmp);
            Some(v[v.len() / 2])
        };
        let mut out: Vec<ClusterPrior> = groups
            .into_iter()
            .map(|g| ClusterPrior {
                rate_bd: median(g.iter().map(|r| r.0).collect()).unwrap(),
                deviation_hz: median(g.iter().filter_map(|r| r.1).collect()),
                raster_hz: None,
                raster_origin_hz: None,
                support: g.len(),
                source: format!("trusted C14 median of {} bursts", g.len()),
            })
            .collect();
        out.sort_by(|a, b| b.support.cmp(&a.support));
        out
    }
}

/// A sync word to confirm trials with.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SyncPrior {
    /// Bits, transmission order (either polarity is searched).
    pub bits: Vec<u8>,
    /// Bit errors tolerated.
    pub max_errors: usize,
    /// Alternating preamble bits required before it.
    pub min_preamble_bits: usize,
    /// Where it came from.
    pub source: String,
}

impl SyncPrior {
    /// The sync word of a framing model.
    pub fn from_model(model: &FramingModel) -> Option<Self> {
        let s = model.sync.as_ref()?;
        Some(Self {
            bits: hk_estimate::framing::bits::parse_bit_string(&s.bits)?,
            max_errors: s.max_bit_errors,
            min_preamble_bits: 16,
            source: format!("framing model {}", model.signature()),
        })
    }
}

/// Priors for bursts C14 does not trust.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DemodPriors {
    /// Emitter-cluster priors, tried first.
    pub cluster: Vec<ClusterPrior>,
    /// Then the [`STANDARD_RATES_BD`] table.
    pub standard_rates: bool,
    /// A sync word that confirms a trial.
    pub sync: Option<SyncPrior>,
}

/// Where the demodulation rate came from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum SeedSource {
    /// C14 trusted rate and deviation.
    TrustedC14,
    /// An emitter-cluster prior.
    ClusterPrior {
        /// Index into [`DemodPriors::cluster`].
        index: usize,
        /// Its source description.
        source: String,
    },
    /// The standard-rate table.
    StandardRate,
}

impl SeedSource {
    /// Short name.
    pub fn as_str(&self) -> &'static str {
        match self {
            SeedSource::TrustedC14 => "trusted-c14",
            SeedSource::ClusterPrior { .. } => "cluster-prior",
            SeedSource::StandardRate => "standard-rate",
        }
    }
}

/// One trial.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrialOutcome {
    /// Rate tried, Bd.
    pub rate_bd: f64,
    /// Its source.
    pub source: SeedSource,
    /// Sync found (`None` without a sync prior).
    pub sync_found: Option<bool>,
    /// Lock quality (0 on failure).
    pub lock_quality: f64,
    /// Failure.
    pub error: Option<String>,
}

/// The seed actually used.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DemodSeed {
    /// Source.
    pub source: SeedSource,
    /// Rate, Bd.
    pub rate_bd: f64,
    /// Deviation used for the channel filter, Hz.
    pub deviation_hz: Option<f64>,
    /// C14 trusted the rate.
    pub c14_trusted: bool,
    /// A sync prior confirmed the chosen trial (`None` without a sync prior).
    pub sync_confirmed: Option<bool>,
    /// The seed rate was divided by this factor after a run-length check
    /// ([`run_length_divisor`]): every run of the first demodulation was a multiple of it.
    pub harmonic_divisor: Option<usize>,
    /// Every trial, in order.
    pub trials: Vec<TrialOutcome>,
}

/// Oversampling factor `k ≥ 2` suggested by the run lengths of `bits`: at a rate `k×` too high
/// every run is ≈ a multiple of `k` symbols (C20 pitfall: a wrong C14 rate yields garbage bits
/// with "lock"). `None` unless ≥ 12 inner runs, the 20th-percentile run is ≥ 2 symbols and
/// ≥ 80 % of runs lie within ±0.25 k of a non-zero multiple of `k`.
pub fn run_length_divisor(bits: &[u8]) -> Option<usize> {
    let runs = run_lengths(bits);
    if runs.len() < 14 {
        return None;
    }
    let inner = &runs[1..runs.len() - 1];
    let mut sorted = inner.to_vec();
    sorted.sort_unstable();
    let unit = sorted[sorted.len() / 5];
    if unit < 2 {
        return None;
    }
    (unit.saturating_sub(1).max(2)..=unit + 1)
        .map(|k| {
            let fit = inner
                .iter()
                .filter(|&&r| {
                    let q = r as f64 / k as f64;
                    q.round() >= 1.0 && (q - q.round()).abs() <= 0.25
                })
                .count() as f64
                / inner.len() as f64;
            (k, fit)
        })
        .filter(|&(_, fit)| fit >= 0.8)
        .max_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)))
        .map(|(k, _)| k)
}

fn run_lengths(bits: &[u8]) -> Vec<usize> {
    let mut runs = Vec::new();
    let mut len = 1;
    for w in bits.windows(2) {
        if w[0] == w[1] {
            len += 1;
        } else {
            runs.push(len);
            len = 1;
        }
    }
    if !bits.is_empty() {
        runs.push(len);
    }
    runs
}

/// Fraction of inner runs that are one symbol long (random NRZ ≈ 0.5; a `k×` rate ≈ 0).
fn single_run_fraction(bits: &[u8]) -> f64 {
    let runs = run_lengths(bits);
    if runs.len() < 3 {
        return 0.0;
    }
    let inner = &runs[1..runs.len() - 1];
    inner.iter().filter(|&&r| r == 1).count() as f64 / inner.len() as f64
}

/// Settings.
#[derive(Clone, Debug, PartialEq)]
pub struct FskReceiverConfig {
    /// Snippet extraction (its `min_rate_hz` is raised to `rate_factor × box bandwidth`).
    pub snippet: SnippetConfig,
    /// Snippet rate as a multiple of the box bandwidth (T-011 needs ≥ 6).
    pub rate_factor: f64,
    /// C13 hints (clock ppm); the family hint is set to 2-FSK.
    pub hints: Hints,
    /// C14 settings.
    pub blind: BlindConfig,
    /// Demodulator settings.
    pub demod: FskDemodConfig,
    /// Use a trusted C14 rate (off: priors only, e.g. to test a prior).
    pub use_trusted: bool,
    /// Most trials per burst.
    pub max_trials: usize,
}

impl Default for FskReceiverConfig {
    fn default() -> Self {
        Self {
            snippet: SnippetConfig::default(),
            rate_factor: 6.0,
            hints: Hints::default(),
            blind: BlindConfig::default(),
            demod: FskDemodConfig::default(),
            use_trusted: true,
            max_trials: 20,
        }
    }
}

/// One demodulated burst.
#[derive(Clone, Debug)]
pub struct FskBurst {
    /// Demodulator version.
    pub demod_version: String,
    /// The detection box.
    pub request: SnippetRequest,
    /// Source rate, Hz.
    pub source_rate_hz: f64,
    /// Tuned centre, Hz.
    pub tuned_center_hz: f64,
    /// Snippet centre offset from the tuned centre, Hz.
    pub channel_offset_hz: f64,
    /// Timing anchor.
    pub anchor: SampleTime,
    /// Snippet rate, Hz.
    pub snippet_rate_hz: f64,
    /// C13.
    pub params: ParameterSet,
    /// C14.
    pub blind: SymbolParameters,
    /// Seed and trials.
    pub seed: DemodSeed,
    /// Symbols (`None`: every trial failed).
    pub symbols: Option<FskSymbols>,
}

impl FskBurst {
    /// Hard bits (empty when nothing was demodulated).
    pub fn bits(&self) -> &[u8] {
        self.symbols.as_ref().map_or(&[], |s| &s.bits)
    }

    /// Host time of a source index.
    pub fn timestamp_of_source(&self, index: f64) -> Timestamp {
        let dt = (index - self.anchor.sample_index as f64) / self.source_rate_hz;
        self.anchor
            .host_time
            .saturating_add_nanos((dt * 1e9).round() as i64)
    }

    /// Time of symbol `k` (or the box start).
    pub fn timestamp_of_symbol(&self, k: usize) -> Timestamp {
        self.symbols
            .as_ref()
            .and_then(|s| s.source_index.get(k).copied())
            .map_or_else(
                || self.timestamp_of_source(self.request.start_index as f64),
                |i| self.timestamp_of_source(i),
            )
    }

    /// Box time span.
    pub fn time_range(&self) -> TimeRange {
        TimeRange::new(
            self.timestamp_of_source(self.request.start_index as f64),
            self.timestamp_of_source(self.request.end_index as f64),
        )
    }

    /// RF centre (receiver frame), Hz.
    pub fn rf_center_hz(&self) -> f64 {
        let off = self.symbols.as_ref().map_or_else(
            || self.params.cfo_hz.value().unwrap_or(0.0),
            FskSymbols::cfo_hz,
        );
        self.tuned_center_hz + self.channel_offset_hz + off
    }

    /// Whether this burst's two-level alphabet is a **measurement** or only the demodulator's
    /// output (T-614), with no framing evidence. See [`FskBurst::alphabet_evidence_framed`].
    pub fn alphabet_evidence(&self) -> AlphabetEvidence {
        self.alphabet_evidence_framed(FrameEvidence::default())
    }

    /// Whether this burst's two-level alphabet is a **measurement** (T-614), given what framing
    /// inference found in its bits.
    ///
    /// A 2-FSK demodulator *always* returns bits — that is what it is for — so bits existing is
    /// no evidence that the emission has a symbol alphabet. Analogue FM demodulated at a
    /// standard-rate trial returns bits too, and before T-614 every such burst was stored as
    /// `mod_order: 2` with a rate and a deviation. The alphabet counts as measured only when
    /// something independent of the trial agrees, in this order:
    ///
    /// 1. a CRC checked on this burst's frame, or a sync prior was confirmed in its bits;
    /// 2. **veto**: the bits are periodic ([`periodic_bits`]) — a tone-modulated carrier
    ///    sampled at any rate yields a repeating pattern, as does an unmodulated carrier, while
    ///    data does not;
    /// 3. framing found the learned sync word in this burst;
    /// 4. the rate was C14's trusted clock, or a cluster prior built from trusted C14 rates.
    ///
    /// Anything else — the best-lock standard-rate trial nothing confirmed — abstains.
    pub fn alphabet_evidence_framed(&self, framed: FrameEvidence) -> AlphabetEvidence {
        let Some(s) = self.symbols.as_ref() else {
            return AlphabetEvidence::Abstained("nothing demodulated");
        };
        if framed.crc_valid {
            return AlphabetEvidence::Measured("frame CRC valid");
        }
        if self.seed.sync_confirmed == Some(true) {
            return AlphabetEvidence::Measured("sync prior confirmed");
        }
        if periodic_bits(&s.bits).is_some_and(|(_, r)| r >= PERIODIC_BITS_MIN_CORR) {
            return AlphabetEvidence::Abstained(
                "periodic bits: a modulating waveform, not symbols",
            );
        }
        if framed.sync_found {
            return AlphabetEvidence::Measured("framing sync found");
        }
        match self.seed.source {
            SeedSource::TrustedC14 => AlphabetEvidence::Measured("trusted C14 clock"),
            SeedSource::ClusterPrior { .. } => AlphabetEvidence::Measured("cluster prior"),
            SeedSource::StandardRate => {
                AlphabetEvidence::Abstained("unconfirmed standard-rate trial")
            }
        }
    }

    /// Data-model parameters (docs/07 §2.14), with no framing evidence. See
    /// [`FskBurst::estimated_params_framed`].
    pub fn estimated_params(&self) -> EstimatedParams {
        self.estimated_params_framed(FrameEvidence::default())
    }

    /// Data-model parameters (docs/07 §2.14). The symbol rate, deviation and `mod_order` are
    /// reported only when [`FskBurst::alphabet_evidence_framed`] says the alphabet was measured;
    /// otherwise they are `None` (measured values only, never a default — T-614), and the CFO
    /// falls back to C13's, which is a measurement of the carrier, not of symbols.
    pub fn estimated_params_framed(&self, framed: FrameEvidence) -> EstimatedParams {
        let s = self
            .symbols
            .as_ref()
            .filter(|_| self.alphabet_evidence_framed(framed).is_measured());
        EstimatedParams {
            symbol_rate_hz: s.map(|s| s.lock.tracked_rate_bd),
            deviation_hz: s.and_then(|s| s.deviation_hz),
            cfo_hz: s
                .map(FskSymbols::cfo_hz)
                .or_else(|| self.params.cfo_hz.value()),
            mod_order: s.map(|_| 2),
            roll_off: None,
            bandwidth_hz: self.params.obw99_hz.value(),
            pilot_hz: None,
        }
    }
}

/// What framing inference found in one burst (T-614), for [`FskBurst::alphabet_evidence_framed`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameEvidence {
    /// The learned sync word was located in the burst's bits.
    pub sync_found: bool,
    /// The frame's CRC checked.
    pub crc_valid: bool,
}

/// Whether a burst's symbol alphabet was measured (T-614).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlphabetEvidence {
    /// Measured; the evidence.
    Measured(&'static str),
    /// Not measured; why the estimator abstains.
    Abstained(&'static str),
}

impl AlphabetEvidence {
    /// `true` for [`AlphabetEvidence::Measured`].
    pub fn is_measured(&self) -> bool {
        matches!(self, AlphabetEvidence::Measured(_))
    }
}

/// Longest lag [`periodic_bits`] searches, symbols.
///
/// **A priori.** A sinusoid sampled at any rate, sliced to a sign, repeats to within `1/(L+1)` of
/// a cycle at some lag `L ≤ PERIODIC_BITS_MAX_LAG` (Dirichlet), which makes its sign sequence
/// agree with itself at that lag on `1 − 2/(L+1)` of symbols: ≥ 0.97 at 64. Data does not repeat.
pub const PERIODIC_BITS_MAX_LAG: usize = 64;

/// Least |autocorrelation| of the ±1 bit sequence, at the best lag, that marks the bits periodic.
///
/// **A priori.** For random data the autocorrelation at each lag has standard deviation
/// `1/√N`; over ≥ 4 × 64 bits and 64 lags its maximum is ~0.25. A framed packet's periodic part is
/// its preamble, a fraction of the burst (a quarter for a 32-bit preamble on a 128-bit frame), so
/// 0.8 leaves every real packet shape clear while a tone (≥ 0.97 noiseless) and an unmodulated
/// carrier (1.0) exceed it.
pub const PERIODIC_BITS_MIN_CORR: f64 = 0.8;

/// The lag and |autocorrelation| of the most self-similar lag of `bits` in
/// `1..=`[`PERIODIC_BITS_MAX_LAG`], using only lags with at least three periods' worth of
/// overlap. `None` when fewer than 64 bits.
pub fn periodic_bits(bits: &[u8]) -> Option<(usize, f64)> {
    if bits.len() < 64 {
        return None;
    }
    let max_lag = PERIODIC_BITS_MAX_LAG.min(bits.len() / 4);
    (1..=max_lag)
        .map(|lag| {
            let n = bits.len() - lag;
            let agree = bits[lag..].iter().zip(bits).filter(|(a, b)| a == b).count();
            (lag, ((2 * agree) as f64 / n as f64 - 1.0).abs())
        })
        .max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0)))
}

/// The receiver.
pub struct FskReceiver {
    config: FskReceiverConfig,
    c13: ParamEstimator,
    c14: BlindEstimator,
    demod: FskDemod,
}

impl Default for FskReceiver {
    fn default() -> Self {
        Self::new(FskReceiverConfig::default())
    }
}

impl FskReceiver {
    /// A receiver with `config`.
    pub fn new(config: FskReceiverConfig) -> Self {
        Self {
            c13: ParamEstimator::default(),
            c14: BlindEstimator::new(config.blind.clone()),
            demod: FskDemod::new(config.demod.clone()),
            config,
        }
    }

    /// Settings.
    pub fn config(&self) -> &FskReceiverConfig {
        &self.config
    }

    /// Demodulates the burst in `request`.
    ///
    /// Trials, in order: the trusted C14 rate (if any and `use_trusted`), each
    /// [`ClusterPrior`], then the plausible [`STANDARD_RATES_BD`] (≥ `min_sps` samples per
    /// symbol, rate within box bandwidth / 50 … 3 × box bandwidth). With a sync prior the
    /// first trial whose bits contain the sync is chosen; without one the trusted C14 trial,
    /// else the best lock quality (unconfirmed). The chosen seed and all trials are reported.
    pub fn run<T: IqSample>(
        &mut self,
        info: InputInfo<'_>,
        iq: &[T],
        request: &SnippetRequest,
        priors: &DemodPriors,
    ) -> Result<FskBurst, DemodError> {
        let base = info.time.sample_index;
        let end = base + iq.len() as u64;
        if request.start_index < base
            || request.end_index > end
            || request.end_index <= request.start_index
        {
            return Err(DemodError::InvalidRequest(format!(
                "samples {}..{} not inside {base}..{end}",
                request.start_index, request.end_index
            )));
        }
        let source_rate_hz = info.provenance.tune.sample_rate_hz;
        let tuned_center_hz = info.provenance.tune.center_hz;
        let mut extractor = SnippetExtractor::new(SnippetConfig {
            min_rate_hz: self
                .config
                .snippet
                .min_rate_hz
                .max(self.config.rate_factor * request.bandwidth_hz),
            ..self.config.snippet
        });
        let snip = extractor.extract(info, iq, request)?;
        let hints = Hints {
            family: FamilyHint::Fsk { levels: 2 },
            ..self.config.hints
        };
        let params = self.c13.estimate(&snip, &hints);
        let blind = self.c14.estimate_snippet(&snip, &params);
        let trusted = blind
            .symbol_rate_bd
            .value()
            .filter(|_| self.config.use_trusted);
        let c14_dev = blind.deviation_hz.value();
        let cfo_measured = blind
            .cfo_fsk_levels
            .value()
            .or_else(|| params.cfo_hz.value());
        let range = params
            .extent
            .map_or(snip.box_range.clone(), |e| e.start..e.end);

        let mut plan: Vec<(f64, Option<f64>, SeedSource, Option<&ClusterPrior>)> = Vec::new();
        if let Some(r) = trusted {
            plan.push((r, c14_dev, SeedSource::TrustedC14, None));
        }
        for (index, c) in priors.cluster.iter().enumerate() {
            plan.push((
                c.rate_bd,
                c.deviation_hz,
                SeedSource::ClusterPrior {
                    index,
                    source: c.source.clone(),
                },
                Some(c),
            ));
        }
        if priors.standard_rates {
            let bw = request.bandwidth_hz;
            for &r in STANDARD_RATES_BD {
                let sps = snip.sample_rate_hz / r;
                // Weak boxes cover only the strongest part of the spectrum: allow 3 × box.
                if sps >= self.config.demod.min_sps && r <= 3.0 * bw && r >= bw / 50.0 {
                    plan.push((r, None, SeedSource::StandardRate, None));
                }
            }
        }
        let mut seen: Vec<f64> = Vec::new();
        plan.retain(|p| {
            let dup = seen.iter().any(|&r| (p.0 / r - 1.0).abs() < 0.005);
            if !dup {
                seen.push(p.0);
            }
            !dup
        });
        plan.truncate(self.config.max_trials);

        let mut trials = Vec::new();
        let mut chosen: Option<(usize, FskSymbols, Option<f64>, bool)> = None;
        let mut best_unconfirmed: Option<(usize, FskSymbols, Option<f64>)> = None;
        for (ti, (rate, dev, source, cluster)) in plan.iter().enumerate() {
            let dev = dev.or(c14_dev);
            let cfo = cfo_measured.unwrap_or_else(|| raster_cfo(cluster, &snip).unwrap_or(0.0));
            let req = FskDemodRequest {
                rate_bd: *rate,
                deviation_hz: dev,
                cfo_hz: cfo,
                range: range.clone(),
                bandwidth_hz: Some(request.bandwidth_hz),
            };
            match self.demod.demodulate(&snip, &req) {
                Ok(sy) => {
                    let sync_found = priors.sync.as_ref().map(|p| {
                        locate_sync(&sy.bits, &p.bits, p.max_errors, p.min_preamble_bits, None)
                            .is_some()
                    });
                    trials.push(TrialOutcome {
                        rate_bd: *rate,
                        source: source.clone(),
                        sync_found,
                        lock_quality: sy.lock.lock_quality,
                        error: None,
                    });
                    match sync_found {
                        Some(true) => {
                            chosen = Some((ti, sy, dev, true));
                            break;
                        }
                        None if *source == SeedSource::TrustedC14 => {
                            chosen = Some((ti, sy, dev, false));
                            break;
                        }
                        _ => {
                            let better = best_unconfirmed.as_ref().is_none_or(|(bi, b, _)| {
                                // A trusted C14 trial outranks any prior when nothing confirms.
                                plan[*bi].2 != SeedSource::TrustedC14
                                    && (*source == SeedSource::TrustedC14
                                        || sy.lock.lock_quality > b.lock.lock_quality)
                            });
                            if better {
                                best_unconfirmed = Some((ti, sy, dev));
                            }
                        }
                    }
                }
                Err(e) => trials.push(TrialOutcome {
                    rate_bd: *rate,
                    source: source.clone(),
                    sync_found: None,
                    lock_quality: 0.0,
                    error: Some(e.to_string()),
                }),
            }
        }
        let (mut seed, mut symbols) = match chosen {
            Some((ti, sy, dev, confirmed)) => (
                DemodSeed {
                    source: plan[ti].2.clone(),
                    rate_bd: plan[ti].0,
                    deviation_hz: dev,
                    c14_trusted: trusted.is_some(),
                    sync_confirmed: priors.sync.as_ref().map(|_| confirmed),
                    harmonic_divisor: None,
                    trials,
                },
                Some(sy),
            ),
            None => match best_unconfirmed {
                Some((ti, sy, dev)) => (
                    DemodSeed {
                        source: plan[ti].2.clone(),
                        rate_bd: plan[ti].0,
                        deviation_hz: dev,
                        c14_trusted: trusted.is_some(),
                        sync_confirmed: priors.sync.as_ref().map(|_| false),
                        harmonic_divisor: None,
                        trials,
                    },
                    Some(sy),
                ),
                None => (
                    DemodSeed {
                        source: plan
                            .first()
                            .map_or(SeedSource::StandardRate, |p| p.2.clone()),
                        rate_bd: plan.first().map_or(f64::NAN, |p| p.0),
                        deviation_hz: None,
                        c14_trusted: trusted.is_some(),
                        sync_confirmed: priors.sync.as_ref().map(|_| false),
                        harmonic_divisor: None,
                        trials,
                    },
                    None,
                ),
            },
        };
        // Harmonic check on an unconfirmed choice: runs all ≈ multiples of k → rate / k.
        if seed.sync_confirmed != Some(true) {
            if let Some(k) = symbols.as_ref().and_then(|s| run_length_divisor(&s.bits)) {
                let rate = seed.rate_bd / k as f64;
                let req = FskDemodRequest {
                    rate_bd: rate,
                    deviation_hz: seed.deviation_hz,
                    cfo_hz: symbols.as_ref().map_or(0.0, |s| s.cfo_applied_hz),
                    range: range.clone(),
                    bandwidth_hz: Some(request.bandwidth_hz),
                };
                if let Ok(sy) = self.demod.demodulate(&snip, &req) {
                    let sync_found = priors.sync.as_ref().map(|p| {
                        locate_sync(&sy.bits, &p.bits, p.max_errors, p.min_preamble_bits, None)
                            .is_some()
                    });
                    seed.trials.push(TrialOutcome {
                        rate_bd: rate,
                        source: seed.source.clone(),
                        sync_found,
                        lock_quality: sy.lock.lock_quality,
                        error: None,
                    });
                    if single_run_fraction(&sy.bits) >= 0.2 || sync_found == Some(true) {
                        seed.rate_bd = rate;
                        seed.harmonic_divisor = Some(k);
                        if sync_found.is_some() {
                            seed.sync_confirmed = sync_found;
                        }
                        symbols = Some(sy);
                    }
                }
            }
        }
        Ok(FskBurst {
            demod_version: FSK_DEMOD_VERSION.into(),
            request: *request,
            source_rate_hz,
            tuned_center_hz,
            channel_offset_hz: snip.center_offset_hz,
            anchor: snip.time.time,
            snippet_rate_hz: snip.sample_rate_hz,
            params,
            blind,
            seed,
            symbols,
        })
    }
}

/// CFO from a cluster raster when C13 abstained: nearest raster channel minus the snippet
/// centre.
fn raster_cfo(cluster: &Option<&ClusterPrior>, snip: &hk_estimate::ChannelSnippet) -> Option<f64> {
    let c = cluster.as_ref()?;
    let (raster, origin) = (c.raster_hz?, c.raster_origin_hz?);
    let centre = snip.rf_center_hz();
    let channel = origin + ((centre - origin) / raster).round() * raster;
    Some(channel - centre)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg_bits(n: usize, mut x: u64) -> Vec<u8> {
        (0..n)
            .map(|_| {
                x = x
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                (x >> 63) as u8
            })
            .collect()
    }

    /// T-614: a sinusoid sliced at any rate repeats; random data does not.
    #[test]
    fn a_sliced_tone_is_periodic_at_any_rate_and_data_is_not() {
        for ratio in [2.0, 2.4, 4.8, 9.6, 3.3, 7.77] {
            let bits: Vec<u8> = (0..1000)
                .map(|k| u8::from((std::f64::consts::TAU * k as f64 / ratio + 0.3).sin() > 0.0))
                .collect();
            let (lag, r) = periodic_bits(&bits).unwrap();
            assert!(
                r >= PERIODIC_BITS_MIN_CORR,
                "ratio {ratio}: lag {lag} r {r}"
            );
        }
        for seed in 0..20 {
            let bits = lcg_bits(150, seed);
            let (lag, r) = periodic_bits(&bits).unwrap();
            assert!(r < 0.5, "seed {seed}: lag {lag} r {r}");
        }
        // A 32-bit alternating preamble ahead of 112 random bits stays clear of the veto.
        let mut framed: Vec<u8> = (0..32).map(|i| (i % 2) as u8).collect();
        framed.extend(lcg_bits(112, 7));
        assert!(periodic_bits(&framed).unwrap().1 < PERIODIC_BITS_MIN_CORR);
        // An unmodulated carrier (every bit equal) is periodic; too few bits is no answer.
        assert_eq!(periodic_bits(&[1; 200]).map(|p| p.1), Some(1.0));
        assert_eq!(periodic_bits(&[1; 63]), None);
    }
}
