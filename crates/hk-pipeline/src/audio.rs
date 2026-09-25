//! **The chooser (ADR-0015 §12.2, LP-5 = T-869): probe → mode → *which* pipeline.**
//!
//! "Listen is just a decode pipeline with an audio sink" hides a third thing beside the recipe
//! and the opener: *something* has to decide what to run, because the user never picks a mode
//! (the product rule: modulation, bandwidth and squelch are estimated from the signal). That
//! decision is this module, named and owned rather than left implicit — if it stays implicit,
//! "no manual mode" degrades into "pick a recipe by hand" (§12.10 objection 1).
//!
//! [`choose`] is a **pure function of what the probe measured**. It reads no ring, tunes nothing
//! and starts nothing: the caller (`chains/listen.rs`) probes the live edge, hands the result
//! here, and acts on the answer. That makes the whole decision unit-testable and keeps the one
//! place a mode is chosen honest about its evidence.
//!
//! # The three answers
//!
//! | Answer | Meaning | Who serves it |
//! |---|---|---|
//! | [`Choice::Recipe`] | a recipe whose sink is `audio_out` **fits the measurement** | the recipe runtime, as an ephemeral pipeline (§12.3) |
//! | [`Choice::Legacy`] | an analog mode was recognised, but no recipe fits it | `chains/listen.rs`, unchanged (§12.5) |
//! | [`Choice::Refuse`] | nothing to demodulate | the opener, as `422 no-analog-mode` |
//!
//! `legacy` is a first-class answer, not a fallback to be removed: SSB and CW have no blocks and
//! are **decided** to stay legacy permanently (U4 = A, §12.5), so the two paths coexist by
//! design.
//!
//! # Ranking is over measurements, never over frequency
//!
//! A recipe is offered only when [`hk_recipe::matching::rank`] — the arithmetic behind
//! `GET /api/recipes/match` (ADR-0011 §2.4, T-164) — reads the *measured* family, occupied
//! bandwidth and feature tokens (a measured 19 kHz pilot) as a [`Outcome::Fit`]. A recipe's
//! `freq_hz` carries weight zero there and can only break a tie, so the chooser can never pick a
//! recipe because a band plan says an FM station lives at 100 MHz. A parameter nothing measured
//! earns nothing, so a probe that abstained ranks nothing highly and the answer is `legacy` —
//! the conservative direction, since legacy is the live-tested path.
//!
//! This is exactly a depth-1, budget-1 instance of ADR-0015 §3's search restricted to the analog
//! skeletons, which is why the unification is real; until MAUTO exists it is the existing probe,
//! wrapped, with no new estimation and no new thresholds — with the one deliberate exception
//! below.
//!
//! # The weak-carrier rule (decided here, T-869)
//!
//! Live finding (explorer, 2026-09-25): at 162.4008 MHz — a NOAA weather station, a real,
//! permanently-keyed NBFM carrier at about 5 dB SNR — Listen answered `422 no-analog-mode`
//! ("C13 OBW99 abstained (LowSnr) and no dominant carrier line"). The refusal is *honest* — no
//! mode was recognised — but it is the wrong product answer: every receiver ever built will
//! demodulate a weak NFM channel and let the squelch decide whether you hear it.
//!
//! So the chooser adds one rule, and only one: when **no mode was recognised** but the estimator
//! did measure **energy in the box** ([`BoxEnergy`]) and the user's selection is narrow enough to
//! *be* a narrowband channel ([`WEAK_CARRIER_MAX_SELECTION_HZ`]), demodulate it as NBFM on the
//! selection with the **squelch armed from the measured noise power** — silent until the channel
//! actually rises, never a refusal.
//!
//! **Which measurement, and why not the obvious one.** The first cut of this rule asked for a
//! measured `snr_box_db`, and was **dead code**: that estimate abstains for *any* band reason
//! (`hk_estimate`'s `snr_estimate`), and "no occupied band" is precisely the abstention that
//! produced the refusal, so the rule could never fire on its own trigger case. [`BoxEnergy`]
//! reads what C13 **does** publish when the band estimate gives up: its **presence statistic**
//! (the band stage attaches it to `obw99_hz` as evidence only once the presence test has passed —
//! so its mere existence is the estimator's own "something is here", with no new threshold), the
//! occupied bandwidth the mode selector measured *between adjacent emissions* when C13 abstained
//! (T-099), and the strongest line's SNR. What is *not* weakened:
//!
//! - **Noise stays refused.** Every one of those is a measurement: when the estimator's presence
//!   test fails nothing is attached, nothing is measured, and the answer is still
//!   [`Choice::Refuse`] (LP-1 §6 freezes `4422` on noise, and it stays frozen).
//! - **The stream never claims more than it knows.** The plan carries the probe's own mode
//!   confidence (zero: nothing recognised it) and its rules version, so the header says a mode
//!   was *not* identified while the audio plays. Implying a recognised mode would be the same
//!   defect as implying resolution the front end never captured.
//! - **A wide drag is still refused.** "Somewhere in this megahertz there might be a carrier" is
//!   not a channel, and demodulating one would be inventing a signal.
//!
//! # Stereo asks for legacy (§12.13)
//!
//! A recipe's `audio` output is mono — `audio_out` takes one input, and the recipe side of
//! stereo is LP-9/LP-10's next step. A request that opted into `channels=2` (T-874) therefore
//! chooses `legacy`, which *can* deliver it, rather than a pipeline that would quietly answer
//! mono. Promising stereo and serving mono is the same dishonesty as promising audio beyond the
//! IQ horizon.

