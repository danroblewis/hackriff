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
//! - **Bandit (T-127, ADR-0012 §5), off by default:** a plan with `extra.bandit` (`true`, or an
//!   object overriding [`BanditConfig`] fields) enables the bandit revisit policy. Confirmed tracks
//!   then feed an interestingness provider ([`SharedInterestingness`]) instead of WRR POIs; each
//!   bandit dwell's outcome (new tracks, bursts and novelty of member detections inside its window
//!   and time, decode rows written meanwhile) reaches `record_outcome` once detection has caught
//!   up; a verification group's trust-test verdict reaches `report_verification`; arms are
//!   re-packed off `next_step` (`refresh_bandit`) at each step boundary. The provider is a
//!   **minimal stub** until T-119's C12 scorer publishes through the same handle.
//! - **API hub (T-127):** each step publishes a [`SchedulerHub`] snapshot (tier shares, arms,
//!   leases) and serves lease create/release commands for `/api/scheduler*`.
//! - **Source restarts** (`--loop`, T-037a): the scheduler controls the source through a
//!   [`SwitchableControl`]; when the capture thread reopens the source the pipeline points it at
//!   the new source's control, and the next tick resends the current step in full, so gain and
//!   filter steps keep applying after every restart.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use hk_core::scheduler::{
    ArmStatus, AttentionStatus, Lease, Poi, PoiKey, Purpose, ScheduleStep, Scheduler,
    SchedulerConfig, StepApplier, SyntheticClock, Verification,
};
use hk_core::{
    DeviceInfo, Gains, SourceCapabilities, SourceControl, SourceError, SourceStats,
    SweepCapability, SweepPlan,
};
use hk_detect::CaptureResult;
use hk_model::FreqRange;
use hk_model::attention::baseline::{BaselineResolution, Maturity};
use hk_model::attention::schedule::{BanditConfig, DwellOutcome};
use hk_model::attention::score::{
    Candidate as Scored, CandidateSet, CandidateSubject, NoveltyScore, ScoreComponents,
    ScoreWeights, SharedInterestingness, interestingness, normalised,
};
use hk_model::{ScanPlan, Timestamp, TrackId};

