//! The actuator for automatic front-end gain management (T-945;
//! [docs/28](../../../docs/28-front-end-gain-management.md)).
//!
//! [`hk_core::gain`] is the policy — the ladder, the score, the bounded search — and touches no
//! device. This is the half that moves the radio, and it exists here for one reason: **every probe
//! must be one [`DeviceAction::Gains`] through the one [`crate::DeviceGate`]** (T-343). A gain run
//! is a sequence of real device actions; it must serialise against a retune, against a user's gain
//! change and against another run exactly as any other device action does, and it must be recorded
//! against the same `device_id` as the frames it produces. A policy that wrote to the driver
//! directly would be a second, ungated path to the front end.
//!
//! [`GainManager::run`] walks one convergence pass:
//!
//! 1. it reads the state in force from [`LiveControl::tuning`];
//! 2. for each probe the controller asks for, it commands the state
//!    ([`LiveControl::set_gains`]) and hands the caller **the state the device actually took** —
//!    what `set_gains` returned after the device's own quantisation, never what was asked for;
//! 3. the caller's `measure` closure waits the settle gap, collects the dwell and answers a
//!    [`GainQuality`];
//! 4. at the end it commands the committed state and returns the [`GainReport`].
//!
//! The measurement is the caller's because what "quality" means depends on what is being worked:
//! an RDS group rate for an FM station, a CRC-valid frame rate for a packet mode, channel SNR when
//! nothing is being decoded. Nothing about that belongs in the control plane.
//!
//! **A run that cannot finish puts the radio back.** If a device action fails or the caller cannot
//! measure, the manager commands the state it started from before returning the error, so an
//! interrupted run never leaves the front end parked on a probe.
//!
//! Nothing constructs a `GainManager` in the composed pipeline yet (docs/28 §6): the policy is off
//! by default and ships dark until HIL.

use std::sync::Arc;

use hk_core::gain::{
    GainController, GainError, GainPolicy, GainProbe, GainQuality, GainReport, GainState, GainStep,
    GainTrigger,
};
use hk_core::source::NamedGain;

use crate::live_control::{LiveControl, LiveControlError};

#[cfg(doc)]
use crate::live_control::DeviceAction;

/// Why a gain run stopped early.
#[derive(Debug)]
pub enum GainRunError {
    /// A device action was refused (the gate is held, the device rejected the value, the run is a
    /// replay). The front end has been put back where the run found it.
    Control(LiveControlError),
    /// The controller refused a measurement (it did not name the outstanding probe's state).
    Policy(GainError),
    /// The caller could not measure this probe, and said why.
    Measure(String),
}

impl std::fmt::Display for GainRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Control(e) => write!(f, "a gain probe was refused: {e}"),
            Self::Policy(e) => write!(f, "{e}"),
            Self::Measure(m) => write!(f, "a gain probe could not be measured: {m}"),
        }
    }
}

impl std::error::Error for GainRunError {}

impl From<GainError> for GainRunError {
    fn from(e: GainError) -> Self {
        Self::Policy(e)
    }
}

/// Drives a [`GainController`] over one live front end (see the [module docs](self)).
pub struct GainManager {
    control: Arc<dyn LiveControl>,
    controller: GainController,
    actions: usize,
}

impl GainManager {
    /// A manager for `control` under `policy`, or [`GainError::NoStages`] when the device declares
    /// no adjustable gain.
    pub fn new(control: Arc<dyn LiveControl>, policy: GainPolicy) -> Result<Self, GainError> {
        let controller = GainController::new(control.capabilities(), policy)?;
        Ok(Self {
            control,
            controller,
            actions: 0,
        })
    }

    /// The policy and search state.
    pub fn controller(&self) -> &GainController {
        &self.controller
    }

    /// The search state, e.g. to set its clock ([`GainController::set_now_s`]).
    pub fn controller_mut(&mut self) -> &mut GainController {
        &mut self.controller
    }