use hk_demod::AnalogMode;
use hk_demod::audio::{AudioConfig, AudioPlan, ProbeResult};
use hk_model::EstimatedParams;
use hk_recipe::matching::{self, Entry, MeasuredSignal, Outcome};

/// Smallest measured SNR the weak-carrier rule accepts from the box or from a line, dB. Below it
/// the estimator has not shown that anything is there, and "maybe there is a carrier" is not a
/// signal. It bounds only the estimates that *are* SNRs; the presence statistic carries the
/// estimator's own threshold and is not re-judged here.
pub const WEAK_CARRIER_MIN_SNR_DB: f64 = 3.0;
/// Widest selection the weak-carrier rule accepts, Hz: twice the widest NBFM channel the mode
/// selector would ever plan ([`WEAK_CARRIER_MAX_CHANNEL_HZ`]). A wider drag is a region, not a
/// channel.
pub const WEAK_CARRIER_MAX_SELECTION_HZ: f64 = 50e3;
/// Narrowest channel the weak-carrier rule plans, Hz (`AudioPlan::from_probe`'s NBFM floor).
pub const WEAK_CARRIER_MIN_CHANNEL_HZ: f64 = 6e3;
/// Widest channel the weak-carrier rule plans, Hz (`AudioPlan::from_probe`'s NBFM ceiling).
pub const WEAK_CARRIER_MAX_CHANNEL_HZ: f64 = 25e3;

/// The seed parameters a chosen recipe is started on: the channel the probe (and refinement)
/// measured. The recipe's own `input.bandwidth_hz` still governs its DDC where it declares one;
/// the seed is what the pipeline is *aimed* at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Seed {
    /// Channel centre, RF Hz.
    pub center_hz: f64,
    /// Channel bandwidth, Hz.
    pub bandwidth_hz: f64,
    /// Whether the audio needs an AGC. A property of the **mode**, not of the recipe: FM is
    /// constant-amplitude, so its level is set by deviation and an AGC would only ride the noise
    /// between programme peaks; AM, SSB and CW need one. The recipe's `agc` node is seeded with
    /// it (ADR-0015 §12.2: the chooser returns a recipe *and seed parameters*), so the pipeline
    /// scales the audio the way the legacy chain does for the mode that was measured.
    pub agc: bool,
}

impl Seed {
    /// `[lo, hi]` of the seeded channel, Hz.
    pub fn extent_hz(self) -> (f64, f64) {
        let h = 0.5 * self.bandwidth_hz;
        (self.center_hz - h, self.center_hz + h)
    }
}

/// What the chooser decided (see the module docs).
#[derive(Clone, Debug, PartialEq)]
pub enum Choice {
    /// Run this recipe as an audio pipeline, seeded from the probe.
    Recipe {
        /// Recipe id.
        id: String,
        /// Version ranked (the store's latest).
        version: u32,
        /// The channel to start it on.
        seed: Seed,
        /// The plan the probe produced: what the channel was measured to be, and what today's
        /// chain would run if the pipeline cannot be started after all.
        plan: Box<AudioPlan>,
        /// Its match score, 0–1.
        score: f64,
        /// Why it was chosen, in words.
        why: String,
    },
    /// Run today's chain on this plan (§12.5).
    Legacy {
        /// The demodulation plan.
        plan: Box<AudioPlan>,
        /// Why no recipe was chosen, in words.
        why: String,
        /// The plan is the **weak-carrier rule's**, not a recognised mode: nothing identified
        /// the modulation, and the channel is demodulated with the squelch armed anyway. The
        /// stream must say so — the caller reports mode confidence **zero** and marks the rules
        /// version — because "nbfm at confidence 1" would be a claim nothing measured.
        weak_carrier: bool,
    },
    /// Nothing to demodulate: the opener refuses `422 no-analog-mode` with this reason.
    Refuse {
        /// The probe's own reason.
        why: String,
    },
}

