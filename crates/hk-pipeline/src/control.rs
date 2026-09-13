//! The control thread: event-rate decisions, never on the sample path.
//!
//! - Runtime chains ([`ChainManager`]): confirmed tracks select and attach chains; member boxes,
//!   closes and merges are forwarded; coverage chains follow the tuned window; finished chains
//!   are reaped.
//! - The attention scheduler (hackriffd, [`SchedState`]): driven on **stream time** (a
//!   `SyntheticClock` set from the capture thread's newest block), so replays schedule exactly as
//!   the recording's timeline. Each due step is applied through `StepApplier`; confirmed tracks
//!   are offered as POIs (with a verification group when `verify_pois`); the detector's segment
//!   capture results are matched to the step they started in and recorded into that POI's
//!   `Verification`, which is evaluated with `hk_detect::trust` when the group ends.
//! - **Replay guard:** a recording cannot be retuned. [`ReplayGuard`] passes gain and filter
//!   changes to the replay's virtual tuning (provenance-only changes, as T-009 designed) but
//!   ignores `tune`, so detections keep the recording's true frequencies; retune trust tests on a
//!   replay are skipped and counted. The capture thread marks every provenance a virtual gain or
//!   filter change produced ([`VIRTUAL_TUNING_DEVICE_SUFFIX`] on its `device_id`), so stored
//!   detections and recordings never pass a virtual gain off as the recording's real one.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use hk_core::scheduler::{
    Poi, PoiKey, ScheduleStep, Scheduler, SchedulerConfig, StepApplier, SyntheticClock,
    Verification,
};
use hk_core::{Gains, SourceCapabilities, SourceControl, SourceError};
use hk_detect::CaptureResult;
use hk_model::{ScanPlan, Timestamp, TrackId};

use crate::chains::ChainManager;
use crate::events::{Candidate, ControlEvent};
use crate::run::Shared;
use crate::stats::{Counters, add, inc};
use crate::verify::{Cap, TrustEval};

/// Appended to a provenance's `device_id` when its gain or filter state is virtual: the
/// scheduler changed it on a replay, and the recorded samples never had it.
pub const VIRTUAL_TUNING_DEVICE_SUFFIX: &str = "+virtual-tuning";

/// Passes a scheduler's controls to a replay, except retunes (see the module docs).
pub struct ReplayGuard {
    inner: Arc<dyn SourceControl>,
    counters: Arc<Counters>,
}

impl ReplayGuard {
    /// Guards `inner`.
    pub fn new(inner: Arc<dyn SourceControl>, counters: Arc<Counters>) -> Self {
        Self { inner, counters }
    }
}

impl SourceControl for ReplayGuard {
    fn capabilities(&self) -> &SourceCapabilities {
        self.inner.capabilities()
    }

    fn tune(&self, _center_hz: f64) -> Result<(), SourceError> {
        inc(&self.counters.scheduler.virtual_tunes_ignored);
        Ok(())
    }

    fn set_sample_rate(&self, sample_rate_hz: f64) -> Result<(), SourceError> {
        self.inner.set_sample_rate(sample_rate_hz)
    }

    fn set_gains(&self, gains: &Gains) -> Result<(), SourceError> {
        self.inner.set_gains(gains)
    }

    fn set_baseband_filter(&self, bandwidth_hz: f64) -> Result<(), SourceError> {
        self.inner.set_baseband_filter(bandwidth_hz)
    }

    fn set_bias_tee(&self, enabled: bool) -> Result<(), SourceError> {
        self.inner.set_bias_tee(enabled)
    }

    fn start(&self) -> Result<(), SourceError> {
        self.inner.start()
    }

    fn stop(&self) -> Result<(), SourceError> {
        Ok(())
    }
}

/// Scheduler state owned by the control thread.
pub(crate) struct SchedState {
    scheduler: Scheduler<SyntheticClock>,
    applier: StepApplier,
    counters: Arc<Counters>,
    step_end_ns: Option<i64>,
    recent: VecDeque<ScheduleStep>,
    verifs: HashMap<PoiKey, Verification<Cap>>,
    keys: HashMap<TrackId, PoiKey>,
    next_key: PoiKey,
    pairs: u8,
    verify: bool,
}

impl SchedState {
    /// Compiles `plan` for the source behind `control`. A non-controllable source (a replay) runs
    /// every step at its own rate and behind the [`ReplayGuard`].
    pub fn new(
        plan: &ScanPlan,
        control: Arc<dyn SourceControl>,
        fs: f64,
        t0: Timestamp,
        counters: Arc<Counters>,
        verify: bool,
    ) -> anyhow::Result<Self> {
        let caps = control.capabilities().clone();
        let mut cfg = SchedulerConfig::from_plan(plan)?;
        let control: Arc<dyn SourceControl> = if caps.controllable {
            control
        } else {
            cfg.sweep_rate_hz = fs;
            cfg.dwell_min_rate_hz = fs;
            cfg.max_span_hz = fs;
            Arc::new(ReplayGuard::new(control, Arc::clone(&counters)))
        };
        let pairs = cfg.gain_step_pairs;
        let scheduler = Scheduler::new(plan, cfg, &caps, SyntheticClock::new(t0))?;
        Ok(Self {
            scheduler,
            applier: StepApplier::new(control),
            counters,
            step_end_ns: None,
            recent: VecDeque::with_capacity(64),
            verifs: HashMap::new(),
            keys: HashMap::new(),
            next_key: 1,
            pairs,
            verify,
        })
    }

