//! Analog auto-mode selection (C19; docs/04 §6.1). There is **no manual mode input**: the mode
//! is inferred from the C13 [`ParameterSet`] of a channel snippet plus two measurements taken
//! here, and every decision carries its confidence, features and the candidates it weighed.
//!
//! Features (T-065 reworked them after a blind synthetic sweep; see
//! `tests/signal_062_mode_sweep.rs` and [`measure`]):
//! - **OBW99**, CFO, noise density N0 and the x² line from C13.
//! - **Line SNR and line fraction** ρ = P_line / P_signal from a box PSD: a carrier is ≈ 1, tone
//!   AM `1/(1 + m²/2)`, FM small, SSB ≈ 0. A line with ρ ≥ 0.5 and ≥ 15 dB SNR makes the signal
//!   *carrier-dominant*.
//! - **Envelope variation** `κ_s − 1`, where `κ_s = E|s|⁴ / (E|s|²)²` of the signal after a
//!   channel filter (±max(0.6·OBW, 3.5 kHz) around the line or CFO), with the noise removed using
//!   C13's N0: `E|s|⁴ = m₄ − 4SN − 2N²`, `S = m₂ − N`. 0 for a constant envelope (FM); ≈ 0.4 for
//!   AM with 50 % tone modulation.
//! - For carrier-dominant signals, a zero-phase **carrier reference** splits the sidebands into
//!   in-phase (AM) and quadrature (FM/PM) power. `var I − var Q` is noise-free: its t-statistic,
//!   the **I/Q balance**, the tone-equivalent **AM depth** and the **sideband-to-carrier ratio**
//!   are recorded. **On/off keying** of the carrier reference separates keyed CW.
//! - **19 kHz pilot** from a trial quadrature discriminator on wide channels ([`tone_frequency`]
//!   in 18.9–19.1 kHz with a significance test).
//!
//! Rules (thresholds tuned on the T-065 synthetic sweep, still to be trained on captures):
//! - **WFM:** OBW ≥ 120 kHz; a pilot confirms it (stereo). Without a pilot, mono WFM needs
//!   OBW ≥ 150 kHz and a constant envelope, at lower confidence. A lightly modulated station
//!   (quiet programme, test tones: the T-023 synthetic reads OBW99 ≈ 97 kHz) is still WFM when
//!   OBW ≥ 50 kHz, the envelope is constant and the trial discriminator finds the pilot; nothing
//!   else puts a significant 19 kHz line in an FM discriminator's output.
//! - **CW:** carrier-dominant, OBW ≤ 500 Hz (or unmeasurable) and either keyed on/off (with
//!   in-phase sidebands), or an unmodulated carrier (no significant in-phase sidebands, sidebands
//!   ≥ 20 dB below the carrier). The decision records the smallest AM depth the test could have
//!   seen; AM shallower than that (10 % speech at 10 dB) reads as a carrier.
//! - **AM:** carrier-dominant with significant in-phase sidebands (t ≥ 6, balance ≥ 0.6, depth
//!   ≥ 5 %). A constant-looking envelope does not veto it: 30 % voice AM has `κ − 1` ≈ 0.02.
//! - **NBFM:** 500 Hz < OBW ≤ 25 kHz with a constant envelope and no in-phase sidebands, or a
//!   carrier-dominant narrow-index FM/PM (quadrature sidebands).
//! - **SSB:** no dominant carrier, a strongly varying envelope, 300 Hz ≤ OBW ≤ 4 kHz and no x²
//!   line (DSB-SC and BPSK have one).
//! - Otherwise, or when C13 finds no occupied band and no dominant carrier (noise), **unknown**.

mod measure;

use hk_estimate::clock::tone_frequency;
use hk_estimate::{ChannelSnippet, ParameterSet};
use num_complex::Complex32;
use serde::{Deserialize, Serialize};

use crate::dsp::Discriminator;
use measure::{Band, Line};