use crate::chains::ChainManager;
use crate::events::{Candidate, ControlEvent, MemberBox};
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
    /// T-127: the bandit's provider, stub scorer and pending dwell outcomes (bandit on).
    bandit: Option<BanditWiring>,
    /// T-127: the API hub.
    hub: Option<Arc<SchedulerHub>>,
    /// The pipeline's rate (every step's, leases included).
    fs: f64,
    regions: Vec<FreqRange>,
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
        let mut scheduler = Scheduler::new(plan, cfg, &caps, SyntheticClock::new(t0))?;
        let bandit = match bandit_config(plan)? {
            Some(bcfg) => {
                let provider = Arc::new(SharedInterestingness::default());
                scheduler
                    .enable_bandit(bcfg, Arc::clone(&provider) as _)
                    .map_err(|e| anyhow::anyhow!("extra.bandit: {e}"))?;
                Some(BanditWiring {
                    stub: StubInterestingness::new(provider),
                    pending: VecDeque::with_capacity(MAX_PENDING_OUTCOMES),
                })
            }
            None => None,
        };
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
            bandit,
            hub: None,
            fs,
            regions: plan.regions.iter().map(|r| r.freq).collect(),
        })
    }

    /// T-127: publishes snapshots to `hub` and serves its lease commands.
    pub(crate) fn attach_hub(&mut self, hub: Arc<SchedulerHub>) {
        self.hub = Some(hub);
        self.publish_hub();
    }

    fn publish_hub(&self) {
        let Some(hub) = &self.hub else { return };
        let snap = HubSnapshot {
            status: self.scheduler.attention_status(),
            arms: self.scheduler.arm_table(),
            regions: self.regions.clone(),
            leases: self.scheduler.leases().copied().collect(),
            plan_version: self.scheduler.plan().plan_version,
        };
        *hub.snapshot.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(snap));
    }

    /// Serves queued lease commands. Returns whether there were any.
    fn serve_hub(&mut self) -> bool {
        let Some(hub) = self.hub.clone() else {
            return false;
        };
        let cmds = std::mem::take(&mut *hub.inbox.lock().unwrap_or_else(PoisonError::into_inner));
        let changed = !cmds.is_empty();
        for cmd in cmds {
            match cmd {
                HubCmd::Add(mut lease, reply) => {
                    if lease.id == 0 {
                        lease.id = self.scheduler.leases().map(|l| l.id).max().unwrap_or(0) + 1;
                    }
                    lease.rate_hz = self.fs;
                    let r = self
                        .scheduler
                        .add_lease(lease)
                        .map(|()| lease)
                        .map_err(|e| e.to_string());
                    let _ = reply.try_send(r);
                }
                HubCmd::Release(id, reply) => {
                    let _ = reply.try_send(self.scheduler.release_lease(id));
                }
            }
        }
        if changed {
            self.publish_hub();
        }
        changed
    }

    /// T-127: a member detection feeds the stub scorer and the outcome of the bandit dwell whose
    /// window and time contain it.
    fn on_member(&mut self, track: TrackId, member: &MemberBox) {
        let usable = self.scheduler.config().usable_fraction;
        let Some(b) = self.bandit.as_mut() else {
            return;
        };
        let t = member.t_start.as_unix_nanos();
        let (new_track, novelty) = b.stub.on_member(track, member);
        let center = 0.5 * (member.f_lo_hz + member.f_hi_hz);
        for p in &mut b.pending {
            let st = &p.step;
            let half = 0.5 * st.rate_hz * usable;
            let inside = st.t_start.as_unix_nanos() <= t
                && t < st.t_end().as_unix_nanos()
                && (center - st.center_hz).abs() <= half;
            if !inside {
                continue;
            }
            if !member.continues {
                p.outcome.bursts += 1;
            }
            if !p.tracks.contains(&track) {
                p.tracks.push(track);
                p.outcome.novelty_sum += novelty;
                if new_track {
                    p.outcome.new_detections += 1;
                }
            }
        }
    }

    /// T-127: bandit dwells detection has caught up with go to `record_outcome`.
    fn flush_outcomes(&mut self, now_ns: i64, force: bool) {
        let decodes = self.counters.chains.decodes.load(Ordering::Relaxed);
        let Some(b) = self.bandit.as_mut() else {
            return;
        };
        while let Some(p) = b.pending.front() {
            let next_start = self
                .recent
                .iter()
                .find(|s| s.seq == p.step.seq + 1)
                .map(|s| s.t_start.as_unix_nanos());
            let end = p
                .step
                .t_end()
                .as_unix_nanos()
                .min(next_start.unwrap_or(i64::MAX));
            if !force && now_ns < end.saturating_add(OUTCOME_GRACE_NS) {
                break;
            }
            let Some(mut p) = b.pending.pop_front() else {
                break;
            };
            p.outcome.dwell_s = (end - p.step.t_start.as_unix_nanos()).max(0) as f64 / 1e9;
            p.outcome.valid_decodes =
                u32::try_from(decodes.saturating_sub(p.decodes_at_start)).unwrap_or(u32::MAX);
            self.scheduler.record_outcome(&p.outcome);
        }
    }

    fn offer(&mut self, cand: &Candidate) {
        if let Some(b) = self.bandit.as_mut() {
            if let Some(track) = cand.track {
                b.stub.on_confirmed(track, cand);
            }
            return;
        }
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
        if let Some(b) = self.bandit.as_mut() {
            b.stub.on_closed(track);
        }
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
        if self.serve_hub() {
            // A lease was added or released: the scheduler already cut or trimmed the running
            // step; take the next one now.
            self.step_end_ns = None;
        }
        self.flush_outcomes(now_ns, false);
        if self.step_end_ns.is_some_and(|end| now_ns < end) {
            return;
        }
        if let Some(b) = self.bandit.as_mut() {
            b.stub.maybe_publish(now_ns);
        }
        // Packing allocates, so it happens here, off `next_step` (ADR-0012 §5.1).
        self.scheduler.refresh_bandit();
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
                let mut eval = TrustEval::new(c, &counters.verdicts, track, t);
                let report = v.evaluate(&mut eval);
                // T-127: the bandit's suspect candidates learn their trust-test verdict.
                if let (Some(pass), true) = (eval.verdict(), self.bandit.is_some()) {
                    self.scheduler.report_verification(k, pass);
                }
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
        if let (Purpose::Bandit { .. }, true) = (step.purpose, self.bandit.is_some()) {
            let arm = self.scheduler.arm_key_of(&step);
            let decodes_at_start = counters.chains.decodes.load(Ordering::Relaxed);
            if self
                .bandit
                .as_ref()
                .is_some_and(|b| b.pending.len() >= MAX_PENDING_OUTCOMES)
            {
                self.flush_outcomes(now_ns, true);
            }
            if let Some(b) = self.bandit.as_mut() {
                b.pending.push_back(PendingOutcome {
                    step,
                    tracks: Vec::new(),
                    decodes_at_start,
                    outcome: DwellOutcome {
                        seq: step.seq,
                        arm,
                        dwell_s: 0.0,
                        new_detections: 0,
                        bursts: 0,
                        novelty_sum: 0.0,
                        valid_decodes: 0,
                        suspect_detections: 0,
                    },
                });
            }
        }
        self.publish_hub();
    }
}