impl Choice {
    /// Whether the answer's plan came from the weak-carrier rule, so nothing recognised the
    /// mode and the stream must not claim one was.
    pub fn weak_carrier(&self) -> bool {
        matches!(
            self,
            Choice::Legacy {
                weak_carrier: true,
                ..
            }
        )
    }

    /// The demodulation plan behind the answer: the channel the probe measured. Only
    /// [`Choice::Refuse`] has none, because nothing was recognised.
    pub fn plan(&self) -> Option<&AudioPlan> {
        match self {
            Choice::Legacy { plan, .. } | Choice::Recipe { plan, .. } => Some(plan),
            Choice::Refuse { .. } => None,
        }
    }
}

/// What the chooser is asked about: one probe of one selection.
pub struct Ask<'a> {
    /// The selection the user asked to listen to, `[lo, hi]` RF Hz.
    pub selection: (f64, f64),
    /// Channels the client opted into (T-874): `2` asks for stereo, which only legacy serves.
    pub channels: u32,
    /// The probe that decided (or failed to decide) the mode.
    pub probe: &'a ProbeResult,
    /// The plan the probe and refinement produced, or why there is none.
    pub plan: Result<AudioPlan, String>,
    /// The parameters the stream will report: the probe's estimates, refined where refinement
    /// locked (T-070). The ranking reads its bandwidth and features, so the recipe is ranked
    /// against exactly what the header says was measured — never against a wider claim.
    pub params: &'a EstimatedParams,
    /// Audio recipes to rank: recipes with an `audio` output, as the store holds them. Empty
    /// (the flag off, or no such recipe) means only `legacy` and `refuse` can be answered.
    pub recipes: &'a [Entry],
    /// Audio settings (the squelch and channel defaults the plan is built with).
    pub audio: &'a AudioConfig,
}

/// What one emission measured, as [`matching::rank`] reads it. **Measurements only:** an
/// abstained estimate stays `None` (unmeasured earns nothing), and nothing here comes from a band
/// plan.
pub fn measured(plan: &AudioPlan, params: &EstimatedParams) -> MeasuredSignal {
    MeasuredSignal {
        family: Some(plan.mode_name().to_owned()),
        f_center_hz: Some(plan.channel_center_hz),
        bandwidth_hz: params.bandwidth_hz,
        // A Listen probe measures neither a symbol rate nor a duty cycle: unmeasured, never
        // asserted as absent.
        symbol_rate_bd: None,
        bursty: None,
        features: matching::features_from_params(params),
    }
}

/// What the estimator measured about **energy in the probe box**, whatever its band estimate
/// then did. Every field is a measurement the probe published; `None` means "not measured",
/// never "absent".
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BoxEnergy {
    /// C13's **presence statistic** for the box, dB. The band stage attaches it to `obw99_hz` as
    /// evidence, and only reaches that stage once the presence test has passed — so `Some` *is*
    /// the estimator saying something is there, at its own threshold, and `None` is the
    /// estimator saying it never got past "is anything here at all?".
    pub presence_db: Option<f64>,
    /// Measured box SNR, dB. Abstains for any band reason, so it is usually `None` exactly when
    /// this rule is asked — kept because when it *is* measured it is the directest evidence.
    pub snr_box_db: Option<f64>,
    /// The strongest line's power over its noise, dB (the mode selector's own measurement).
    pub line_snr_db: Option<f64>,
    /// Occupied bandwidth the mode selector measured **between adjacent emissions** when C13
    /// abstained (T-099): a band measured in a crowded box is still a band measured.
    pub adjacent_obw_hz: Option<f64>,
}