/// Rule-set id and version recorded with every decision.
pub const MODE_RULES_VERSION: &str = "hk-demod/mode-rules@0.2.0";

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
    /// Every mode, in declaration order.
    pub const ALL: [AnalogMode; 6] = [
        AnalogMode::Wfm,
        AnalogMode::Nbfm,
        AnalogMode::Am,
        AnalogMode::Ssb,
        AnalogMode::Cw,
        AnalogMode::Unknown,
    ];

    /// Data-model mode string (`Demodulation.mode`), also the emitter Classification family
    /// `write_session` records. These are modulation names, not services: the pipeline's family
    /// vocabulary (`hk_pipeline::family`, T-039) maps `wfm` to the `fm-broadcast` service and
    /// leaves the shared-use modes (`nbfm`, `am`, `ssb`, `cw`) unmapped.
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
    /// Smallest envelope variation for SSB (speech on a suppressed carrier).
    pub ssb_min_envelope: f64,
    /// Largest line fraction for SSB (no carrier).
    pub ssb_max_line_fraction: f64,
    /// Largest C13 x² line significance for SSB, dB (DSB-SC/BPSK square to a line).
    pub ssb_max_square_line_db: f64,
    /// Envelope variation below this is constant.
    pub constant_envelope_max: f64,
    /// Envelope variation above this is varying.
    pub varying_envelope_min: f64,
    /// Smallest line fraction (line power / signal power) for a carrier-dominant signal.
    pub carrier_min_fraction: f64,
    /// Smallest line SNR for a carrier-dominant signal, dB.
    pub carrier_min_snr_db: f64,
    /// Smallest in-phase sideband t-statistic for AM (its negative for quadrature FM/PM).
    pub am_min_inphase_t: f64,
    /// Smallest I/Q balance for AM.
    pub am_min_iq_balance: f64,
    /// Smallest tone-equivalent AM depth (below it a carrier counts as unmodulated).
    pub am_min_depth: f64,
    /// Largest I/Q balance for a carrier-dominant narrow-index FM/PM.
    pub fm_max_iq_balance: f64,
    /// Largest sideband-to-carrier ratio of an unmodulated carrier, dB.
    pub carrier_max_sideband_db: f64,
    /// Largest in-phase sideband statistic of an unmodulated carrier.
    pub carrier_max_inphase_t: f64,
    /// Largest AM detection floor for a full-confidence unmodulated carrier.
    pub carrier_max_depth_floor: f64,
    /// Smallest keyed-off fraction for keyed CW.
    pub keyed_min_off_fraction: f64,
    /// Smallest keyed "on" SNR over the carrier-reference noise, dB.
    pub keyed_min_on_snr_db: f64,
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
            ssb_min_envelope: 0.5,
            ssb_max_line_fraction: 0.3,
            ssb_max_square_line_db: 20.0,
            constant_envelope_max: 0.1,
            varying_envelope_min: 0.2,
            carrier_min_fraction: 0.5,
            carrier_min_snr_db: 15.0,
            am_min_inphase_t: 6.0,
            am_min_iq_balance: 0.6,
            am_min_depth: 0.05,
            fm_max_iq_balance: -0.3,
            carrier_max_sideband_db: -20.0,
            carrier_max_inphase_t: 3.0,
            carrier_max_depth_floor: 0.15,
            keyed_min_off_fraction: 0.25,
            keyed_min_on_snr_db: 6.0,
            pilot_check_min_obw_hz: 50e3,
            pilot_min_significance_db: 15.0,
            max_feature_samples: 1 << 18,
            min_decide_confidence: 0.35,
        }
    }
}

