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
//! - **One sample rate per run** (T-037a): detection resolution, ring sizing and history geometry
//!   are fixed at start, so every scheduler step (sweep and dwell) uses the pipeline's rate, for a
//!   live radio as for a replay.
//! - **Source restarts** (`--loop`, T-037a): the scheduler controls the source through a
//!   [`SwitchableControl`]; when the capture thread reopens the source the pipeline points it at
//!   the new source's control, and the next tick resends the current step in full, so gain and
//!   filter steps keep applying after every restart.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use hk_core::scheduler::{
    Poi, PoiKey, ScheduleStep, Scheduler, SchedulerConfig, StepApplier, SyntheticClock,
    Verification,
};
use hk_core::{
    DeviceInfo, Gains, SourceCapabilities, SourceControl, SourceError, SourceStats,
    SweepCapability, SweepPlan,
};
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

    fn set_gain(&self, stage: &str, db: f64) -> Result<(), SourceError> {
        self.inner.set_gain(stage, db)
    }

    fn stats(&self) -> Option<SourceStats> {
        self.inner.stats()
    }

    fn device_info(&self) -> Option<DeviceInfo> {
        self.inner.device_info()
    }
}

/// A control handle whose target can be replaced while a scheduler holds it (T-037a): the
/// pipeline repoints it when `--loop` reopens the source.
pub struct SwitchableControl {
    capabilities: SourceCapabilities,
    inner: RwLock<Arc<dyn SourceControl>>,
    generation: AtomicU64,
}

impl SwitchableControl {
    /// Controls `inner`; capabilities are the first source's (a reopened source is the same kind).
    pub fn new(inner: Arc<dyn SourceControl>) -> Self {
        Self {
            capabilities: inner.capabilities().clone(),
            inner: RwLock::new(inner),
            generation: AtomicU64::new(0),
        }
    }

    /// Points every later command at `inner`.
    pub fn replace(&self, inner: Arc<dyn SourceControl>) {
        *self.inner.write().unwrap_or_else(PoisonError::into_inner) = inner;
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Replacements so far.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    fn current(&self) -> Arc<dyn SourceControl> {
        Arc::clone(&self.inner.read().unwrap_or_else(PoisonError::into_inner))
    }
}

impl SourceControl for SwitchableControl {
    fn capabilities(&self) -> &SourceCapabilities {
        &self.capabilities
    }

    fn tune(&self, center_hz: f64) -> Result<(), SourceError> {
        self.current().tune(center_hz)
    }

    fn set_sample_rate(&self, sample_rate_hz: f64) -> Result<(), SourceError> {
        self.current().set_sample_rate(sample_rate_hz)
    }

    fn set_gains(&self, gains: &Gains) -> Result<(), SourceError> {
        self.current().set_gains(gains)
    }

    fn set_baseband_filter(&self, bandwidth_hz: f64) -> Result<(), SourceError> {
        self.current().set_baseband_filter(bandwidth_hz)
    }

    fn set_bias_tee(&self, enabled: bool) -> Result<(), SourceError> {
        self.current().set_bias_tee(enabled)
    }

    fn start(&self) -> Result<(), SourceError> {
        self.current().start()
    }

    fn stop(&self) -> Result<(), SourceError> {
        self.current().stop()
    }

    fn set_gain(&self, stage: &str, db: f64) -> Result<(), SourceError> {
        self.current().set_gain(stage, db)
    }

    fn stats(&self) -> Option<SourceStats> {
        self.current().stats()
    }

    fn device_info(&self) -> Option<DeviceInfo> {
        self.current().device_info()
    }

    fn sweep_capability(&self) -> Option<SweepCapability> {
        self.current().sweep_capability()
    }

    fn start_sweep(&self, plan: &SweepPlan) -> Result<(), SourceError> {
        self.current().start_sweep(plan)
    }