impl BoxEnergy {
    /// What one probe measured about its box.
    pub fn of(probe: &ProbeResult) -> Self {
        let f = &probe.mode.features;
        Self {
            presence_db: probe.params.obw99_hz.evidence().significance_db,
            snr_box_db: probe.params.snr_box_db.value(),
            line_snr_db: f.line_snr_db,
            adjacent_obw_hz: f.adjacent.as_ref().and(f.obw99_hz),
        }
    }

    /// Whether the estimator measured energy in the box at all (see the module docs). An SNR is
    /// judged against [`WEAK_CARRIER_MIN_SNR_DB`]; the presence statistic and an adjacent-channel
    /// bandwidth are measurements that exist or do not.
    pub fn measured(&self) -> bool {
        let over =
            |v: Option<f64>| v.is_some_and(|s| s.is_finite() && s >= WEAK_CARRIER_MIN_SNR_DB);
        self.presence_db.is_some_and(f64::is_finite)
            || over(self.snr_box_db)
            || over(self.line_snr_db)
            || self
                .adjacent_obw_hz
                .is_some_and(|b| b.is_finite() && b > 0.0)
    }
}

/// The channel the weak-carrier rule plans for a selection of `selection_hz`, given what the
/// estimator measured about the box.
///
/// `Some(bandwidth_hz)` means "demodulate this as a narrowband channel"; `None` means the rule
/// does not apply and the caller refuses. Pure arithmetic, so the rule is testable on its own.
pub fn weak_narrowband_channel(energy: &BoxEnergy, selection_hz: f64) -> Option<f64> {
    if !energy.measured() {
        return None;
    }
    if !(selection_hz.is_finite() && selection_hz > 0.0)
        || selection_hz > WEAK_CARRIER_MAX_SELECTION_HZ
    {
        return None;
    }
    Some(selection_hz.clamp(WEAK_CARRIER_MIN_CHANNEL_HZ, WEAK_CARRIER_MAX_CHANNEL_HZ))
}

/// The weak-carrier plan for a probe that recognised no mode (see the module docs), or `None`
/// when the rule does not apply.
fn weak_narrowband_plan(
    probe: &ProbeResult,
    (lo, hi): (f64, f64),
    _cfg: &AudioConfig,
) -> Option<AudioPlan> {
    let bandwidth = weak_narrowband_channel(&BoxEnergy::of(probe), hi - lo)?;
    // The squelch compares the channel power against this: armed, and closed until the channel
    // actually rises. An unmeasured noise density leaves it `None`, which would force the
    // squelch permanently open — a weak channel would then hiss, so the rule declines instead.
    let noise_power = probe
        .params
        .noise_density
        .value()
        .filter(|n| n.is_finite() && *n > 0.0)
        .map(|n| n * bandwidth)?;
    Some(AudioPlan {
        mode: AnalogMode::Nbfm,
        sideband: None,
        channel_center_hz: 0.5 * (lo + hi),
        channel_bandwidth_hz: bandwidth,
        noise_power: Some(noise_power),
        agc: false,
        deemphasis_s: None,
    })
}

/// Picks the recipe that fits `m`, or `None` with the reason nothing was chosen.
fn pick(m: &MeasuredSignal, entries: &[Entry]) -> Result<(Entry, f64, String), String> {
    if entries.is_empty() {
        return Err("no audio recipe is available".into());
    }
    let ranked = matching::rank(entries, m);
    let Some(best) = ranked.candidates.first() else {
        return Err(format!(
            "no audio recipe fits the measurement ({})",
            if ranked.reasons.is_empty() {
                "nothing ranked".to_owned()
            } else {
                ranked.reasons.join(", ")
            }
        ));
    };
    if best.outcome != Outcome::Fit {
        return Err(format!(
            "the best audio recipe {} only partly fits (score {:.2}, {} compared, {} \
             conflicting, {} unmeasured)",
            best.id, best.score, best.compared, best.conflicting, best.unmeasured
        ));
    }
    // An ambiguous top two is not a decision: two recipes within `AMBIGUOUS_DELTA` of each other
    // means the measurement does not separate them, and guessing would be the "forced top
    // choice" the ranking rule exists to prevent.
    if ranked
        .candidates
        .get(1)
        .is_some_and(|second| (best.score - second.score).abs() <= matching::AMBIGUOUS_DELTA)
    {
        return Err(format!(
            "the measurement does not separate {} from {}",
            best.id, ranked.candidates[1].id
        ));
    }
    let entry = entries
        .iter()
        .find(|e| e.id == best.id)
        .cloned()
        .ok_or_else(|| "the ranked recipe is gone".to_owned())?;
    let why = format!(
        "{} fits the measurement (score {:.2}, {} of {} expectations agreeing)",
        best.id,
        best.score,
        best.agreed,
        best.compared + best.unmeasured
    );
    Ok((entry, best.score, why))
}