impl ModeConfig {
    fn carrier_dominant(&self, line: &Line) -> bool {
        line.fraction >= self.carrier_min_fraction && line.snr_db >= self.carrier_min_snr_db
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
    /// Spectral symmetry (C13, about the OBW99 mid-point).
    pub symmetry: Option<f64>,
    /// Spectral flatness.
    pub flatness: Option<f64>,
    /// C13 carrier line offset from the band centre (CFO), Hz.
    pub carrier_line_offset_hz: Option<f64>,
    /// C13 carrier line significance, dB.
    pub carrier_line_db: Option<f64>,
    /// C13 x² line significance, dB.
    pub square_line_db: Option<f64>,
    /// Strongest line in the box PSD, offset from the box centre, Hz.
    pub line_offset_hz: Option<f64>,
    /// That line's power over its noise, dB.
    pub line_snr_db: Option<f64>,
    /// That line's power over the signal power in the box.
    pub line_fraction: Option<f64>,
    /// Half-width of the channel filter behind the envelope and sideband features, Hz.
    pub channel_half_width_hz: Option<f64>,
    /// `κ_s − 1` (see the module docs).
    pub envelope_variation: Option<f64>,
    /// In-phase minus quadrature sideband power over its no-AM standard error (carrier-dominant
    /// signals).
    pub inphase_sideband_t: Option<f64>,
    /// In-phase minus quadrature over total sideband power: +1 AM, −1 narrow-index FM/PM.
    pub iq_balance: Option<f64>,
    /// Tone-equivalent AM depth (modulation index), 0–1+.
    pub am_depth: Option<f64>,
    /// Smallest tone-equivalent AM depth the sideband test could detect in this snippet.
    pub am_depth_floor: Option<f64>,
    /// Sideband power over carrier power, dB (`None` when the sidebands do not clear the noise).
    pub sideband_to_carrier_db: Option<f64>,
    /// Fraction of 5 ms steps with the carrier keyed off.
    pub keyed_off_fraction: Option<f64>,
    /// Keyed "on" level over the carrier-reference noise, dB.
    pub keyed_on_snr_db: Option<f64>,
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
            square_line_db: params.shape.square_line_db,
            ..Default::default()
        };
        let box_samples = &snip.samples[snip.box_range.clone()];
        let x = &box_samples[..box_samples.len().min(c.max_feature_samples)];
        let m = params
            .noise_density
            .value()
            .map(|n0| {
                let band = Band {
                    obw_hz: f.obw99_hz,
                    cfo_hz: cfo.unwrap_or(0.0),
                    n0,
                };
                measure::measure(x, snip.sample_rate_hz, snip.box_bandwidth_hz, band, c)
            })
            .unwrap_or_default();
        f.line_offset_hz = m.line.map(|l| l.offset_hz);
        f.line_snr_db = m.line.map(|l| l.snr_db);
        f.line_fraction = m.line.map(|l| l.fraction);
        f.channel_half_width_hz = m.half_width_hz;
        f.envelope_variation = m.envelope_variation;
        if let Some(k) = m.coherent {
            f.inphase_sideband_t = Some(k.inphase_t);
            f.iq_balance = Some(k.iq_balance);
            f.am_depth = Some(k.am_depth);
            f.am_depth_floor = Some(k.am_depth_floor);
            f.sideband_to_carrier_db = k.sideband_to_carrier_db;
            f.keyed_off_fraction = Some(k.keyed_off_fraction);
            f.keyed_on_snr_db = Some(k.keyed_on_snr_db);
        }
        let carrier = m.line.filter(|l| c.carrier_dominant(l));
        let obw = f.obw99_hz;
        if obw.is_none() && carrier.is_none() {
            return unknown(
                f,
                Vec::new(),
                format!(
                    "no occupied band: C13 OBW99 abstained ({:?}) and no dominant carrier line",
                    params.obw99_hz.reason()
                ),
            );
        }
        if let Some(o) = obw
            && o >= c.pilot_check_min_obw_hz
        {
            f.pilot = Some(self.pilot_check(x, snip.sample_rate_hz));
        }
        let env = f.envelope_variation;
        let constant = env.is_some_and(|v| v <= c.constant_envelope_max);
        let varying = env.is_some_and(|v| v >= c.varying_envelope_min);
        let env_txt = match env {
            Some(v) => format!("envelope variation {v:.3}"),
            None => "envelope variation not measurable".into(),
        };
        let obw_txt = match obw {
            Some(o) => format!("OBW99 {o:.0} Hz"),
            None => "OBW99 not measured (low SNR)".into(),
        };
        // Without OBW99 only the carrier rules run, at reduced confidence.
        let cap = if obw.is_some() { 1.0 } else { 0.6 };
        let mut cands = Vec::new();

        // WFM
        if let Some(obw) = obw {
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
        }