    fn offer(&mut self, cand: &Candidate) {
        let Some(track) = cand.track else { return };
        let key = *self.keys.entry(track).or_insert_with(|| {
            let k = self.next_key;
            self.next_key += 1;
            k
        });
        let poi = Poi {
            key,
            center_hz: 0.5 * (cand.f_lo_hz + cand.f_hi_hz),
            bandwidth_hz: (cand.f_hi_hz - cand.f_lo_hz).max(1.0),
            interestingness: 1.0,
            burst_interval_ns: None,
            verify: self.verify,
        };
        let c = &self.counters.scheduler;
        match self.scheduler.offer_poi(poi) {
            Ok(()) => inc(&c.pois_offered),
            Err(_) => inc(&c.pois_refused),
        }
    }

    fn remove(&mut self, track: TrackId) {
        if let Some(key) = self.keys.remove(&track) {
            self.scheduler.remove_poi(key);
        }
    }

    fn on_capture(&mut self, t_start: Timestamp, capture: CaptureResult) {
        let t = t_start.as_unix_nanos();
        let Some(step) = self
            .recent
            .iter()
            .find(|s| s.t_start.as_unix_nanos() <= t && t < s.t_end().as_unix_nanos())
            .copied()
        else {
            return;
        };
        let Some(key) = step.purpose.poi() else {
            return;
        };
        if let Some(v) = self.verifs.get_mut(&key) {
            if v.record(&step, Cap(capture)).is_ok() {
                inc(&self.counters.scheduler.verification_captures);
            }
        }
    }

    fn tick(&mut self, now_ns: i64) {
        if now_ns <= 0 {
            return;
        }
        self.scheduler
            .clock()
            .set(Timestamp::from_unix_nanos(now_ns));
        if self.step_end_ns.is_some_and(|end| now_ns < end) {
            return;
        }
        let step = self.scheduler.next_step();
        let current = step.purpose.poi();
        let counters = Arc::clone(&self.counters);
        let c = &counters.scheduler;
        let finished: Vec<PoiKey> = self
            .verifs
            .keys()
            .copied()
            .filter(|k| Some(*k) != current)
            .collect();
        for k in finished {
            if let Some(v) = self.verifs.remove(&k) {
                let track = self
                    .keys
                    .iter()
                    .find(|(_, key)| **key == k)
                    .map(|(t, _)| *t);
                let t = Timestamp::from_unix_nanos(now_ns);
                let report = v.evaluate(&mut TrustEval::new(c, &counters.verdicts, track, t));
                add(
                    &c.gain_pairs_skipped_clipped,
                    u64::from(report.gain_pairs_skipped_clipped),
                );
                inc(&c.verifications);
            }
        }
        if let Some(k) = current {
            let pairs = self.pairs;
            self.verifs
                .entry(k)
                .or_insert_with(|| Verification::new(k, pairs));
        }
        match self.applier.apply(&step) {
            Ok(_) => inc(&c.steps),
            Err(_) => inc(&c.apply_errors),
        }
        if step.purpose.is_discovery() {
            inc(&c.sweep_steps);
        } else if step.purpose.is_trust_test() {
            inc(&c.trust_steps);
        } else if current.is_some() {
            inc(&c.dwell_steps);
        }
        self.step_end_ns = Some(step.t_end().as_unix_nanos());
        if self.recent.len() == 64 {
            self.recent.pop_front();
        }
        self.recent.push_back(step);
    }
}

/// Runs until the detection reader has finished and every chain has been reaped.
pub(crate) fn run(
    shared: Arc<Shared>,
    rx: Receiver<ControlEvent>,
    mut sched: Option<SchedState>,
) -> anyhow::Result<()> {
    let mut chains = ChainManager::new(Arc::clone(&shared));
    let mut detect_done = false;
    loop {
        match rx.recv_timeout(Duration::from_millis(2)) {
            Ok(ev) => match ev {
                ControlEvent::TrackConfirmed(cand) => {
                    if let Some(s) = sched.as_mut() {
                        s.offer(&cand);
                    }
                    chains.on_confirmed(cand);
                }
                ControlEvent::Member { track, member } => chains.on_member(track, member),
                ControlEvent::TrackClosed { track, .. } => {
                    if let Some(s) = sched.as_mut() {
                        s.remove(track);
                    }
                    chains.on_track_closed(track);
                }
                ControlEvent::TrackMerged { from, into } => chains.on_merged(from, into),
                ControlEvent::Capture { t_start, capture } => {
                    if let Some(s) = sched.as_mut() {
                        s.on_capture(t_start, *capture);
                    }
                }
                ControlEvent::Manual { spec, candidate } => {
                    if !detect_done {
                        chains.attach_manual(&spec, candidate);
                    }
                }
                ControlEvent::DetachManual => chains.detach_manual(),
                ControlEvent::DetectFinished => {
                    detect_done = true;
                    chains.detach_all();
                }
            },
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                detect_done = true;
                chains.detach_all();
            }
        }
        if !detect_done {
            chains.poll_coverage();
            if let Some(s) = sched.as_mut() {
                s.tick(shared.counters.stream_time_ns.load(Ordering::Relaxed));
            }
        }
        chains.reap();
        if detect_done && chains.running() == 0 {
            break;
        }
    }
    Ok(())
}