impl Drop for SchedState {
    fn drop(&mut self) {
        if let Some(hub) = &self.hub {
            *hub.snapshot.lock().unwrap_or_else(PoisonError::into_inner) = None;
        }
    }
}

/// `extra.bandit`: absent, `null` or `false` keeps the bandit off (the default); `true` takes the
/// defaults; an object overrides [`BanditConfig`] fields.
pub fn bandit_config(plan: &ScanPlan) -> anyhow::Result<Option<BanditConfig>> {
    use serde_json::Value;
    let cfg = match plan.extra.get("bandit") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => return Ok(None),
        Some(Value::Bool(true)) => BanditConfig::default(),
        Some(Value::Object(over)) => {
            let mut v = serde_json::to_value(BanditConfig::default())?;
            if let Some(base) = v.as_object_mut() {
                for (k, x) in over {
                    base.insert(k.clone(), x.clone());
                }
            }
            serde_json::from_value(v).map_err(|e| anyhow::anyhow!("extra.bandit: {e}"))?
        }
        Some(other) => anyhow::bail!("extra.bandit must be a boolean or an object, not {other}"),
    };
    cfg.validate()
        .map_err(|e| anyhow::anyhow!("extra.bandit: {e}"))?;
    Ok(Some(cfg))
}

/// Bandit dwells awaiting their outcome, at most.
const MAX_PENDING_OUTCOMES: usize = 32;
/// Stream time after a bandit dwell ends before its outcome is recorded (detection latency).
const OUTCOME_GRACE_NS: i64 = 1_000_000_000;

struct BanditWiring {
    stub: StubInterestingness,
    pending: VecDeque<PendingOutcome>,
}

struct PendingOutcome {
    step: ScheduleStep,
    tracks: Vec<TrackId>,
    decodes_at_start: u64,
    outcome: DwellOutcome,
}

// ---------------------------------------------------------------------------------------------
// T-127 STUB interestingness provider (ADR-0012 §5.6). T-119 replaces this publisher with the
// C12 scorer (baselines, novelty, occupancy); the scheduler keeps the same
// `SharedInterestingness` handle. Deliberately tiny: blind confirmed tracks only, novelty from
// "first seen within the last hour", no SNR, never suspect.
// ---------------------------------------------------------------------------------------------

/// Re-scoring interval for member-only changes, ns (ADR-0012 §0 "10 s re-scoring").
const STUB_RESCORE_NS: i64 = 10_000_000_000;
/// Novelty fades to 0 over this age, s.
const STUB_NOVELTY_WINDOW_S: f64 = 3600.0;

struct StubTrack {
    f_lo_hz: f64,
    f_hi_hz: f64,
    first_ns: i64,
    last_ns: i64,
    bursts: u32,
    confirmed: bool,
}

