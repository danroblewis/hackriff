//! Analog auto-mode selection (C19; docs/04 §6.1). There is **no manual mode input**: the mode
//! is inferred from the C13 [`ParameterSet`] of a channel snippet plus two measurements taken
//! here, and every decision carries its confidence, features and the candidates it weighed.
//!
//! Features:
//! - **OBW99** and spectral **symmetry**, the **carrier line** (offset and significance) from
//!   C13.
//! - **Envelope variation** `κ_s − 1`, where `κ_s = E|s|⁴ / (E|s|²)²` of the signal after a
//!   channel filter (±0.6·OBW around the CFO), with the noise contribution removed using C13's
//!   N0: `E|s|⁴ = m₄ − 4SN − 2N²`, `S = m₂ − N`. 0 for a constant envelope (FM); ≈ 0.4 for AM
//!   with 50 % tone modulation.
//! - **19 kHz pilot** from a trial quadrature discriminator on wide channels ([`tone_frequency`]
//!   in 18.9–19.1 kHz with a significance test).
//!
//! Rules (provisional; the card says thresholds must be trained on captures):
//! - **WFM:** OBW ≥ 120 kHz; a pilot confirms it (stereo). Without a pilot, mono WFM needs
//!   OBW ≥ 150 kHz and a constant envelope, at lower confidence. A lightly modulated station
//!   (quiet programme, test tones: the T-023 synthetic reads OBW99 ≈ 97 kHz) is still WFM when
//!   OBW ≥ 50 kHz, the envelope is constant and the trial discriminator finds the pilot; nothing
//!   else puts a significant 19 kHz line in an FM discriminator's output.
//! - **NBFM:** 500 Hz < OBW ≤ 25 kHz and a constant envelope.
//! - **AM:** a significant carrier line at the band centre, symmetric sidebands and a varying
//!   envelope.
//! - **SSB:** |symmetry| ≥ 0.6 with 300 Hz ≤ OBW ≤ 4 kHz; **CW:** OBW < 500 Hz (single line).
//!   Both are marked low confidence.
//! - Otherwise, or when C13 finds no occupied band (noise), **unknown**.

use hk_estimate::clock::tone_frequency;
use hk_estimate::{ChannelSnippet, ParameterSet};
use num_complex::Complex32;
use serde::{Deserialize, Serialize};

use crate::dsp::{Discriminator, FirDecimator, lowpass_taps};

/// Rule-set id and version recorded with every decision.
pub const MODE_RULES_VERSION: &str = "hk-demod/mode-rules@0.1.0";

/// Analog demodulation modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AnalogMode {
    /// Broadcast wideband FM.
    Wfm,
    /// Narrowband FM.
    Nbfm,
    /// Amplitude modulation (with carrier).
    Am,
    /// Single sideband.
    Ssb,
    /// On/off keyed carrier or unmodulated carrier.
    Cw,
    /// Not an analog mode this selector recognises (noise, digital, unclear).
    Unknown,
}

impl AnalogMode {
    /// Data-model mode string (`Demodulation.mode`).
    pub const fn as_str(self) -> &'static str {
        match self {
            AnalogMode::Wfm => "wfm",
            AnalogMode::Nbfm => "nbfm",
            AnalogMode::Am => "am",
            AnalogMode::Ssb => "ssb",
            AnalogMode::Cw => "cw",
            AnalogMode::Unknown => "unknown",
        }
    }
}

/// Mode-selection thresholds.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModeConfig {
    /// Smallest WFM OBW99, Hz.
    pub wfm_min_obw_hz: f64,
    /// Smallest OBW99 for mono WFM without a pilot, Hz.
    pub wfm_mono_min_obw_hz: f64,
    /// Smallest OBW99 for WFM when a pilot is found and the envelope is constant, Hz.
    pub wfm_pilot_min_obw_hz: f64,
    /// Largest NBFM OBW99, Hz.
    pub nbfm_max_obw_hz: f64,
    /// Largest CW OBW99, Hz.
    pub cw_max_obw_hz: f64,
    /// SSB OBW99 range, Hz.
    pub ssb_min_obw_hz: f64,
    /// See `ssb_min_obw_hz`.
    pub ssb_max_obw_hz: f64,
    /// Smallest |symmetry| for SSB.
    pub ssb_min_symmetry: f64,
    /// Largest |symmetry| for AM.
    pub am_max_symmetry: f64,
    /// Envelope variation below this is constant.
    pub constant_envelope_max: f64,
    /// Envelope variation above this is varying.
    pub varying_envelope_min: f64,
    /// AM carrier line: largest distance from the band centre as a fraction of OBW99 (at
    /// least 50 Hz).
    pub am_carrier_max_offset_frac: f64,
    /// Smallest carrier-line significance for AM, dB.
    pub am_carrier_min_db: f64,
    /// OBW99 from which the trial discriminator looks for a pilot, Hz.
    pub pilot_check_min_obw_hz: f64,
    /// Smallest pilot-line significance, dB.
    pub pilot_min_significance_db: f64,
    /// Samples used by the envelope and pilot measurements.
    pub max_feature_samples: usize,
    /// Smallest confidence for a decision other than `unknown`.
    pub min_decide_confidence: f64,
}