    /// Device actions issued since this manager was built.
    pub fn device_actions(&self) -> usize {
        self.actions
    }

    /// The gain state in force, as this device can express it.
    ///
    /// A stage [`LiveControl::tuning`] does not name has never been set through this control, so
    /// its value is *unknown* rather than zero. The ladder fills it with the stage's minimum, which
    /// is a **proposal for what to command**, not a claim about the past: the first probe commands
    /// every stage explicitly, so from then on the run knows each one because it set it.
    pub fn state_in_force(&self) -> GainState {
        self.controller
            .ladder()
            .realizable(&GainState::new(self.control.tuning().gains))
    }

    /// Runs one convergence pass. `Ok(None)` when the policy is off, a run is already in progress
    /// or the last one was too recent — in which case no device action was issued.
    ///
    /// `measure` is given the probe with **the state the device took**; it must wait
    /// `probe.settle_s`, collect `probe.dwell_s` of samples that do not span a discontinuity, and
    /// answer a [`GainQuality`] measured under `probe.state`.
    pub fn run<M>(
        &mut self,
        trigger: GainTrigger,
        mut measure: M,
    ) -> Result<Option<GainReport>, GainRunError>
    where
        M: FnMut(&GainProbe) -> Result<GainQuality, String>,
    {
        let from = self.state_in_force();
        if !self.controller.trigger(&from, trigger) {
            return Ok(None);
        }
        loop {
            match self.controller.next_step() {
                GainStep::Probe(probe) => {
                    let applied = match self.apply(&probe.state) {
                        Ok(a) => a,
                        Err(e) => return Err(self.abort(&from, GainRunError::Control(e))),
                    };
                    let probe = GainProbe {
                        state: applied,
                        ..probe
                    };
                    let quality = match measure(&probe) {
                        Ok(q) => q,
                        Err(m) => return Err(self.abort(&from, GainRunError::Measure(m))),
                    };
                    if let Err(e) = self.controller.observe(quality) {
                        return Err(self.abort(&from, GainRunError::Policy(e)));
                    }
                }
                GainStep::Commit { state, report } => {
                    match self.apply(&state) {
                        Ok(_) => {
                            // Only now is it a commit: the device took it. Doing this bookkeeping
                            // on the choice alone would start a re-run interval for a state the
                            // radio never reached.
                            self.controller.committed();
                            return Ok(Some(*report));
                        }
                        Err(e) => return Err(self.abort(&from, GainRunError::Control(e))),
                    };
                }
                // `Disabled` cannot be reached inside a run (the trigger above refused it), and
                // `Hold` means the controller measured nothing and already abandoned the run
                // itself. Neither has issued a device action, and neither leaves a run in progress.
                GainStep::Disabled | GainStep::Hold => return Ok(None),
            }
        }
    }

    /// One [`DeviceAction::Gains`], answering with what the device took.
    fn apply(&mut self, state: &GainState) -> Result<GainState, LiveControlError> {
        let gains: Vec<NamedGain> = state.gains().to_vec();
        self.actions += 1;
        let tuning = self.control.set_gains(&gains)?;
        Ok(self
            .controller
            .ladder()
            .realizable(&GainState::new(tuning.gains)))
    }

    /// Ends a run that could not finish: the controller **abandons** it and the front end goes back
    /// where the run found it. Then the original failure is reported — a failed restore is not
    /// allowed to hide it.
    ///
    /// The abandon is the load-bearing half. Without it the controller would keep the outstanding
    /// probe and stay `Running`, so every later [`Self::run`] would answer `Ok(None)` — the same
    /// answer as "off" or "holding" — and a policy that had stopped for good on one refused probe
    /// would never say so (found in review, T-945). A refused probe is not rare: the gate exists
    /// because a user retune can collide with exactly this.
    fn abort(&mut self, from: &GainState, err: GainRunError) -> GainRunError {
        self.controller.abandon();
        let _ = self.apply(from);
        err
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Mutex, PoisonError};

