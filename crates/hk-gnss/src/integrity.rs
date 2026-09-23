//! GNSS jamming and spoofing — **the half that stays blind**.
//!
//! This is the on-mission, attack-map half of C36 (AWARE-002, AWARE-003), and unlike
//! acquisition it is *not* an exception to anything. A jammer is detectable by ordinary blind
//! means for a simple physical reason: it sits **above** the noise floor. The satellite does
//! not, which is why acquisition needs the codes and this does not.
//!
//! The rule this module exists to hold:
//!
//! > [`assess_jamming`] takes power evidence and only **optionally** takes receiver observables.
//!
//! With `None` for the observables — no PRN codes, no correlator, no receiver running at all —
//! an in-band floor rise still raises a jamming suspicion, because the ordinary blind floor
//! tracker found it. The observables, when a receiver happens to be running, only *corroborate*
//! and sharpen the verdict. `jamming_fires_without_any_observables` asserts this, so the blind
//! half cannot silently acquire a dependency on the exception.
//!
//! Note what this module does **not** do: it never tells a detector where to look. A floor rise
//! is found wherever it happens by the ordinary path, and the band plan then *suggests* "GNSS
//! allocation here" through the existing `hk-context` machinery — allocation-only explanations
//! already score low and cannot set an emitter's status. That is suggesting, not leading.

use crate::observable::GnssObservableEpoch;

/// Thresholds for both assessments.
#[derive(Clone, Copy, Debug)]
pub struct IntegrityConfig {
    /// In-band floor rise above the calibrated baseline that counts as interference, dB.
    pub floor_rise_db: f32,
    /// Mean C/N0 drop across locked satellites that counts as significant, dB.
    pub cn0_drop_db: f32,
    /// Fraction of tracked satellites that must drop for the loss to read as "all", not "some".
    pub all_sv_fraction: f32,
    /// C/N0 spread below which a constellation looks suspiciously uniform, dB. Real
    /// constellations spread by elevation, so near-equal power is a classic spoofing tell.
    pub equal_power_sigma_db: f32,
    /// Fastest plausible receiver motion, m/s. A handheld exceeding this implies a bad solution.
    pub max_speed_m_s: f64,
    /// Largest plausible receiver clock drift, seconds per second.
    pub max_clock_rate_s_per_s: f64,
}

impl Default for IntegrityConfig {
    fn default() -> Self {
        Self {
            floor_rise_db: 6.0,
            cn0_drop_db: 6.0,
            all_sv_fraction: 0.8,
            equal_power_sigma_db: 1.5,
            max_speed_m_s: 340.0,
            max_clock_rate_s_per_s: 1.0e-5,
        }
    }
}

/// Power-domain evidence. **This is blind**: it comes from the ordinary noise-floor tracker
/// comparing an in-band measurement against its calibrated baseline. Nothing here knows about
/// codes or satellites.
///
/// The HackRF has no GNSS-style front-end AGC to report (C36 card), so in-band power against the
/// calibrated floor is the only available proxy — and it is reported as a proxy, not as AGC.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PowerEvidence {
    /// Measured in-band noise floor, dBFS.
    pub observed_floor_dbfs: f32,
    /// Calibrated baseline floor for this band and gain state, dBFS.
    pub baseline_floor_dbfs: f32,
    /// Centre of the measured band, Hz.
    pub center_hz: f64,
    /// Width of the measured band, Hz.
    pub bandwidth_hz: f64,
}

impl PowerEvidence {
    /// How far the floor has risen above baseline, dB. Negative means it fell.
    pub fn floor_rise_db(&self) -> f32 {
        self.observed_floor_dbfs - self.baseline_floor_dbfs
    }
}

/// Receiver-side corroboration, available only when a GNSS receiver happens to be running.
/// Every field is a *loss* measurement; none of it is required to flag jamming.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LockEvidence {
    /// Satellites locked before the event.
    pub svs_before: u8,
    /// Satellites locked now.
    pub svs_now: u8,
    /// Mean C/N0 drop across satellites still locked, dB.
    pub mean_cn0_drop_db: f32,
}

impl LockEvidence {
    /// Satellites lost.
    pub fn lost(&self) -> u8 {
        self.svs_before.saturating_sub(self.svs_now)
    }

    /// Whether effectively the whole constellation went, rather than one satellite.
    pub fn all_lost(&self, cfg: &IntegrityConfig) -> bool {
        if self.svs_before == 0 {
            return false;
        }
        f32::from(self.lost()) / f32::from(self.svs_before) >= cfg.all_sv_fraction
    }
}

/// What the evidence supports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JammingVerdict {
    /// Nothing unusual.
    Quiet,
    /// Satellites lost with no floor rise: the antenna is obstructed, not jammed. A handheld
    /// indoors or against the body looks exactly like this, and must not be called jamming.
    BlockageSuspect,
    /// An in-band floor rise consistent with interference.
    JammingSuspect,
}