impl Default for ModeConfig {
    fn default() -> Self {
        Self {
            wfm_min_obw_hz: 120e3,
            wfm_mono_min_obw_hz: 150e3,
            wfm_pilot_min_obw_hz: 50e3,
            nbfm_max_obw_hz: 25e3,
            cw_max_obw_hz: 500.0,
            ssb_min_obw_hz: 300.0,
            ssb_max_obw_hz: 4_000.0,
            ssb_min_symmetry: 0.6,
            am_max_symmetry: 0.3,
            constant_envelope_max: 0.1,
            varying_envelope_min: 0.2,
            am_carrier_max_offset_frac: 0.05,
            am_carrier_min_db: 15.0,
            pilot_check_min_obw_hz: 50e3,
            pilot_min_significance_db: 15.0,
            max_feature_samples: 1 << 18,
            min_decide_confidence: 0.35,
        }
    }
}

/// A trial-discriminator pilot measurement.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PilotCheck {
    /// Measured tone, Hz (`None` if no significant line).
    pub frequency_hz: Option<f64>,
    /// Line significance, dB.
    pub significance_db: Option<f64>,
    /// Accepted as a stereo pilot.
    pub found: bool,
}

/// Features the decision used.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModeFeatures {
    /// OBW99, Hz.
    pub obw99_hz: Option<f64>,
    /// Box SNR, dB.
    pub snr_db: Option<f64>,
    /// Spectral symmetry.
    pub symmetry: Option<f64>,
    /// Spectral flatness.
    pub flatness: Option<f64>,
    /// Carrier line offset from the band centre (CFO), Hz.
    pub carrier_line_offset_hz: Option<f64>,
    /// Carrier line significance, dB.
    pub carrier_line_db: Option<f64>,
    /// `κ_s − 1` (see the module docs).
    pub envelope_variation: Option<f64>,
    /// Trial-discriminator pilot check, when run.
    pub pilot: Option<PilotCheck>,
}

/// One mode considered.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModeCandidate {
    /// Mode.
    pub mode: AnalogMode,
    /// Confidence, 0–1.
    pub confidence: f64,
    /// Why, in words.
    pub evidence: Vec<String>,
}

/// The selected mode with its evidence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModeDecision {
    /// Selected mode (`Unknown` when no candidate is confident enough).
    pub mode: AnalogMode,
    /// Confidence in `mode`, 0–1.
    pub confidence: f64,
    /// Features measured.
    pub features: ModeFeatures,
    /// Candidates weighed, most confident first.
    pub candidates: Vec<ModeCandidate>,
    /// Why `unknown`, when it is.
    pub reason: Option<String>,
    /// Rule-set version.
    pub rules_version: String,
}

/// The rule-based mode selector.
#[derive(Clone, Debug, Default)]
pub struct ModeSelector {
    config: ModeConfig,
}

impl ModeSelector {
    /// A selector.
    pub fn new(config: ModeConfig) -> Self {
        Self { config }
    }

