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
//!   and time, decodes written for those tracks) reaches `record_outcome` once detection has caught
//!   up; a verification group's trust-test verdict reaches `report_verification`; arms are
//!   re-packed off `next_step` (`refresh_bandit`) at each step boundary. The provider is the run's
//!   C12 scorer (T-128, [`crate::attention::AttentionService`]): confirmed tracks, their members
//!   (SNR, suspect flags, bursts) and recipe matches feed its candidate table, a verification
//!   verdict marks the candidate trust-tested, and member suspect flags count in the dwell's
//!   `suspect_detections`.
//! - **API hub (T-127):** each step publishes a [`SchedulerHub`] snapshot (tier shares, arms,
//!   leases) and serves lease create/release commands for `/api/scheduler*`.
//! - **Source restarts** (`--loop`, T-037a): the scheduler controls the source through a
//!   [`SwitchableControl`]; when the capture thread reopens the source the pipeline points it at
//!   the new source's control, and the next tick resends the current step in full, so gain and
//!   filter steps keep applying after every restart.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use hk_core::scheduler::{
    ArmStatus, AttentionStatus, Lease, Poi, PoiKey, Purpose, ScheduleStep, Scheduler,
    SchedulerConfig, SchedulerError, StepApplier, SyntheticClock, Verification,
};
use hk_core::{
    DeviceInfo, Gains, SourceCapabilities, SourceControl, SourceError, SourceStats,
    SweepCapability, SweepPlan,
};
use hk_detect::CaptureResult;
use hk_model::FreqRange;
use hk_model::attention::schedule::{BanditConfig, DwellOutcome};
use hk_model::{ScanPlan, Timestamp, TrackId};