    use hk_core::gain::{DecodeQuality, GainPhase};
    use hk_core::source::SourceCapabilities;
    use hk_model::BiasTee;

    use super::*;
    use crate::live_control::{DeviceAction, DeviceGate, LiveTuning};

    /// A front end that quantises gains as its capabilities say, records every device action, and
    /// can be made to refuse one. Its gate is a real [`DeviceGate`], so a probe that arrives while
    /// another device action is held is refused exactly as it would be in a server.
    struct Spy {
        caps: SourceCapabilities,
        gate: DeviceGate,
        tuning: Mutex<LiveTuning>,
        applied: Mutex<Vec<GainState>>,
        /// Refuse exactly this `set_gains` call number (0-based).
        refuse_at: AtomicU64,
        /// Refuse every call from this number on.
        refuse_from: AtomicU64,
        calls: AtomicU64,
        other_actions: AtomicBool,
    }

    impl Spy {
        fn new() -> Arc<Self> {
            let caps = SourceCapabilities::hackrf_one();
            Arc::new(Self {
                caps,
                gate: DeviceGate::new(Some("spy:1".into())),
                tuning: Mutex::new(LiveTuning {
                    center_hz: 98.1e6,
                    sample_rate_hz: 2e6,
                    gains: vec![
                        NamedGain::new("lna", 32.0),
                        NamedGain::new("vga", 30.0),
                        NamedGain::new("amp", 11.0),
                    ],
                    bias_tee: BiasTee::Off,
                    baseband_filter_hz: None,
                }),
                applied: Mutex::new(Vec::new()),
                refuse_at: AtomicU64::new(u64::MAX),
                refuse_from: AtomicU64::new(u64::MAX),
                calls: AtomicU64::new(0),
                other_actions: AtomicBool::new(false),
            })
        }
        fn applied(&self) -> Vec<GainState> {
            self.applied
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    impl LiveControl for Spy {
        fn capabilities(&self) -> &SourceCapabilities {
            &self.caps
        }
        fn tuning(&self) -> LiveTuning {
            self.tuning
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
        fn device_id(&self) -> Option<&str> {
            Some("spy:1")
        }
        fn set_center(&self, _hz: f64) -> Result<LiveTuning, LiveControlError> {
            self.other_actions.store(true, Ordering::SeqCst);
            Ok(self.tuning())
        }
        fn set_rate(&self, _hz: f64) -> Result<LiveTuning, LiveControlError> {
            self.other_actions.store(true, Ordering::SeqCst);
            Ok(self.tuning())
        }
        fn set_window(&self, _c: f64, _r: f64) -> Result<LiveTuning, LiveControlError> {
            self.other_actions.store(true, Ordering::SeqCst);
            Ok(self.tuning())
        }
        fn set_gains(&self, gains: &[NamedGain]) -> Result<LiveTuning, LiveControlError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == self.refuse_at.load(Ordering::SeqCst)
                || n >= self.refuse_from.load(Ordering::SeqCst)
            {
                return Err(LiveControlError::DeviceBusy {
                    device_id: Some("spy:1".into()),
                    holder: DeviceAction::Retune,
                    held_for_s: 0.2,
                    requested: DeviceAction::Gains,
                });
            }
            let _op = self.gate.enter(DeviceAction::Gains)?;
            let mut t = self.tuning.lock().unwrap_or_else(PoisonError::into_inner);
            for g in gains {
                let stage =
                    self.caps
                        .gain_stage(&g.stage)
                        .ok_or_else(|| LiveControlError::OutOfRange {
                            what: format!("gain stage {}", g.stage),
                            value: g.db,
                        })?;
                let db = stage.quantise(g.db).ok_or(LiveControlError::OutOfRange {
                    what: format!("gain stage {}", g.stage),
                    value: g.db,
                })?;
                match t.gains.iter_mut().find(|x| x.stage == g.stage) {
                    Some(x) => x.db = db,
                    None => t.gains.push(NamedGain::new(g.stage.clone(), db)),
                }
            }
            self.applied
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(GainState::new(t.gains.clone()));
            Ok(t.clone())
        }
        fn set_bias_tee(&self, _on: bool) -> Result<LiveTuning, LiveControlError> {
            self.other_actions.store(true, Ordering::SeqCst);
            Ok(self.tuning())
        }
        fn set_baseband_filter(&self, _hz: f64) -> Result<LiveTuning, LiveControlError> {
            self.other_actions.store(true, Ordering::SeqCst);
            Ok(self.tuning())
        }
    }

    /// The docs/28 §2 shape as a function of total gain: dead at both ends, a window in the middle.
    fn scene(total_db: f64, dwell_s: f64) -> GainQuality {
        let clip = if total_db < 46.0 {
            0.0
        } else {
            (1e-3 * 10f64.powf((total_db - 50.0) / 4.0)).min(0.95)
        };
        let snr = if total_db < 20.0 || clip > 5e-3 {
            None
        } else {
            Some(total_db - 26.0 - 60.0 * clip)
        };
        let groups = match snr {
            Some(s) if s > 12.0 && clip <= 3e-3 => ((s - 12.0) * 1.5) as u64,
            _ => 0,
        };
        GainQuality::new(GainState::new([]), dwell_s)
            .with_clip(clip, Some(clip > 1e-4))
            .with_snr(snr)
            .with_decode(Some(DecodeQuality::from_count(
                "rds-groups",
                groups,
                dwell_s,
            )))
    }

    fn measure(probe: &GainProbe) -> Result<GainQuality, String> {
        let mut q = scene(probe.state.total_db(), probe.dwell_s);
        // The quality is measured under the state the device took.
        q.state = probe.state.clone();
        Ok(q)
    }

    #[test]
    fn a_disabled_policy_issues_no_device_action_at_all() {
        let spy = Spy::new();
        let mut m = GainManager::new(spy.clone(), GainPolicy::default()).unwrap();
        assert!(m.run(GainTrigger::Overload, measure).unwrap().is_none());
        assert_eq!(m.device_actions(), 0);
        assert!(spy.applied().is_empty(), "the radio was touched while off");
    }

    #[test]
    fn every_probe_is_one_gains_action_and_the_run_commits_the_best_state() {
        let spy = Spy::new();
        let mut m = GainManager::new(spy.clone(), GainPolicy::default().enabled()).unwrap();
        let started = m.state_in_force();
        assert_eq!(started.total_db(), 73.0, "the explorer's overloaded state");
        let report = m
            .run(GainTrigger::Overload, measure)
            .unwrap()
            .expect("a run");
        // One device action per probe, plus the commit.
        assert_eq!(m.device_actions(), report.probes.len() + 1);
        assert_eq!(spy.applied().len(), report.probes.len() + 1);
        // It came down, and the state it left the radio in decodes.
        assert!(report.committed.total_db() < 73.0, "{}", report.explain());
        let left = GainState::new(spy.tuning().gains);
        assert!(
            left.same_as(&report.committed),
            "{left} vs {}",
            report.committed
        );
        let groups = report
            .probes
            .iter()
            .find(|p| p.applied.same_as(&report.committed))
            .and_then(|p| p.quality.decode.as_ref())
            .map(|d| d.rate_per_s)
            .unwrap();
        assert!(groups > 0.0, "committed a state that decodes nothing");
        // And it never reached any other device route.
        assert!(!spy.other_actions.load(Ordering::SeqCst));
        assert!(report.probes.iter().any(|p| p.phase == GainPhase::Fine));
    }

    #[test]
    fn the_probe_is_measured_under_what_the_device_took_not_what_was_asked_for() {
        let spy = Spy::new();
        let mut m = GainManager::new(spy.clone(), GainPolicy::default().enabled()).unwrap();
        let report = m
            .run(GainTrigger::Manual, |probe| {
                // The device quantises to its own grid (LNA 8 dB, VGA 2 dB, amp on/off), so every
                // probe handed to the caller is already realizable.
                for g in probe.state.gains() {
                    let stage = SourceCapabilities::hackrf_one()
                        .gain_stage(&g.stage)
                        .unwrap()
                        .clone();
                    assert_eq!(stage.quantise(g.db), Some(g.db), "{} {}", g.stage, g.db);
                }
                measure(probe)
            })
            .unwrap()
            .unwrap();
        for p in &report.probes {
            assert!(
                p.applied.same_as(&p.commanded),
                "this device took what it was given: {} vs {}",
                p.applied,
                p.commanded
            );
        }
    }

    #[test]
    fn a_refused_device_action_puts_the_front_end_back_and_says_why() {
        let spy = Spy::new();
        spy.refuse_at.store(3, Ordering::SeqCst);
        let mut m = GainManager::new(spy.clone(), GainPolicy::default().enabled()).unwrap();
        let started = m.state_in_force();
        let err = m.run(GainTrigger::Overload, measure).unwrap_err();
        assert!(
            matches!(
                &err,
                GainRunError::Control(LiveControlError::DeviceBusy { .. })
            ),
            "{err}"
        );
        assert!(err.to_string().contains("refused"), "{err}");
        // The 4th call was refused; the 5th is the restore.
        let left = GainState::new(spy.tuning().gains);
        assert!(
            left.same_as(&started),
            "an interrupted run left the radio on a probe: {left} vs {started}"
        );
    }

    #[test]
    fn a_front_end_that_keeps_refusing_still_reports_the_first_failure() {
        let spy = Spy::new();
        spy.refuse_from.store(2, Ordering::SeqCst);
        let mut m = GainManager::new(spy.clone(), GainPolicy::default().enabled()).unwrap();
        let err = m.run(GainTrigger::Overload, measure).unwrap_err();
        // The restore was refused too. That cannot turn into a different error, and it cannot turn
        // into success: the caller is told the probe was refused, and where the radio is is
        // whatever the device last accepted.
        assert!(
            matches!(
                &err,
                GainRunError::Control(LiveControlError::DeviceBusy { .. })
            ),
            "{err}"
        );
        assert_eq!(spy.applied().len(), 2, "only the accepted calls landed");
    }

    #[test]
    fn a_probe_that_cannot_be_measured_ends_the_run_rather_than_guessing() {
        let spy = Spy::new();
        let mut m = GainManager::new(spy.clone(), GainPolicy::default().enabled()).unwrap();
        let started = m.state_in_force();
        let err = m
            .run(GainTrigger::Overload, |probe| {
                if probe.index < 2 {
                    measure(probe)
                } else {
                    Err("the ring had no contiguous dwell at this state".into())
                }
            })
            .unwrap_err();
        assert!(matches!(err, GainRunError::Measure(_)), "{err}");
        assert!(
            GainState::new(spy.tuning().gains).same_as(&started),
            "the radio must be put back"
        );
    }

    /// Found in review (T-945): after one refused probe the manager used to be dead for good —
    /// the controller kept the outstanding probe, stayed `Running`, and every later `run` answered
    /// `Ok(None)`, which is also what "off" and "holding" answer. A gain run colliding with a user
    /// retune on the gate is exactly the case the gate exists for, so this is not a rare path.
    #[test]
    fn after_a_refused_probe_a_later_run_still_happens() {
        let spy = Spy::new();
        spy.refuse_at.store(3, Ordering::SeqCst);
        let mut m = GainManager::new(spy.clone(), GainPolicy::default().enabled()).unwrap();
        let started = m.state_in_force();
        let err = m.run(GainTrigger::Overload, measure).unwrap_err();
        assert!(matches!(err, GainRunError::Control(_)), "{err}");
        assert!(!m.controller().running(), "the run must not still be open");
        assert_eq!(m.controller().settled(), None, "it decided nothing");
        // The one that matters: the front end is still managed.
        let report = m
            .run(GainTrigger::Overload, measure)
            .expect("the second run must not fail")
            .expect("and must not be silently refused");
        assert!(
            report.committed.total_db() < started.total_db(),
            "{}",
            report.explain()
        );
        assert_eq!(m.controller().settled(), Some(&report.committed));
        assert!(GainState::new(spy.tuning().gains).same_as(&report.committed));
    }

    /// The review's second half: the commit is the device taking it, not the controller choosing it.
    #[test]
    fn a_refused_commit_settles_nothing_and_does_not_block_the_next_run() {
        let spy = Spy::new();
        let policy = GainPolicy {
            max_probes: 2,
            ..GainPolicy::default().enabled()
        };
        // Two probes then the commit: calls 0 and 1 are the probes, call 2 is the commit.
        spy.refuse_at.store(2, Ordering::SeqCst);
        let mut m = GainManager::new(spy.clone(), policy).unwrap();
        let started = m.state_in_force();
        let err = m.run(GainTrigger::Overload, measure).unwrap_err();
        assert!(matches!(err, GainRunError::Control(_)), "{err}");
        assert_eq!(
            m.controller().settled(),
            None,
            "a commit the device refused settles nothing"
        );
        assert!(GainState::new(spy.tuning().gains).same_as(&started));
        // And no re-run interval was started for a state the radio never reached.
        spy.refuse_at.store(u64::MAX, Ordering::SeqCst);
        assert!(m.run(GainTrigger::Overload, measure).unwrap().is_some());
    }

    #[test]
    fn a_second_run_inside_the_rerun_interval_issues_nothing() {
        let spy = Spy::new();
        let mut m = GainManager::new(spy.clone(), GainPolicy::default().enabled()).unwrap();
        m.controller_mut().set_now_s(500.0);
        assert!(m.run(GainTrigger::Overload, measure).unwrap().is_some());
        let actions = m.device_actions();
        assert!(m.run(GainTrigger::Overload, measure).unwrap().is_none());
        assert_eq!(m.device_actions(), actions, "a refused run costs nothing");
    }

    #[test]
    fn a_device_with_no_adjustable_gain_has_no_manager() {
        struct NoGain(SourceCapabilities, LiveTuning);
        impl LiveControl for NoGain {
            fn capabilities(&self) -> &SourceCapabilities {
                &self.0
            }
            fn tuning(&self) -> LiveTuning {
                self.1.clone()
            }
            fn set_center(&self, _hz: f64) -> Result<LiveTuning, LiveControlError> {
                unreachable!()
            }
            fn set_rate(&self, _hz: f64) -> Result<LiveTuning, LiveControlError> {
                unreachable!()
            }
            fn set_window(&self, _c: f64, _r: f64) -> Result<LiveTuning, LiveControlError> {
                unreachable!()
            }
            fn set_gains(&self, _g: &[NamedGain]) -> Result<LiveTuning, LiveControlError> {
                unreachable!()
            }
            fn set_bias_tee(&self, _on: bool) -> Result<LiveTuning, LiveControlError> {
                unreachable!()
            }
            fn set_baseband_filter(&self, _hz: f64) -> Result<LiveTuning, LiveControlError> {
                unreachable!()
            }
        }
        let mut caps = SourceCapabilities::hackrf_one();
        caps.gain_stages.clear();
        caps.driver = "replay".into();
        let tuning = LiveTuning {
            center_hz: 98.1e6,
            sample_rate_hz: 2e6,
            gains: Vec::new(),
            bias_tee: BiasTee::Unknown,
            baseband_filter_hz: None,
        };
        let err = match GainManager::new(
            Arc::new(NoGain(caps, tuning)),
            GainPolicy::default().enabled(),
        ) {
            Ok(_) => panic!("a device with no gain stage got a gain manager"),
            Err(e) => e,
        };
        assert_eq!(
            err,
            GainError::NoStages {
                driver: "replay".into()
            }
        );
    }
}
