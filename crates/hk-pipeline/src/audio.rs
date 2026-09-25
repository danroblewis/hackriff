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
//! did measure **energy in the box** ([`WEAK_CARRIER_MIN_SNR_DB`]) and the user's selection is
//! narrow enough to *be* a narrowband channel ([`WEAK_CARRIER_MAX_SELECTION_HZ`]), demodulate it
//! as NBFM on the selection with the **squelch armed from the measured noise power** — silent
//! until the channel actually rises, never a refusal. What is *not* weakened:
//!
//! - **Noise stays refused.** The rule needs a measured SNR: when the estimator's presence test
//!   fails, `snr_box_db` abstains, nothing is measured and the answer is still
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

/// Smallest measured box SNR the weak-carrier rule accepts, dB. Below it the estimator has not
/// shown that anything is there, and "maybe there is a carrier" is not a signal.
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
    },
    /// Nothing to demodulate: the opener refuses `422 no-analog-mode` with this reason.
    Refuse {
        /// The probe's own reason.
        why: String,
    },
}

impl Choice {
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

/// The channel the weak-carrier rule plans for a selection of `selection_hz`, given the box SNR
/// the estimator measured (`None` = it abstained, so nothing is known to be there).
///
/// `Some(bandwidth_hz)` means "demodulate this as a narrowband channel"; `None` means the rule
/// does not apply and the caller refuses. Pure arithmetic, so the rule is testable on its own.
pub fn weak_narrowband_channel(snr_box_db: Option<f64>, selection_hz: f64) -> Option<f64> {
    let snr = snr_box_db.filter(|s| s.is_finite())?;
    if snr < WEAK_CARRIER_MIN_SNR_DB {
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
    let bandwidth = weak_narrowband_channel(probe.params.snr_box_db.value(), hi - lo)?;
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
    let plan = match &ask.plan {
        Ok(p) => p.clone(),
        Err(why) => match weak_narrowband_plan(ask.probe, ask.selection, ask.audio) {
            // The one rule this module adds: a narrow selection with measured energy is a
            // channel, demodulated with the squelch armed rather than refused (T-869).
            Some(p) => p,
            None => {
                return Choice::Refuse { why: why.clone() };
            }
        },
    };
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
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_recipe::MatchHints;

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
        // The NOAA case: ~5 dB in a 20 kHz drag → a 20 kHz NBFM channel.
        assert_eq!(weak_narrowband_channel(Some(5.0), 20e3), Some(20e3));
        // Noise: the estimator abstained, so nothing is known to be there (LP-1 §6's 4422).
        assert_eq!(weak_narrowband_channel(None, 20e3), None);
        // Measured, but no more than the noise: not a carrier.
        assert_eq!(weak_narrowband_channel(Some(1.0), 20e3), None);
        // A megahertz-wide drag is a region, not a channel.
        assert_eq!(weak_narrowband_channel(Some(20.0), 1e6), None);
        // The planned channel is clamped to what NBFM means.
        assert_eq!(
            weak_narrowband_channel(Some(9.0), 2e3),
            Some(WEAK_CARRIER_MIN_CHANNEL_HZ)
        );
        assert_eq!(
            weak_narrowband_channel(Some(9.0), 40e3),
            Some(WEAK_CARRIER_MAX_CHANNEL_HZ)
        );
    }
}