use crate::chains::{ChainManager, TrackDecodes};
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
    /// T-127: decodes per track (the segment's [`Shared::track_decodes`], set by [`run`]).
    track_decodes: Arc<TrackDecodes>,
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
        attention: Option<Arc<crate::attention::AttentionService>>,
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
                // T-128: the run's C12 scorer (a memory-only one without a run).
                let attention = match attention {
                    Some(a) => a,
                    None => Arc::new(crate::attention::AttentionService::in_memory()?),
                };
                scheduler
                    .enable_bandit(bcfg, attention.provider() as _)
                    .map_err(|e| anyhow::anyhow!("extra.bandit: {e}"))?;
                // T-251: the scheduler's own worst-case revisit sets the idle gap, and with it how
                // fast a stopped candidate's confidence decays and how long it stays a re-check
                // request. It has to come from how often this receiver looks: a gap shorter than
                // the revisit period would claim an absence nobody observed. A plan with no
                // revisit bound yields the conservative 60 s.
                attention.set_idle_gap(hk_model::IdleGap::from_revisit_s(
                    scheduler.plan().revisit_bound_ns as f64 / 1e9,
                ));
                Some(BanditWiring {
                    attention,
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
            track_decodes: Arc::default(),
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

    /// Serves queued lease commands, skipping any whose API call already gave up (503). Returns
    /// whether the scheduler cut or trimmed the running step, so the caller takes the next step
    /// now. A refused add, an unknown release or an unchanged renewal leave the running step and
    /// its accounting (sweep-floor window, coverage, bandit visits) alone (T-127 review).
    fn serve_hub(&mut self) -> bool {
        let Some(hub) = self.hub.clone() else {
            return false;
        };
        let cmds = std::mem::take(&mut *hub.inbox.lock().unwrap_or_else(PoisonError::into_inner));
        if cmds.is_empty() {
            return false;
        }
        let before = self.scheduler.running_end();
        for cmd in cmds {
            if !cmd.take() {
                continue;
            }
            match cmd.op {
                HubOp::Add(mut lease, reply) => {
                    if lease.id == 0 {
                        lease.id = self.scheduler.leases().map(|l| l.id).max().unwrap_or(0) + 1;
                    }
                    lease.rate_hz = self.fs;
                    let r = self
                        .scheduler
                        .add_lease(lease)
                        .map(|()| lease)
                        .map_err(|e| match e {
                            SchedulerError::TableFull(_) => HubError::TableFull(e.to_string()),
                            e => HubError::Refused(e.to_string()),
                        });
                    let _ = reply.try_send(r);
                }
                HubOp::Release(id, reply) => {
                    let _ = reply.try_send(self.scheduler.release_lease(id));
                }
            }
        }
        self.publish_hub();
        let cut = self.scheduler.running_end() < before;
        if cut {
            // A cut bandit dwell had its visit rolled back by the scheduler: drop its outcome too.
            if let (Some(step), Some(b)) = (self.recent.back(), self.bandit.as_mut()) {
                b.pending.retain(|p| p.step.seq != step.seq);
            }
        }
        cut
    }

    /// T-127/T-128: a member detection feeds the C12 candidate table and the outcome of the bandit dwell whose
    /// window and time contain it. A dwell cut by a lease never reaches here with its planned
    /// end: [`Self::serve_hub`] drops its outcome.
    ///
    /// Decode credit (T-127 review): a dwell counts the decodes written for the tracks it saw,
    /// from the track's first member in the dwell until its first member after the dwell (sample
    /// clock), or until the flush when no later member arrives. Limitation: decodes a chain
    /// writes after that later member for bursts inside the dwell are not credited.
    fn on_member(&mut self, track: TrackId, member: &MemberBox) {
        let usable = self.scheduler.config().usable_fraction;
        let Some(b) = self.bandit.as_mut() else {
            return;
        };
        let decodes = &self.track_decodes;
        let t = member.t_start.as_unix_nanos();
        let (new_track, novelty) = b.attention.on_track_member(track, &member_evidence(member));
        let center = 0.5 * (member.f_lo_hz + member.f_hi_hz);
        for p in &mut b.pending {
            let st = &p.step;
            let end = st.t_end().as_unix_nanos();
            if let Some(m) = p.tracks.iter_mut().find(|m| m.track == track) {
                if t >= end && m.frozen.is_none() {
                    m.frozen = Some(decodes.get(track));
                }
            }
            if !dwell_contains(st, usable, t, center) {
                continue;
            }
            if !member.continues {
                p.outcome.bursts += 1;
            }
            if member.suspect {
                p.outcome.suspect_detections += 1;
            }
            if !p.tracks.iter().any(|m| m.track == track) {
                p.tracks.push(TrackMark {
                    track,
                    base: decodes.get(track),
                    frozen: None,
                });
                p.outcome.novelty_sum += novelty;
                if new_track {
                    p.outcome.new_detections += 1;
                }
            }
        }
    }

    /// T-174 (ADR-0012 §2.6): a member [`Self::on_member`] counted as suspect had its DC flag
    /// refuted by a clean twin from another tuning. It leaves the candidate's suspect members and
    /// the suspect detections of the pending dwell that contains it, so it counts toward neither a
    /// suspect ban nor a suspect-only dwell. A dwell whose outcome was already recorded keeps it.
    fn on_member_refuted(&mut self, track: TrackId, member: &MemberBox) {
        let usable = self.scheduler.config().usable_fraction;
        let Some(b) = self.bandit.as_mut() else {
            return;
        };
        b.attention.on_track_member_refuted(track);
        let t = member.t_start.as_unix_nanos();
        let center = 0.5 * (member.f_lo_hz + member.f_hi_hz);
        for p in &mut b.pending {
            if dwell_contains(&p.step, usable, t, center) {
                p.outcome.suspect_detections = p.outcome.suspect_detections.saturating_sub(1);
            }
        }
    }

    /// T-127: bandit dwells detection has caught up with go to `record_outcome`.
    fn flush_outcomes(&mut self, now_ns: i64, force: bool) {
        let decodes = &self.track_decodes;
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
                u32::try_from(p.credited_decodes(decodes)).unwrap_or(u32::MAX);
            self.scheduler.record_outcome(&p.outcome);
        }
    }

    /// A confirmed track: a bandit candidate (with whether a chain recipe matches it) or a WRR POI.
    fn offer(&mut self, cand: &Candidate, recipe_match: bool) {
        if let Some(b) = self.bandit.as_mut() {
            if let Some(track) = cand.track {
                let freq = FreqRange::new(cand.f_lo_hz, cand.f_hi_hz.max(cand.f_lo_hz + 1.0));
                b.attention.on_track_confirmed(track, freq, recipe_match);
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
            b.attention.on_track_closed(track);
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
        if let Some(b) = self.bandit.as_ref() {
            b.attention
                .publish_candidates(Timestamp::from_unix_nanos(now_ns), false);
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
                if let Some(b) = self.bandit.as_ref() {
                    if let Some(pass) = eval.verdict() {
                        self.scheduler.report_verification(k, pass);
                        b.attention.on_trust_tested(k);
                    }
                    // T-128: republish so a still-suspect candidate is banned (§5.3).
                    b.attention.request_publish();
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
    attention: Arc<crate::attention::AttentionService>,
    pending: VecDeque<PendingOutcome>,
}

struct PendingOutcome {
    step: ScheduleStep,
    tracks: Vec<TrackMark>,
    outcome: DwellOutcome,
}

impl PendingOutcome {
    /// Decodes written for the dwell's own tracks over their credit windows (see
    /// [`SchedState::on_member`]); decodes on any other track never count.
    fn credited_decodes(&self, decodes: &TrackDecodes) -> u64 {
        self.tracks
            .iter()
            .map(|m| {
                m.frozen
                    .unwrap_or_else(|| decodes.get(m.track))
                    .saturating_sub(m.base)
            })
            .sum()
    }
}

/// A track a pending dwell saw, with its decode count when the dwell first saw it and when a
/// later member (after the dwell) arrived.
struct TrackMark {
    track: TrackId,
    base: u64,
    frozen: Option<u64>,
}

/// A member at `t_ns` centred on `center_hz` falls inside the dwell `st` (its time and usable
/// window).
fn dwell_contains(st: &ScheduleStep, usable: f64, t_ns: i64, center_hz: f64) -> bool {
    let half = 0.5 * st.rate_hz * usable;
    st.t_start.as_unix_nanos() <= t_ns
        && t_ns < st.t_end().as_unix_nanos()
        && (center_hz - st.center_hz).abs() <= half
}

/// A member box as C12 candidate evidence (T-128).
fn member_evidence(m: &MemberBox) -> crate::candidates::MemberEvidence {
    crate::candidates::MemberEvidence {
        f_lo_hz: m.f_lo_hz,
        f_hi_hz: m.f_hi_hz,
        t_ns: m.t_start.as_unix_nanos(),
        continues: m.continues,
        snr_db: Some(m.snr_db).filter(|s| s.is_finite()),
        suspect: m.suspect,
    }
}

// ---------------------------------------------------------------------------------------------
// T-127 API hub.
// ---------------------------------------------------------------------------------------------

/// How long an API lease command waits for the control thread.
const HUB_REPLY_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_millis(100)
} else {
    Duration::from_secs(2)
};

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
    /// The control thread did not answer in time. The command was cancelled: it never applies.
    Busy,
    /// The scheduler refused it.
    Refused(String),
    /// The lease table is full.
    TableFull(String),
}

/// A queued command and its hand-off state ([`CMD_QUEUED`] → taken by the control thread, or
/// cancelled by an API call that gave up).
struct HubCmd {
    op: HubOp,
    state: Arc<AtomicU8>,
}

enum HubOp {
    Add(Lease, SyncSender<Result<Lease, HubError>>),
    Release(u64, SyncSender<bool>),
}

const CMD_QUEUED: u8 = 0;
const CMD_TAKEN: u8 = 1;
const CMD_CANCELLED: u8 = 2;

impl HubCmd {
    /// Claims the command for the control thread; false when its caller already gave up.
    fn take(&self) -> bool {
        self.state
            .compare_exchange(CMD_QUEUED, CMD_TAKEN, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }
}

impl SchedulerHub {
    /// The latest snapshot; `None` when no scheduler is running.
    pub fn snapshot(&self) -> Option<Arc<HubSnapshot>> {
        self.snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Queues a command and waits for its answer. On timeout the command is cancelled, so an
    /// API call that answered 503 never takes effect later (before the first sample, too); if the
    /// control thread took it at that moment, its answer is awaited instead.
    fn send<T>(&self, make: impl FnOnce(SyncSender<T>) -> HubOp) -> Result<T, HubError> {
        if self.snapshot().is_none() {
            return Err(HubError::NoScheduler);
        }
        let (tx, rx) = sync_channel(1);
        let state = Arc::new(AtomicU8::new(CMD_QUEUED));
        self.inbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(HubCmd {
                op: make(tx),
                state: Arc::clone(&state),
            });
        match rx.recv_timeout(HUB_REPLY_TIMEOUT) {
            Ok(v) => Ok(v),
            Err(_)
                if state
                    .compare_exchange(
                        CMD_QUEUED,
                        CMD_CANCELLED,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    )
                    .is_ok() =>
            {
                Err(HubError::Busy)
            }
            Err(_) => rx.recv().map_err(|_| HubError::Busy),
        }
    }

    /// Adds or updates a lease (`id` 0 assigns the next free id; the rate is the pipeline's).
    pub fn add_lease(&self, lease: Lease) -> Result<Lease, HubError> {
        self.send(|tx| HubOp::Add(lease, tx))?
    }

    /// Releases a lease. Returns whether it was active.
    pub fn release_lease(&self, id: u64) -> Result<bool, HubError> {
        self.send(|tx| HubOp::Release(id, tx))
    }
}

/// Runs until the detection reader has finished and every chain has been reaped.
pub(crate) fn run(
    shared: Arc<Shared>,
    rx: Receiver<ControlEvent>,
    mut sched: Option<SchedState>,
    mut interactive: Option<crate::observe::InteractiveObserver>,
    attention: Option<Arc<crate::attention::AttentionService>>,
) -> anyhow::Result<()> {
    let mut chains = ChainManager::new(Arc::clone(&shared));
    if let Some(s) = sched.as_mut() {
        s.track_decodes = Arc::clone(&shared.track_decodes);
        if let Some(b) = s.bandit.as_ref() {
            b.attention
                .set_track_decodes(Arc::clone(&shared.track_decodes));
        }
    }
    // T-131: without the bandit (the default) nothing else feeds the C12 candidate table, so the
    // control thread does, read-only: `/api/candidates` is populated and nothing schedules from it.
    let passive = attention.filter(|_| sched.as_ref().is_none_or(|s| s.bandit.is_none()));
    if let Some(a) = &passive {
        a.set_track_decodes(Arc::clone(&shared.track_decodes));
    }
    let mut detect_done = false;
    loop {
        match rx.recv_timeout(Duration::from_millis(2)) {
            Ok(ev) => match ev {
                ControlEvent::TrackConfirmed(cand) => {
                    if sched.is_some() || passive.is_some() {
                        let recipe = crate::chains::spec::select_for_track(
                            &shared.specs,
                            cand.f_lo_hz,
                            cand.f_hi_hz,
                            cand.bursty,
                        )
                        .is_some();
                        if let Some(s) = sched.as_mut() {
                            s.offer(&cand, recipe);
                        }
                        if let (Some(a), Some(track)) = (&passive, cand.track) {
                            let freq =
                                FreqRange::new(cand.f_lo_hz, cand.f_hi_hz.max(cand.f_lo_hz + 1.0));
                            a.on_track_confirmed(track, freq, recipe);
                        }
                    }
                    chains.on_confirmed(cand);
                }
                ControlEvent::Member { track, member } => {
                    if let Some(s) = sched.as_mut() {
                        s.on_member(track, &member);
                    }
                    if let Some(a) = &passive {
                        a.on_track_member(track, &member_evidence(&member));
                    }
                    chains.on_member(track, member);
                }
                ControlEvent::MemberRefuted { track, member } => {
                    if let Some(s) = sched.as_mut() {
                        s.on_member_refuted(track, &member);
                    }
                    if let Some(a) = &passive {
                        a.on_track_member_refuted(track);
                    }
                }
                ControlEvent::TrackClosed { track, .. } => {
                    if let Some(s) = sched.as_mut() {
                        s.remove(track);
                    }
                    if let Some(a) = &passive {
                        a.on_track_closed(track);
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
            let now_ns = shared.counters.stream_time_ns.load(Ordering::Relaxed);
            if let Some(s) = sched.as_mut() {
                s.tick(now_ns);
            }
            // Every 10 s of sample clock (at once when a confirmed track appeared or closed).
            if let (Some(a), true) = (&passive, now_ns > 0) {
                a.publish_candidates(Timestamp::from_unix_nanos(now_ns), false);
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
                    tuning_step: hk_core::TuningStep::Unknown,
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

    const CENTER: f64 = 433.5e6;

    /// A replay-like scheduler (`bandit` sets `extra.bandit`) with a hub attached; its t0 in ns.
    fn sched_with_hub(bandit: bool) -> (SchedState, Arc<SchedulerHub>, Arc<Counters>, i64) {
        let fs = 250e3;
        let t0 = Timestamp::from_unix_nanos(1_789_297_800_000_000_000);
        let switch = Arc::new(SwitchableControl::new(
            Recorder::new(CENTER, fs) as Arc<dyn SourceControl>
        ));
        let counters = Arc::new(Counters::default());
        let mut plan = replay_plan(CENTER, fs, t0);
        if bandit {
            plan.extra["bandit"] = serde_json::Value::Bool(true);
        }
        let mut s =
            SchedState::new(&plan, switch, fs, t0, Arc::clone(&counters), false, None).unwrap();
        let hub = Arc::new(SchedulerHub::default());
        s.attach_hub(Arc::clone(&hub));
        (s, hub, counters, t0.as_unix_nanos())
    }

    /// Queues a command as the API would, without blocking on its answer.
    fn queue(hub: &SchedulerHub, op: HubOp) {
        hub.inbox.lock().unwrap().push(HubCmd {
            op,
            state: Arc::new(AtomicU8::new(CMD_QUEUED)),
        });
    }

    fn pin(duration_ns: Option<i64>) -> Lease {
        Lease {
            id: 0,
            kind: hk_model::attention::observation::LeaseKind::UserPin,
            center_hz: CENTER,
            rate_hz: 0.0,
            gains: None,
            duration_ns,
        }
    }

    fn not_other_s(st: &AttentionStatus) -> f64 {
        st.discovery_s + st.exploit_s + st.explore_s
    }

    /// T-127 review: a refused POST and a DELETE of an unknown lease leave the running step and
    /// its accounting unchanged (no new step is taken early).
    #[test]
    fn refused_or_unknown_lease_commands_leave_the_running_step_alone() {
        let (mut s, hub, counters, t) = sched_with_hub(false);
        s.tick(t + 1);
        let end = s.step_end_ns.expect("a running step");
        assert!(end > t + 4);
        let steps = counters.scheduler.steps.load(Ordering::Relaxed);
        let before = s.scheduler.attention_status();
        let (add_tx, add_rx) = sync_channel(1);
        queue(&hub, HubOp::Add(pin(Some(0)), add_tx));
        let (rel_tx, rel_rx) = sync_channel(1);
        queue(&hub, HubOp::Release(99, rel_tx));
        s.tick(t + 2);
        assert!(matches!(add_rx.try_recv(), Ok(Err(HubError::Refused(_)))));
        assert_eq!(rel_rx.try_recv(), Ok(false));
        assert_eq!(s.step_end_ns, Some(end));
        assert_eq!(s.scheduler.running_end().as_unix_nanos(), end);
        assert_eq!(counters.scheduler.steps.load(Ordering::Relaxed), steps);
        let after = s.scheduler.attention_status();
        assert_eq!(not_other_s(&after), not_other_s(&before));
        assert_eq!(after.other_s, before.other_s);
    }

    /// T-127 review: an accepted lease cuts the running step at once, and its accounting keeps
    /// only the time it actually observed.
    #[test]
    fn an_accepted_lease_trims_the_running_step_to_its_observed_time() {
        let (mut s, hub, counters, t) = sched_with_hub(false);
        s.tick(t + 1);
        let end = s.step_end_ns.expect("a running step");
        let before = not_other_s(&s.scheduler.attention_status());
        let mid = t + 1 + (end - t - 1) / 2;
        let (tx, rx) = sync_channel(1);
        queue(&hub, HubOp::Add(pin(None), tx));
        s.tick(mid);
        let lease = rx.try_recv().unwrap().unwrap();
        assert!(lease.id >= 1);
        let unrun = (end - mid) as f64 / 1e9;
        let after = not_other_s(&s.scheduler.attention_status());
        assert!(
            (before - after - unrun).abs() < 1e-6,
            "{before} {after} {unrun}"
        );
        let step = s.recent.back().unwrap();
        assert!(matches!(step.purpose, Purpose::Lease { .. }), "{step:?}");
        assert_eq!(step.t_start.as_unix_nanos(), mid);
        assert_eq!(counters.scheduler.steps.load(Ordering::Relaxed), 2);
    }

    /// T-127 review: a command whose API call timed out (503) never applies later, including one
    /// sent before the first sample.
    #[test]
    fn a_lease_command_that_timed_out_never_applies() {
        let (mut s, hub, _counters, t) = sched_with_hub(false);
        assert_eq!(hub.add_lease(pin(None)), Err(HubError::Busy));
        s.tick(t + 1);
        assert_eq!(s.scheduler.leases().count(), 0);
        assert!(!matches!(
            s.recent.back().map(|st| st.purpose),
            Some(Purpose::Lease { .. })
        ));
    }

    /// T-127 review: continuous decodes on an unrelated track, and decodes on the dwell's own
    /// track after its next member past the dwell, never raise the dwell's `valid_decodes`.
    #[test]
    fn unrelated_track_decodes_do_not_reward_a_dwell() {
        let (mut s, _hub, _counters, t) = sched_with_hub(true);
        s.tick(t + 1);
        let step = *s.recent.back().unwrap();
        let end = step.t_end().as_unix_nanos();
        assert!(end > t + 3);
        let arm = s.scheduler.arm_key_of(&step);
        let b = s.bandit.as_mut().expect("the bandit is on");
        b.pending.clear();
        b.pending.push_back(PendingOutcome {
            step,
            tracks: Vec::new(),
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
        let member = |t_ns: i64, center: f64| MemberBox {
            detection: hk_model::DetectionId::new(),
            samples: 0..1,
            f_lo_hz: center - 1e3,
            f_hi_hz: center + 1e3,
            t_start: Timestamp::from_unix_nanos(t_ns),
            continues: false,
            snr_db: 20.0,
            suspect: false,
        };
        let (ours, other) = (TrackId::new(), TrackId::new());
        let decodes = Arc::clone(&s.track_decodes);
        decodes.add(Some(ours), 3);
        decodes.add(Some(other), 7);
        s.on_member(ours, &member(t + 2, step.center_hz));
        // The unrelated track is outside the dwell's window; it decodes continuously.
        s.on_member(other, &member(t + 2, step.center_hz + step.rate_hz));
        decodes.add(Some(other), 100);
        decodes.add(Some(ours), 2);
        // The dwell's track shows up again after the dwell: later decodes are a later look's.
        s.on_member(ours, &member(end + 10, step.center_hz));
        decodes.add(Some(ours), 50);
        decodes.add(Some(other), 100);
        let p = &s.bandit.as_ref().unwrap().pending[0];
        assert_eq!(p.tracks.len(), 1);
        assert_eq!(p.credited_decodes(&decodes), 2);
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
            None,
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