    fn stop_sweep(&self) -> Result<(), SourceError> {
        self.current().stop_sweep()
    }
}

/// Scheduler state owned by the control thread.
pub(crate) struct SchedState {
    scheduler: Scheduler<SyntheticClock>,
    applier: StepApplier,
    switch: Arc<SwitchableControl>,
    generation: u64,
    counters: Arc<Counters>,
    step_end_ns: Option<i64>,
    recent: VecDeque<ScheduleStep>,
    verifs: HashMap<PoiKey, Verification<Cap>>,
    keys: HashMap<TrackId, PoiKey>,
    next_key: PoiKey,
    pairs: u8,
    verify: bool,
    /// T-115: records what each applied step observed (ADR-0012 §1).
    pub(crate) observer: Option<crate::observe::Observer>,
}

impl SchedState {
    /// Compiles `plan` for the source behind `switch`. Every step runs at the pipeline's rate `fs`
    /// (see the module docs); a non-controllable source (a replay) runs behind the
    /// [`ReplayGuard`].
    pub fn new(
        plan: &ScanPlan,
        switch: Arc<SwitchableControl>,
        fs: f64,
        t0: Timestamp,
        counters: Arc<Counters>,
        verify: bool,
    ) -> anyhow::Result<Self> {
        let caps = switch.capabilities().clone();
        let mut cfg = SchedulerConfig::from_plan(plan)?;
        cfg.sweep_rate_hz = fs;
        cfg.dwell_min_rate_hz = fs;
        cfg.max_span_hz = fs;
        let control: Arc<dyn SourceControl> = if caps.controllable {
            Arc::clone(&switch) as Arc<dyn SourceControl>
        } else {
            Arc::new(ReplayGuard::new(
                Arc::clone(&switch) as Arc<dyn SourceControl>,
                Arc::clone(&counters),
            ))
        };
        let pairs = cfg.gain_step_pairs;
        let scheduler = Scheduler::new(plan, cfg, &caps, SyntheticClock::new(t0))?;
        Ok(Self {
            scheduler,
            applier: StepApplier::new(control),
            generation: switch.generation(),
            switch,
            counters,
            step_end_ns: None,
            recent: VecDeque::with_capacity(64),
            verifs: HashMap::new(),
            keys: HashMap::new(),
            next_key: 1,
            pairs,
            verify,
            observer: None,
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

    /// The compiled plan (T-115: the observer's hop geometry).
    pub(crate) fn plan(&self) -> &hk_core::scheduler::CompiledPlan {
        self.scheduler.plan()
    }

    fn tick(&mut self, now_ns: i64) {
        if now_ns <= 0 {
            return;
        }
        // T-115: the single observer call (ADR-0012 §11): closes the previous step's record when
        // a new step was applied, and tracks the current step's settle.
        if let Some(o) = self.observer.as_mut() {
            o.tick(now_ns, self.recent.back());
        }
        self.scheduler
            .clock()
            .set(Timestamp::from_unix_nanos(now_ns));
        let generation = self.switch.generation();
        if generation != self.generation {
            // The source was reopened: it has none of the applied settings. Resend the current
            // step in full now, not only what the next step changes.
            self.generation = generation;
            self.applier.invalidate();
            if let Some(step) = self.recent.back().copied() {
                if self.applier.apply(&step).is_err() {
                    inc(&self.counters.scheduler.apply_errors);
                }
            }
        }
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use hk_core::source::{Duplex, FrequencyRange, SampleRates, SourceKind};
    use hk_model::sigmf::Datatype;

    use super::*;
    use crate::config::replay_plan;

    /// A source control that records the commands it receives (replay-like capabilities).
    struct Recorder {
        caps: SourceCapabilities,
        calls: Mutex<Vec<&'static str>>,
    }

    impl Recorder {
        fn new(center: f64, fs: f64) -> Arc<Self> {
            Arc::new(Self {
                caps: SourceCapabilities {
                    driver: "recorder".into(),
                    kind: SourceKind::Replay,
                    frequency_ranges: vec![FrequencyRange {
                        min_hz: center - fs / 2.0,
                        max_hz: center + fs / 2.0,
                    }],
                    sample_rates: SampleRates::Discrete(vec![fs]),
                    adc_bits: 8,
                    native_format: Datatype::Ci8,
                    duplex: Duplex::ReceiveOnly,
                    tx_capable: false,
                    controllable: false,
                    gain_stages: Vec::new(),
                    rf_amp: false,
                    baseband_filter: None,
                    bias_tee: false,
                    external_clock: false,
                    hardware_timestamps: false,
                    rf_path_boundaries_hz: Vec::new(),
                },
                calls: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }

        fn log(&self, call: &'static str) -> Result<(), SourceError> {
            self.calls.lock().unwrap().push(call);
            Ok(())
        }
    }

    impl SourceControl for Recorder {
        fn capabilities(&self) -> &SourceCapabilities {
            &self.caps
        }
        fn tune(&self, _: f64) -> Result<(), SourceError> {
            self.log("tune")
        }
        fn set_sample_rate(&self, _: f64) -> Result<(), SourceError> {
            self.log("set_sample_rate")
        }
        fn set_gains(&self, _: &Gains) -> Result<(), SourceError> {
            self.log("set_gains")
        }
        fn set_baseband_filter(&self, _: f64) -> Result<(), SourceError> {
            self.log("set_baseband_filter")
        }
        fn set_bias_tee(&self, _: bool) -> Result<(), SourceError> {
            self.log("set_bias_tee")
        }
        fn start(&self) -> Result<(), SourceError> {
            self.log("start")
        }
        fn stop(&self) -> Result<(), SourceError> {
            self.log("stop")
        }
    }

    /// T-037a item 3: after `--loop` reopens the source, the scheduler's commands reach the new
    /// source, and the current step (with its gains) is resent at once.
    #[test]
    fn a_reopened_source_gets_the_current_step_resent() {
        let (center, fs) = (433.5e6, 250e3);
        let t0 = Timestamp::from_unix_nanos(1_789_297_800_000_000_000);
        let first = Recorder::new(center, fs);
        let switch = Arc::new(SwitchableControl::new(
            Arc::clone(&first) as Arc<dyn SourceControl>
        ));
        let counters = Arc::new(Counters::default());
        let plan = replay_plan(center, fs, t0);
        let mut sched = SchedState::new(
            &plan,
            Arc::clone(&switch),
            fs,
            t0,
            Arc::clone(&counters),
            false,
        )
        .unwrap();
        let t = t0.as_unix_nanos();
        sched.tick(t + 1);
        assert!(first.calls().contains(&"set_gains"), "{:?}", first.calls());
        let before = first.calls().len();

        let second = Recorder::new(center, fs);
        switch.replace(Arc::clone(&second) as Arc<dyn SourceControl>);
        sched.tick(t + 2);
        assert!(
            second.calls().contains(&"set_gains"),
            "the reopened source gets the step's gains: {:?}",
            second.calls()
        );
        assert_eq!(
            first.calls().len(),
            before,
            "the old source gets nothing more"
        );
        assert_eq!(counters.scheduler.apply_errors.load(Ordering::Relaxed), 0);
    }
}