/// A jamming assessment with its reasoning.
#[derive(Clone, Debug, PartialEq)]
pub struct JammingAssessment {
    /// The verdict.
    pub verdict: JammingVerdict,
    /// 0..1.
    pub confidence: f32,
    /// Why, in order of weight.
    pub reasons: Vec<String>,
    /// Whether receiver observables contributed. `false` means the verdict came from blind
    /// power evidence alone.
    pub used_observables: bool,
}

/// Assesses GNSS jamming from blind power evidence, optionally corroborated by receiver lock
/// state.
///
/// `lock` is `Option` **by design**: passing `None` must still work, and must still be able to
/// return [`JammingVerdict::JammingSuspect`]. That is what keeps this half blind.
pub fn assess_jamming(
    power: &PowerEvidence,
    lock: Option<&LockEvidence>,
    cfg: &IntegrityConfig,
) -> JammingAssessment {
    let rise = power.floor_rise_db();
    let raised = rise >= cfg.floor_rise_db;
    let mut reasons = Vec::new();

    if raised {
        reasons.push(format!(
            "in-band floor {rise:.1} dB above baseline over {:.1} MHz at {:.3} MHz (power proxy; \
             the HackRF reports no front-end AGC)",
            power.bandwidth_hz / 1e6,
            power.center_hz / 1e6
        ));
    }

    let Some(lock) = lock else {
        // The blind path, on its own. A floor rise is enough.
        let verdict = if raised {
            JammingVerdict::JammingSuspect
        } else {
            JammingVerdict::Quiet
        };
        if !raised {
            reasons.push("in-band floor at baseline".to_string());
        }
        return JammingAssessment {
            verdict,
            confidence: if raised { 0.6 } else { 0.0 },
            reasons,
            used_observables: false,
        };
    };

    let all_lost = lock.all_lost(cfg);
    let some_lost = lock.lost() > 0;
    let cn0_dropped = lock.mean_cn0_drop_db >= cfg.cn0_drop_db;

    let (verdict, confidence) = match (raised, all_lost, some_lost || cn0_dropped) {
        // The textbook signature: everything goes at once and the floor rises.
        (true, true, _) => {
            reasons.push(format!(
                "all {} tracked satellites lost together",
                lock.svs_before
            ));
            (JammingVerdict::JammingSuspect, 0.95)
        }
        (true, false, true) => {
            reasons.push(format!(
                "{} of {} satellites lost, mean C/N0 down {:.1} dB",
                lock.lost(),
                lock.svs_before,
                lock.mean_cn0_drop_db
            ));
            (JammingVerdict::JammingSuspect, 0.75)
        }
        (true, false, false) => (JammingVerdict::JammingSuspect, 0.6),
        // Loss without a floor rise is obstruction, not interference.
        (false, _, true) => {
            reasons.push(format!(
                "{} of {} satellites lost but the in-band floor is at baseline ({rise:+.1} dB): \
                 obstruction, not interference",
                lock.lost(),
                lock.svs_before
            ));
            (JammingVerdict::BlockageSuspect, 0.6)
        }
        (false, _, false) => {
            reasons.push("in-band floor at baseline, constellation intact".to_string());
            (JammingVerdict::Quiet, 0.0)
        }
    };

    JammingAssessment {
        verdict,
        confidence,
        reasons,
        used_observables: true,
    }
}

/// One spoofing indication.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case", tag = "tell")]
pub enum SpoofTell {
    /// Every satellite at near-identical power. A real constellation spreads by elevation; a
    /// single spoofing transmitter does not.
    EqualPower {
        /// Observed C/N0 spread, dB.
        sigma_db: f32,
        /// Satellites considered.
        svs: u8,
    },
    /// The solution moved faster than the receiver could have.
    PositionJump {
        /// Distance moved, metres.
        metres: f64,
        /// Over this interval, seconds.
        dt_s: f64,
        /// Implied speed, m/s.
        implied_speed_m_s: f64,
    },
    /// The receiver clock stepped faster than it could drift.
    ClockJump {
        /// Step size, seconds.
        seconds: f64,
        /// Over this interval, seconds.
        dt_s: f64,
    },
}

/// A spoofing assessment over a run of epochs.
#[derive(Clone, Debug, PartialEq)]
pub struct SpoofingAssessment {
    /// Everything found, in epoch order.
    pub tells: Vec<SpoofTell>,
    /// 0..1, rising with the number of independent tells.
    pub confidence: f32,
}

impl SpoofingAssessment {
    /// Whether anything at all was found.
    pub fn is_clean(&self) -> bool {
        self.tells.is_empty()
    }
}