/// Chooses what serves one Listen request (see the module docs).
pub fn choose(ask: &Ask<'_>) -> Choice {
    let (plan, weak_carrier) = match &ask.plan {
        Ok(p) => (p.clone(), false),
        Err(why) => match weak_narrowband_plan(ask.probe, ask.selection, ask.audio) {
            // The one rule this module adds: a narrow selection with measured energy is a
            // channel, demodulated with the squelch armed rather than refused (T-869).
            Some(p) => (p, true),
            None => {
                return Choice::Refuse { why: why.clone() };
            }
        },
    };
    if weak_carrier {
        // Nothing recognised the mode, so there is no measured family to rank a recipe against:
        // ranking one on an ASSUMED family would be inventing the evidence the ranking exists to
        // weigh. Today's chain serves it (there is no NBFM recipe anyway, §12.5).
        return Choice::Legacy {
            plan: Box::new(plan),
            why: "no mode was recognised; demodulating the selection as a narrowband channel \
                  with the squelch armed (T-869)"
                .into(),
            weak_carrier: true,
        };
    }
    let seed = Seed {
        center_hz: plan.channel_center_hz,
        bandwidth_hz: plan.channel_bandwidth_hz,
        agc: plan.agc,
    };
    // Stereo is legacy's, until the recipe side of stereo lands (§12.13).
    if ask.channels > 1 {
        return Choice::Legacy {
            plan: Box::new(plan),
            why: "stereo was asked for, and a recipe's audio output is mono".into(),
            weak_carrier: false,
        };
    }
    match pick(&measured(&plan, ask.params), ask.recipes) {
        Ok((entry, score, why)) => Choice::Recipe {
            id: entry.id,
            version: entry.version,
            seed,
            plan: Box::new(plan),
            score,
            why,
        },
        Err(why) => Choice::Legacy {
            plan: Box::new(plan),
            why,
            weak_carrier: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_core::{Discontinuity, ProvenanceHandle};
    use hk_demod::audio::probe;
    use hk_dsp::InputInfo;
    use hk_estimate::SnippetRequest;
    use hk_model::{SampleTime, Timestamp};
    use hk_recipe::MatchHints;
    use num_complex::Complex32;

    const FS: f64 = 1e6;
    const FC: f64 = 100e6;
    /// The box a 20 kHz drag opens (Listen probes twice the selection).
    const BOX_HZ: f64 = 40e3;

    fn provenance() -> ProvenanceHandle {
        let p: hk_model::Provenance = serde_json::from_value(serde_json::json!({
            "device_id": "synthetic:t869-chooser-test",
            "tune": {"center_hz": FC, "sample_rate_hz": FS, "lna_db": 16.0, "vga_db": 20.0,
                     "amp_on": false, "bandwidth_hz": FS * 0.75},
            "overload": false, "quantisation_limited": false, "clock_source": "internal",
            "clock_locked": true, "timestamp_method": "synthetic",
            "timestamp_error_budget_ns": 0,
        }))
        .unwrap();
        ProvenanceHandle::new(p)
    }

    /// Probes half a second of `deviation_hz`-wide **noise-modulated FM** (line-free, like
    /// speech) in noise, through a 40 kHz box — or of noise alone when `deviation_hz` is `None`.
    fn probe_box(deviation_hz: Option<f64>) -> hk_demod::audio::ProbeResult {
        let n = (0.5 * FS) as usize;
        let mut rng = hk_dsp::synth::Rng::new(0x1234_5678);
        let mut iq = hk_dsp::synth::complex_noise(&mut rng, n, 1.0);
        if let Some(dev) = deviation_hz {
            let (mut phase, mut lp) = (0.0f64, 0.0f64);
            let mut m = hk_dsp::synth::Rng::new(0xfeed_face);
            for v in iq.iter_mut() {
                lp += 0.05 * (m.unit() * 2.0 - 1.0 - lp);
                phase = (phase + std::f64::consts::TAU * dev * (10.0 * lp).clamp(-1.0, 1.0) / FS)
                    % std::f64::consts::TAU;
                *v += Complex32::new((6.0 * phase.cos()) as f32, (6.0 * phase.sin()) as f32);
            }
        }
        let prov = provenance();
        let info = InputInfo {
            time: SampleTime {
                sample_index: 0,
                host_time: Timestamp::from_unix_nanos(1_700_000_000_000_000_000),
            },
            discontinuity: Discontinuity::STREAM_START,
            dropped_before: 0,
            provenance: &prov,
        };
        probe(
            info,
            &iq,
            &SnippetRequest {
                start_index: 0,
                end_index: n as u64,
                center_offset_hz: 0.0,
                bandwidth_hz: BOX_HZ,
            },
        )
        .expect("the probe runs")
    }

    /// The probe's answer for `deviation_hz`, run through the whole chooser on a 20 kHz
    /// selection — the shape of the explorer's NOAA drag.
    fn choice_for(deviation_hz: Option<f64>) -> (hk_demod::audio::ProbeResult, Choice) {
        let cfg = AudioConfig::default();
        let probe = probe_box(deviation_hz);
        let plan = AudioPlan::from_probe(&probe, &cfg);
        let params = probe.params.estimated_params();
        let choice = choose(&Ask {
            selection: (FC - 10e3, FC + 10e3),
            channels: 1,
            probe: &probe,
            plan,
            params: &params,
            recipes: &[wfm_entry()],
            audio: &cfg,
        });
        (probe, choice)
    }

    /// **The rule's regression test (T-869).** A wideband, line-free emission inside a narrow
    /// drag: no analog mode is recognised, and — the defect the first cut of this rule had —
    /// `snr_box_db` has abstained, so a rule that asks for a measured box SNR can never fire.
    /// [`BoxEnergy`] reads what C13 *did* publish, so the channel is demodulated with the
    /// squelch armed instead of refused.
    #[test]
    fn a_band_estimate_that_gave_up_still_leaves_measured_energy() {
        let (probe, choice) = choice_for(Some(25e3));
        assert_eq!(
            probe.mode.mode,
            AnalogMode::Unknown,
            "the fixture must be a case nothing recognises: {:?}",
            probe.mode
        );
        assert!(
            probe.params.snr_box_db.value().is_none(),
            "the premise of the defect: the box SNR has abstained, so a rule that needs it is \
             dead code here"
        );
        let energy = BoxEnergy::of(&probe);
        assert!(
            energy.presence_db.is_some(),
            "C13 measured the box's presence statistic: {energy:?}"
        );
        assert!(energy.measured(), "{energy:?}");
        match &choice {
            Choice::Legacy {
                plan, weak_carrier, ..
            } => {
                assert!(*weak_carrier, "the weak-carrier rule produced this plan");
                assert_eq!(plan.mode, AnalogMode::Nbfm);
                assert_eq!(plan.channel_bandwidth_hz, 20e3, "the user's selection");
                assert!(
                    plan.noise_power.is_some_and(|n| n > 0.0),
                    "the squelch is armed from the measured noise power: {plan:?}"
                );
            }
            other => panic!("the channel must be demodulated, not refused: {other:?}"),
        }
    }

    /// Noise: the estimator's presence test fails, nothing is measured, and the refusal LP-1 §6
    /// freezes stands.
    #[test]
    fn noise_measures_nothing_and_is_refused() {
        let (probe, choice) = choice_for(None);
        let energy = BoxEnergy::of(&probe);
        assert!(!energy.measured(), "{energy:?}");
        assert!(
            matches!(choice, Choice::Refuse { .. }),
            "noise must still be 422 no-analog-mode: {choice:?}"
        );
    }

    fn wfm_entry() -> Entry {
        Entry {
            id: "analog-wfm".into(),
            version: 1,
            name: "Analog WFM".into(),
            hints: MatchHints {
                families: vec!["wfm".into()],
                freq_hz: vec![[65.8e6, 108e6]],
                bandwidth_hz: Some([100e3, 300e3]),
                bursty: Some(false),
                ..MatchHints::default()
            },
        }
    }

    fn wfm_measured() -> MeasuredSignal {
        MeasuredSignal {
            family: Some("wfm".into()),
            f_center_hz: Some(100.1e6),
            bandwidth_hz: Some(170e3),
            symbol_rate_bd: None,
            bursty: None,
            features: vec!["pilot-19k".into()],
        }
    }

    #[test]
    fn a_measured_wfm_station_picks_the_wfm_recipe() {
        let (e, score, why) = pick(&wfm_measured(), &[wfm_entry()]).expect("a fit");
        assert_eq!(e.id, "analog-wfm");
        assert!(score >= matching::FIT_SCORE, "{score}");
        assert!(why.contains("analog-wfm"), "{why}");
    }

    #[test]
    fn frequency_alone_never_picks_a_recipe() {
        // Inside the recipe's declared FM band, and nothing else measured: the band plan must
        // not carry the decision (T-164's zero weight on `freq_hz`).
        let m = MeasuredSignal {
            f_center_hz: Some(100.1e6),
            ..MeasuredSignal::default()
        };
        let why = pick(&m, &[wfm_entry()]).expect_err("nothing measured, nothing chosen");
        assert!(why.contains("fits") || why.contains("ranked"), "{why}");
    }

    #[test]
    fn a_narrowband_mode_with_no_recipe_falls_back_to_legacy() {
        let m = MeasuredSignal {
            family: Some("nbfm".into()),
            f_center_hz: Some(162.4e6),
            bandwidth_hz: Some(12e3),
            ..MeasuredSignal::default()
        };
        let why = pick(&m, &[wfm_entry()]).expect_err("no NBFM recipe exists");
        assert!(!why.is_empty());
    }

    #[test]
    fn an_abstained_bandwidth_is_not_agreement() {
        // Family measured, bandwidth abstained: one compared slot is a coincidence, not a match.
        let m = MeasuredSignal {
            family: Some("wfm".into()),
            f_center_hz: Some(100.1e6),
            ..MeasuredSignal::default()
        };
        let why = pick(&m, &[wfm_entry()]).expect_err("too little measured");
        assert!(!why.is_empty());
    }

    #[test]
    fn an_ambiguous_top_two_chooses_nothing() {
        let mut other = wfm_entry();
        other.id = "analog-wfm-copy".into();
        let why = pick(&wfm_measured(), &[wfm_entry(), other]).expect_err("ambiguous");
        assert!(why.contains("does not separate"), "{why}");
    }

    #[test]
    fn no_audio_recipes_means_legacy() {
        let why = pick(&wfm_measured(), &[]).expect_err("nothing to rank");
        assert!(why.contains("no audio recipe"), "{why}");
    }

    #[test]
    fn the_weak_carrier_rule_needs_measured_energy_in_a_narrow_selection() {
        // The case the rule exists for (the NOAA station, and the FillsBand drag regions beside
        // it): C13's band estimate gave up, so `snr_box_db` gave up with it — but the presence
        // statistic it published on the way says something IS there, in a 20 kHz drag.
        let band_gave_up = BoxEnergy {
            presence_db: Some(24.0),
            snr_box_db: None,
            ..BoxEnergy::default()
        };
        assert_eq!(weak_narrowband_channel(&band_gave_up, 20e3), Some(20e3));

        // Noise: the presence test failed, so nothing was attached and nothing is measured —
        // still `4422` (LP-1 §6).
        assert_eq!(
            weak_narrowband_channel(&BoxEnergy::default(), 20e3),
            None,
            "an estimator that measured nothing must not open a stream"
        );

        // The other two measurements that survive a band abstention, each on its own.
        let line = BoxEnergy {
            line_snr_db: Some(9.0),
            ..BoxEnergy::default()
        };
        assert_eq!(weak_narrowband_channel(&line, 20e3), Some(20e3));
        let adjacent = BoxEnergy {
            adjacent_obw_hz: Some(12e3),
            ..BoxEnergy::default()
        };
        assert_eq!(weak_narrowband_channel(&adjacent, 20e3), Some(20e3));

        // An SNR no better than the noise is not a carrier.
        let weak_line = BoxEnergy {
            line_snr_db: Some(1.0),
            ..BoxEnergy::default()
        };
        assert_eq!(weak_narrowband_channel(&weak_line, 20e3), None);

        // A megahertz-wide drag is a region, not a channel.
        assert_eq!(weak_narrowband_channel(&band_gave_up, 1e6), None);

        // The planned channel is clamped to what NBFM means.
        assert_eq!(
            weak_narrowband_channel(&band_gave_up, 2e3),
            Some(WEAK_CARRIER_MIN_CHANNEL_HZ)
        );
        assert_eq!(
            weak_narrowband_channel(&band_gave_up, 40e3),
            Some(WEAK_CARRIER_MAX_CHANNEL_HZ)
        );
    }
}