struct StubInterestingness {
    provider: Arc<SharedInterestingness>,
    tracks: HashMap<TrackId, StubTrack>,
    /// A track was confirmed or closed since the last publish (publish at once).
    changed: bool,
    /// Members of confirmed tracks updated since the last publish (publish at the re-scoring
    /// interval).
    dirty: bool,
    last_publish_ns: i64,
    now_ns: i64,
}

impl StubInterestingness {
    fn new(provider: Arc<SharedInterestingness>) -> Self {
        Self {
            provider,
            tracks: HashMap::new(),
            changed: false,
            dirty: false,
            last_publish_ns: i64::MIN / 2,
            now_ns: 0,
        }
    }

    fn novelty(&self, t: &StubTrack) -> f64 {
        let age_s = (self.now_ns - t.first_ns).max(0) as f64 / 1e9;
        (1.0 - age_s / STUB_NOVELTY_WINDOW_S).clamp(0.0, 1.0)
    }

    /// Returns whether the track is new to the stub, and its novelty.
    fn on_member(&mut self, track: TrackId, m: &MemberBox) -> (bool, f64) {
        let t = m.t_start.as_unix_nanos();
        self.now_ns = self.now_ns.max(t);
        let fresh = !self.tracks.contains_key(&track);
        let e = self.tracks.entry(track).or_insert(StubTrack {
            f_lo_hz: m.f_lo_hz,
            f_hi_hz: m.f_hi_hz,
            first_ns: t,
            last_ns: t,
            bursts: 0,
            confirmed: false,
        });
        e.f_lo_hz = e.f_lo_hz.min(m.f_lo_hz);
        e.f_hi_hz = e.f_hi_hz.max(m.f_hi_hz);
        e.first_ns = e.first_ns.min(t);
        e.last_ns = e.last_ns.max(t);
        if !m.continues {
            e.bursts += 1;
        }
        self.dirty |= e.confirmed;
        let novelty = self.tracks.get(&track).map_or(0.0, |e| self.novelty(e));
        (fresh, novelty)
    }

    fn on_confirmed(&mut self, track: TrackId, c: &Candidate) {
        let now = self.now_ns;
        let e = self.tracks.entry(track).or_insert(StubTrack {
            f_lo_hz: c.f_lo_hz,
            f_hi_hz: c.f_hi_hz,
            first_ns: now,
            last_ns: now,
            bursts: 0,
            confirmed: false,
        });
        e.confirmed = true;
        self.changed = true;
    }

    fn on_closed(&mut self, track: TrackId) {
        if self.tracks.remove(&track).is_some_and(|t| t.confirmed) {
            self.changed = true;
        }
    }

    fn maybe_publish(&mut self, now_ns: i64) {
        self.now_ns = self.now_ns.max(now_ns);
        let due = self.dirty && now_ns.saturating_sub(self.last_publish_ns) >= STUB_RESCORE_NS;
        if !(self.changed || due) {
            return;
        }
        let weights = ScoreWeights::default();
        let mut candidates: Vec<Scored> = self
            .tracks
            .iter()
            .filter(|(_, t)| t.confirmed)
            .map(|(id, t)| {
                let novelty = self.novelty(t);
                let components = ScoreComponents {
                    snr_db: None,
                    novelty,
                    class_entropy: None,
                    decoder_available: false,
                    periodicity: None,
                    boring_prior: 0.0,
                };
                let score = interestingness(&weights, &components);
                let span_s = (t.last_ns - t.first_ns).max(0) as f64 / 1e9;
                Scored {
                    subject: CandidateSubject::Track { id: *id },
                    freq: FreqRange::new(t.f_lo_hz, t.f_hi_hz.max(t.f_lo_hz + 1.0)),
                    score,
                    score_norm: normalised(&weights, score),
                    components,
                    novelty: NoveltyScore {
                        novelty,
                        level_z: None,
                        occupancy_z: None,
                        new_emitter: Some(novelty),
                        observed_s: span_s,
                        maturity: Maturity::Mature {
                            resolution: BaselineResolution::AllHours,
                        },
                        provenance_explained: false,
                    },
                    suspect_fraction: 0.0,
                    needs_verification: false,
                    expected_interval_s: (t.bursts >= 2 && span_s > 0.0)
                        .then(|| span_s / f64::from(t.bursts - 1)),
                    min_on_off_s: None,
                    next_burst_eta: None,
                }
            })
            .collect();
        candidates.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then(a.freq.lo_hz.total_cmp(&b.freq.lo_hz))
        });
        let mut set = CandidateSet::empty(Timestamp::from_unix_nanos(self.now_ns));
        set.weights = weights;
        set.candidates = candidates;
        // A stub set that fails validation is not published (the bandit keeps the last one).
        let _ = self.provider.publish(set);
        self.changed = false;
        self.dirty = false;
        self.last_publish_ns = now_ns;
    }
}