        // Carrier-dominant: keyed CW, AM, unmodulated carrier, narrow-index FM/PM.
        let narrow = obw.is_none_or(|o| o <= c.cw_max_obw_hz);
        let mut am = false;
        if let (Some(line), Some(k)) = (carrier, m.coherent)
            && obw.is_none_or(|o| o < c.wfm_min_obw_hz)
        {
            let line_txt = format!(
                "carrier line {:.0} Hz from the box centre: {:.1} dB, {:.0} % of the signal power",
                line.offset_hz,
                line.snr_db,
                100.0 * line.fraction
            );
            let sb_txt = format!(
                "sidebands: in-phase t {:.1}, I/Q balance {:.2}, AM depth {:.3} (detectable from \
                 {:.3}), sideband/carrier {}",
                k.inphase_t,
                k.iq_balance,
                k.am_depth,
                k.am_depth_floor,
                k.sideband_to_carrier_db
                    .map_or("below noise".into(), |d| format!("{d:.1} dB"))
            );
            // Keying is amplitude modulation: the sidebands must be in phase.
            let keyed = narrow
                && k.keyed_off_fraction >= c.keyed_min_off_fraction
                && k.keyed_on_snr_db >= c.keyed_min_on_snr_db
                && k.iq_balance >= c.am_min_iq_balance;
            am = !keyed
                && k.inphase_t >= c.am_min_inphase_t
                && k.iq_balance >= c.am_min_iq_balance
                && k.am_depth >= c.am_min_depth;
            let quadrature =
                k.inphase_t <= -c.am_min_inphase_t && k.iq_balance <= c.fm_max_iq_balance;
            // No significant in-phase sidebands and little sideband power at all (residual phase
            // noise stays far below the carrier).
            let unmodulated = k.inphase_t < c.carrier_max_inphase_t
                && k.sideband_to_carrier_db
                    .is_none_or(|d| d <= c.carrier_max_sideband_db);
            if keyed {
                cands.push(ModeCandidate {
                    mode: AnalogMode::Cw,
                    confidence: 0.7f64.min(cap),
                    evidence: vec![
                        line_txt,
                        format!(
                            "on/off keyed: off {:.0} % of the time, on {:.1} dB",
                            100.0 * k.keyed_off_fraction,
                            k.keyed_on_snr_db
                        ),
                        obw_txt.clone(),
                    ],
                });
            } else if am {
                let conf = if k.inphase_t >= 2.0 * c.am_min_inphase_t {
                    0.85
                } else {
                    0.6
                };
                cands.push(ModeCandidate {
                    mode: AnalogMode::Am,
                    confidence: f64::min(conf, cap),
                    evidence: vec![
                        line_txt,
                        sb_txt,
                        "in-phase (mirror-symmetric) sidebands: amplitude modulation".into(),
                        env_txt.clone(),
                    ],
                });
            } else if unmodulated && narrow && !am {
                // "Unmodulated" is only as strong as the depth the sideband test could see.
                let conf = if k.am_depth_floor <= c.carrier_max_depth_floor {
                    0.75
                } else {
                    0.5
                };
                cands.push(ModeCandidate {
                    mode: AnalogMode::Cw,
                    confidence: f64::min(conf, cap),
                    evidence: vec![
                        line_txt,
                        sb_txt,
                        format!(
                            "unmodulated carrier: not keyed, no AM deeper than {:.0} %",
                            100.0 * k.am_depth_floor
                        ),
                        obw_txt.clone(),
                    ],
                });
            } else if quadrature
                && constant
                && obw.is_some_and(|o| o > c.cw_max_obw_hz && o <= c.nbfm_max_obw_hz)
            {
                cands.push(ModeCandidate {
                    mode: AnalogMode::Nbfm,
                    confidence: 0.65,
                    evidence: vec![
                        line_txt,
                        sb_txt,
                        "quadrature sidebands: narrow-index FM/PM".into(),
                        env_txt.clone(),
                    ],
                });
            }
        }

        // NBFM
        if let Some(obw) = obw
            && obw > c.cw_max_obw_hz
            && obw <= c.nbfm_max_obw_hz
            && !cands.iter().any(|x| x.mode == AnalogMode::Nbfm)
        {
            let conf = if am {
                0.1
            } else if constant {
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
                    if am {
                        "in-phase sidebands contradict FM".into()
                    } else {
                        "constant-envelope FSK is not excluded (C20 checks the IF histogram)".into()
                    },
                ],
            });
        }

        // SSB
        let no_carrier = m.line.is_none_or(|l| {
            l.fraction <= c.ssb_max_line_fraction || l.snr_db < c.carrier_min_snr_db
        });
        if let Some(obw) = obw
            && (c.ssb_min_obw_hz..=c.ssb_max_obw_hz).contains(&obw)
            && no_carrier
            && env.is_some_and(|v| v >= c.ssb_min_envelope)
            && f.square_line_db
                .is_none_or(|d| d < c.ssb_max_square_line_db)
        {
            let side = match f.symmetry {
                Some(s) if s > 0.0 => "upper sideband likely (voice energy at the low RF edge)",
                Some(s) if s < 0.0 => "lower sideband likely (voice energy at the high RF edge)",
                _ => "sideband undetermined",
            };
            cands.push(ModeCandidate {
                mode: AnalogMode::Ssb,
                confidence: 0.55,
                evidence: vec![
                    format!("{obw_txt}, no carrier line, {env_txt}"),
                    format!(
                        "no x² line ({:?} dB): not DSB-SC or BPSK",
                        f.square_line_db.map(|d| d.round())
                    ),
                    format!("{side} (symmetry {:?})", f.symmetry),
                ],
            });
        }

        // A narrow signal the carrier rules could not place.
        if narrow
            && obw.is_some()
            && !cands
                .iter()
                .any(|x| x.mode == AnalogMode::Cw || x.mode == AnalogMode::Am)
        {
            cands.push(ModeCandidate {
                mode: AnalogMode::Cw,
                confidence: 0.25,
                evidence: vec![
                    format!("narrow, {obw_txt}, but no clean carrier (low confidence)"),
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
            _ => unknown(f, cands, format!("no rule matched confidently ({obw_txt})")),
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