    /// Selects a mode for a snippet and its C13 estimate.
    pub fn select(&self, snip: &ChannelSnippet, params: &ParameterSet) -> ModeDecision {
        let c = &self.config;
        let cfo = params.cfo_hz.value();
        let mut f = ModeFeatures {
            obw99_hz: params.obw99_hz.value(),
            snr_db: params.snr_box_db.value(),
            symmetry: params.shape.symmetry,
            flatness: params.shape.flatness,
            carrier_line_offset_hz: params
                .shape
                .carrier_line
                .value()
                .map(|v| v - cfo.unwrap_or(0.0)),
            carrier_line_db: params.shape.carrier_line.evidence().significance_db,
            ..Default::default()
        };
        let Some(obw) = f.obw99_hz else {
            return unknown(
                f,
                Vec::new(),
                format!(
                    "no occupied band: C13 OBW99 abstained ({:?})",
                    params.obw99_hz.reason()
                ),
            );
        };
        let box_samples = &snip.samples[snip.box_range.clone()];
        let x = &box_samples[..box_samples.len().min(c.max_feature_samples)];
        f.envelope_variation = envelope_variation(
            x,
            snip.sample_rate_hz,
            obw,
            cfo.unwrap_or(0.0),
            params.noise_density.value(),
        );
        if obw >= c.pilot_check_min_obw_hz {
            f.pilot = Some(self.pilot_check(x, snip.sample_rate_hz));
        }
        let env = f.envelope_variation;
        let constant = env.is_some_and(|v| v <= c.constant_envelope_max);
        let varying = env.is_some_and(|v| v >= c.varying_envelope_min);
        let env_txt = match env {
            Some(v) => format!("envelope variation {v:.3}"),
            None => "envelope variation not measurable".into(),
        };
        let mut cands = Vec::new();

        // WFM
        let pilot = f.pilot.filter(|p| p.found);
        let wide = obw >= c.wfm_min_obw_hz;
        if wide || (obw >= c.wfm_pilot_min_obw_hz && pilot.is_some() && constant) {
            let mut ev = vec![if wide {
                format!(
                    "OBW99 {:.0} kHz ≥ {:.0} kHz",
                    obw / 1e3,
                    c.wfm_min_obw_hz / 1e3
                )
            } else {
                format!(
                    "OBW99 {:.0} kHz (lightly modulated; accepted on the pilot and a constant \
                     envelope)",
                    obw / 1e3
                )
            }];
            ev.push(env_txt.clone());
            let conf = if let Some(p) = pilot {
                ev.push(format!(
                    "19 kHz pilot at {:.2} Hz ({:.1} dB) after a trial discriminator",
                    p.frequency_hz.unwrap_or(f64::NAN),
                    p.significance_db.unwrap_or(f64::NAN)
                ));
                if varying { 0.8 } else { 0.95 }
            } else {
                ev.push("no 19 kHz pilot (mono or not broadcast FM)".into());
                match (obw >= c.wfm_mono_min_obw_hz, constant) {
                    (true, true) => 0.65,
                    (true, false) if !varying => 0.45,
                    _ => 0.25,
                }
            };
            cands.push(ModeCandidate {
                mode: AnalogMode::Wfm,
                confidence: conf,
                evidence: ev,
            });
        }

        // NBFM
        if obw > c.cw_max_obw_hz && obw <= c.nbfm_max_obw_hz {
            let conf = if constant {
                0.75
            } else if varying {
                0.1
            } else {
                0.3
            };
            cands.push(ModeCandidate {
                mode: AnalogMode::Nbfm,
                confidence: conf,
                evidence: vec![
                    format!(
                        "OBW99 {:.1} kHz ≤ {:.0} kHz",
                        obw / 1e3,
                        c.nbfm_max_obw_hz / 1e3
                    ),
                    env_txt.clone(),
                    "constant-envelope FSK is not excluded (C20 checks the IF histogram)".into(),
                ],
            });
        }

        // AM
        let centred = f
            .carrier_line_offset_hz
            .is_some_and(|o| o.abs() <= (c.am_carrier_max_offset_frac * obw).max(50.0))
            && f.carrier_line_db.is_some_and(|d| d >= c.am_carrier_min_db);
        let symmetric = f.symmetry.is_some_and(|s| s.abs() <= c.am_max_symmetry);
        if centred && obw <= c.wfm_min_obw_hz {
            let conf = match (symmetric, varying, constant) {
                (true, true, _) => 0.85,
                (true, false, false) => 0.4,
                (_, _, true) => 0.1,
                _ => 0.25,
            };
            cands.push(ModeCandidate {
                mode: AnalogMode::Am,
                confidence: conf,
                evidence: vec![
                    format!(
                        "carrier line {:.1} Hz from centre ({:.1} dB)",
                        f.carrier_line_offset_hz.unwrap_or(f64::NAN),
                        f.carrier_line_db.unwrap_or(f64::NAN)
                    ),
                    format!("symmetry {:?}", f.symmetry),
                    env_txt.clone(),
                ],
            });
        }

        // SSB
        if let Some(s) = f.symmetry
            && s.abs() >= c.ssb_min_symmetry
            && (c.ssb_min_obw_hz..=c.ssb_max_obw_hz).contains(&obw)
        {
            cands.push(ModeCandidate {
                mode: AnalogMode::Ssb,
                confidence: 0.4,
                evidence: vec![
                    format!("asymmetric spectrum (symmetry {s:.2}), OBW99 {obw:.0} Hz"),
                    format!(
                        "{} sideband (low confidence)",
                        if s > 0.0 { "lower" } else { "upper" }
                    ),
                ],
            });
        }

        // CW
        if obw <= c.cw_max_obw_hz {
            cands.push(ModeCandidate {
                mode: AnalogMode::Cw,
                confidence: 0.4,
                evidence: vec![
                    format!("single narrow line, OBW99 {obw:.0} Hz (low confidence)"),
                    env_txt,
                ],
            });
        }

        cands.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
        match cands.first() {
            Some(best) if best.confidence >= c.min_decide_confidence => ModeDecision {
                mode: best.mode,
                confidence: best.confidence,
                features: f,
                candidates: cands.clone(),
                reason: None,
                rules_version: MODE_RULES_VERSION.into(),
            },
            _ => unknown(
                f,
                cands,
                format!("no rule matched confidently (OBW99 {obw:.0} Hz)"),
            ),
        }
    }