// ---------------------------------------------------------------------------------------------
// T-127 API hub.
// ---------------------------------------------------------------------------------------------

/// How long an API lease command waits for the control thread.
const HUB_REPLY_TIMEOUT: Duration = Duration::from_secs(2);

/// The scheduler as the API sees it: the control thread's latest snapshot and a lease command
/// inbox it serves at each tick. Empty (no snapshot) when the run has no scheduler.
#[derive(Default)]
pub struct SchedulerHub {
    snapshot: Mutex<Option<Arc<HubSnapshot>>>,
    inbox: Mutex<Vec<HubCmd>>,
}

/// One published scheduler snapshot.
#[derive(Clone, Debug)]
pub struct HubSnapshot {
    /// Tier shares, sweep floor, bandit summary.
    pub status: AttentionStatus,
    /// Bandit arm table (empty without the bandit).
    pub arms: Vec<ArmStatus>,
    /// The plan's regions.
    pub regions: Vec<FreqRange>,
    /// Active leases.
    pub leases: Vec<Lease>,
    /// Plan version.
    pub plan_version: u32,
}

/// Why a hub command failed.
#[derive(Clone, Debug, PartialEq)]
pub enum HubError {
    /// This run has no scheduler.
    NoScheduler,
    /// The control thread did not answer in time.
    Busy,
    /// The scheduler refused it.
    Refused(String),
}

enum HubCmd {
    Add(Lease, SyncSender<Result<Lease, String>>),
    Release(u64, SyncSender<bool>),
}

impl SchedulerHub {
    /// The latest snapshot; `None` when no scheduler is running.
    pub fn snapshot(&self) -> Option<Arc<HubSnapshot>> {
        self.snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn send<T>(&self, make: impl FnOnce(SyncSender<T>) -> HubCmd) -> Result<T, HubError> {
        if self.snapshot().is_none() {
            return Err(HubError::NoScheduler);
        }
        let (tx, rx) = sync_channel(1);
        self.inbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(make(tx));
        rx.recv_timeout(HUB_REPLY_TIMEOUT)
            .map_err(|_| HubError::Busy)
    }

    /// Adds or updates a lease (`id` 0 assigns the next free id; the rate is the pipeline's).
    pub fn add_lease(&self, lease: Lease) -> Result<Lease, HubError> {
        self.send(|tx| HubCmd::Add(lease, tx))?
            .map_err(HubError::Refused)
    }

    /// Releases a lease. Returns whether it was active.
    pub fn release_lease(&self, id: u64) -> Result<bool, HubError> {
        self.send(|tx| HubCmd::Release(id, tx))
    }
}

/// Runs until the detection reader has finished and every chain has been reaped.
pub(crate) fn run(
    shared: Arc<Shared>,
    rx: Receiver<ControlEvent>,
    mut sched: Option<SchedState>,
    mut interactive: Option<crate::observe::InteractiveObserver>,
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
                ControlEvent::Member { track, member } => {
                    if let Some(s) = sched.as_mut() {
                        s.on_member(track, &member);
                    }
                    chains.on_member(track, member);
                }
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
            // T-115: a live run without the scheduler logs its interactive tuning.
            if let Some(o) = interactive.as_mut() {
                o.tick();
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