/// Looks for spoofing tell-tales across consecutive observable epochs (AWARE-003).
///
/// Operates purely on observables, so it is testable against mocked epochs and needs no RF.
/// **No GNSS signal generator is built or used** (C36 card): spoofing *generation* is out of
/// scope, and this only ever reads.
pub fn assess_spoofing(
    epochs: &[GnssObservableEpoch],
    cfg: &IntegrityConfig,
) -> SpoofingAssessment {
    let mut tells = Vec::new();

    for e in epochs {
        // Uniform power needs enough satellites to be meaningful.
        if e.locked_count() >= 4 {
            if let Some(sigma) = e.cn0_sigma_db() {
                if sigma < cfg.equal_power_sigma_db {
                    tells.push(SpoofTell::EqualPower {
                        sigma_db: sigma,
                        svs: e.locked_count().min(u8::MAX as usize) as u8,
                    });
                }
            }
        }
    }

    for pair in epochs.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let dt_s = (b.t.as_unix_nanos() - a.t.as_unix_nanos()) as f64 / 1e9;
        if dt_s <= 0.0 {
            continue;
        }

        if let (Some(pa), Some(pb)) = (a.position, b.position) {
            let metres = pa.distance_m(&pb);
            let speed = metres / dt_s;
            if speed > cfg.max_speed_m_s {
                tells.push(SpoofTell::PositionJump {
                    metres,
                    dt_s,
                    implied_speed_m_s: speed,
                });
            }
        }

        if let (Some(ca), Some(cb)) = (a.clock_bias_s, b.clock_bias_s) {
            let step = cb - ca;
            if (step / dt_s).abs() > cfg.max_clock_rate_s_per_s {
                tells.push(SpoofTell::ClockJump {
                    seconds: step,
                    dt_s,
                });
            }
        }
    }

    // Independent kinds of tell matter more than repeats of one kind.
    let mut kinds = [false; 3];
    for t in &tells {
        let i = match t {
            SpoofTell::EqualPower { .. } => 0,
            SpoofTell::PositionJump { .. } => 1,
            SpoofTell::ClockJump { .. } => 2,
        };
        kinds[i] = true;
    }
    let distinct = kinds.iter().filter(|&&k| k).count();
    let confidence = match distinct {
        0 => 0.0,
        1 => 0.5,
        2 => 0.8,
        _ => 0.95,
    };

    SpoofingAssessment { tells, confidence }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observable::{Ecef, SvObservable};
    use hk_model::Timestamp;

    fn power(rise_db: f32) -> PowerEvidence {
        PowerEvidence {
            observed_floor_dbfs: -80.0 + rise_db,
            baseline_floor_dbfs: -80.0,
            center_hz: crate::prn::L1_HZ,
            bandwidth_hz: 2.046e6,
        }
    }

    /// **The invariant of this module.** No observables, no codes, no receiver — a floor rise
    /// alone still flags jamming. If this ever needs a `LockEvidence` to pass, the blind half has
    /// been made to depend on the known-code exception.
    #[test]
    fn jamming_fires_without_any_observables() {
        let cfg = IntegrityConfig::default();
        let a = assess_jamming(&power(12.0), None, &cfg);
        assert_eq!(a.verdict, JammingVerdict::JammingSuspect);
        assert!(!a.used_observables, "must not require a receiver");
        assert!(a.confidence > 0.0);
        assert!(!a.reasons.is_empty());
    }

    #[test]
    fn a_quiet_band_is_quiet_without_observables() {
        let cfg = IntegrityConfig::default();
        let a = assess_jamming(&power(0.5), None, &cfg);
        assert_eq!(a.verdict, JammingVerdict::Quiet);
        assert!(!a.used_observables);
    }

    /// The C36 pitfall: a handheld indoors loses satellites without any floor rise. That is
    /// blockage, and calling it jamming would fill the attack map with the user's own body.
    #[test]
    fn satellites_lost_with_a_flat_floor_is_blockage_not_jamming() {
        let cfg = IntegrityConfig::default();
        let lock = LockEvidence {
            svs_before: 9,
            svs_now: 1,
            mean_cn0_drop_db: 10.0,
        };
        let a = assess_jamming(&power(0.0), Some(&lock), &cfg);
        assert_eq!(a.verdict, JammingVerdict::BlockageSuspect);
        assert!(a.used_observables);
    }

    #[test]
    fn a_whole_constellation_lost_with_a_floor_rise_is_the_strongest_case() {
        let cfg = IntegrityConfig::default();
        let lock = LockEvidence {
            svs_before: 10,
            svs_now: 0,
            mean_cn0_drop_db: 20.0,
        };
        let a = assess_jamming(&power(15.0), Some(&lock), &cfg);
        assert_eq!(a.verdict, JammingVerdict::JammingSuspect);
        assert!(a.confidence > 0.9, "{:?}", a.confidence);
    }

    /// Observables may only sharpen the verdict, never be required for it.
    #[test]
    fn observables_only_corroborate() {
        let cfg = IntegrityConfig::default();
        let blind = assess_jamming(&power(12.0), None, &cfg);
        let lock = LockEvidence {
            svs_before: 10,
            svs_now: 0,
            mean_cn0_drop_db: 20.0,
        };
        let corroborated = assess_jamming(&power(12.0), Some(&lock), &cfg);
        assert_eq!(blind.verdict, corroborated.verdict);
        assert!(corroborated.confidence > blind.confidence);
    }

    fn sv(prn: u8, cn0: f32) -> SvObservable {
        SvObservable {
            prn,
            cn0_dbhz: cn0,
            doppler_hz: 0.0,
            elevation_deg: None,
            locked: true,
            pseudorange_m: None,
            carrier_phase_cycles: None,
        }
    }

    fn epoch(t_s: i64, cn0s: &[f32], pos: Option<Ecef>, clock: Option<f64>) -> GnssObservableEpoch {
        GnssObservableEpoch {
            t: Timestamp::from_unix_nanos(t_s * 1_000_000_000),
            svs: cn0s
                .iter()
                .enumerate()
                .map(|(i, &c)| sv(i as u8 + 1, c))
                .collect(),
            position: pos,
            clock_bias_s: clock,
        }
    }

    #[test]
    fn a_realistic_constellation_raises_nothing() {
        let cfg = IntegrityConfig::default();
        // Spread by elevation, as a real sky is.
        let a = epoch(0, &[46.0, 42.0, 38.0, 35.0, 30.0], None, None);
        let b = epoch(1, &[46.0, 42.0, 38.0, 35.0, 30.0], None, None);
        let out = assess_spoofing(&[a, b], &cfg);
        assert!(out.is_clean(), "{:?}", out.tells);
        assert_eq!(out.confidence, 0.0);
    }

    #[test]
    fn every_satellite_at_the_same_power_is_a_tell() {
        let cfg = IntegrityConfig::default();
        let e = epoch(0, &[44.0, 44.1, 43.9, 44.0, 44.05], None, None);
        let out = assess_spoofing(&[e], &cfg);
        assert!(
            matches!(out.tells.as_slice(), [SpoofTell::EqualPower { svs: 5, .. }]),
            "{:?}",
            out.tells
        );
    }

    #[test]
    fn uniform_power_needs_enough_satellites_to_mean_anything() {
        let cfg = IntegrityConfig::default();
        // Three satellites at equal power is not evidence.
        let e = epoch(0, &[44.0, 44.0, 44.0], None, None);
        assert!(assess_spoofing(&[e], &cfg).is_clean());
    }

    #[test]
    fn an_impossible_position_jump_is_a_tell() {
        let cfg = IntegrityConfig::default();
        let a = epoch(
            0,
            &[46.0, 40.0, 35.0, 30.0],
            Some(Ecef {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            }),
            None,
        );
        let b = epoch(
            1,
            &[46.0, 40.0, 35.0, 30.0],
            Some(Ecef {
                x: 50_000.0,
                y: 0.0,
                z: 0.0,
            }),
            None,
        );
        let out = assess_spoofing(&[a, b], &cfg);
        assert!(
            out.tells
                .iter()
                .any(|t| matches!(t, SpoofTell::PositionJump { .. })),
            "{:?}",
            out.tells
        );
    }

    #[test]
    fn a_clock_step_is_a_tell() {
        let cfg = IntegrityConfig::default();
        let a = epoch(0, &[46.0, 40.0, 35.0, 30.0], None, Some(0.0));
        let b = epoch(1, &[46.0, 40.0, 35.0, 30.0], None, Some(0.5));
        let out = assess_spoofing(&[a, b], &cfg);
        assert!(
            out.tells
                .iter()
                .any(|t| matches!(t, SpoofTell::ClockJump { .. })),
            "{:?}",
            out.tells
        );
    }

    #[test]
    fn independent_tells_raise_confidence_above_repeats_of_one() {
        let cfg = IntegrityConfig::default();
        let a = epoch(
            0,
            &[44.0, 44.0, 44.0, 44.0],
            Some(Ecef {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            }),
            Some(0.0),
        );
        let b = epoch(
            1,
            &[44.0, 44.0, 44.0, 44.0],
            Some(Ecef {
                x: 50_000.0,
                y: 0.0,
                z: 0.0,
            }),
            Some(0.5),
        );
        let out = assess_spoofing(&[a, b], &cfg);
        assert!(out.confidence >= 0.95, "{:?}", out);
    }
}