    fn pilot_check(&self, x: &[Complex32], fs: f64) -> PilotCheck {
        let mut disc = Discriminator::new(fs);
        let mpx: Vec<f32> = x.iter().map(|&s| disc.push(s)).skip(1).collect();
        let est = tone_frequency(&mpx, fs, 18_900.0, 19_100.0);
        let sig = est.evidence().significance_db;
        let freq = est.value();
        PilotCheck {
            frequency_hz: freq,
            significance_db: sig,
            found: freq.is_some()
                && sig.is_some_and(|s| s >= self.config.pilot_min_significance_db),
        }
    }
}

fn unknown(features: ModeFeatures, candidates: Vec<ModeCandidate>, reason: String) -> ModeDecision {
    let best = candidates.first().map_or(0.0, |c| c.confidence);
    ModeDecision {
        mode: AnalogMode::Unknown,
        confidence: 1.0 - best,
        features,
        candidates,
        reason: Some(reason),
        rules_version: MODE_RULES_VERSION.into(),
    }
}

/// `κ_s − 1` of `x` (see the module docs), or `None` when the signal does not clear the noise
/// or the filter cannot be built.
fn envelope_variation(
    x: &[Complex32],
    fs: f64,
    obw_hz: f64,
    cfo_hz: f64,
    n0: Option<f64>,
) -> Option<f64> {
    let n0 = n0?;
    let stop = (0.9 * obw_hz).min(0.49 * fs);
    let pass = (0.6 * obw_hz).min(stop - fs / 400.0);
    if pass <= 0.0 {
        return None;
    }
    let taps = lowpass_taps(fs, pass, stop, 50.0).ok()?;
    let sum: f64 = taps.iter().map(|&t| f64::from(t)).sum();
    let sq: f64 = taps.iter().map(|&t| f64::from(t).powi(2)).sum();
    let noise = n0 * fs * sq / (sum * sum);
    let skip = taps.len();
    if x.len() <= 4 * skip {
        return None;
    }
    let mut fir = FirDecimator::<Complex32>::new(taps, 1);
    let step = -cfo_hz / fs;
    let (mut m2, mut m4, mut cnt) = (0.0f64, 0.0f64, 0u64);
    for (n, &s) in x.iter().enumerate() {
        let ph = std::f64::consts::TAU * (step * n as f64).fract();
        let y = fir.push(s * Complex32::new(ph.cos() as f32, ph.sin() as f32))?;
        if n >= skip {
            let p = f64::from(y.norm_sqr());
            m2 += p;
            m4 += p * p;
            cnt += 1;
        }
    }
    let (m2, m4) = (m2 / cnt as f64, m4 / cnt as f64);
    let s = m2 - noise;
    if s <= noise {
        return None;
    }
    let e4 = m4 - 4.0 * s * noise - 2.0 * noise * noise;
    Some(e4 / (s * s) - 1.0)
}
