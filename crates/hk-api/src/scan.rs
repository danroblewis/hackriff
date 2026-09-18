//! The in-app survey sweep (T-452): T-406's iterative scan, started and stopped from the running
//! server, stepping the **interactive** front end.
//!
//! # `hk serve` still does not drive the scheduler, and that is the decision
//!
//! `crate::control`'s five device routes exist because `hk serve` is *interactive tuning*: the run
//! is composed without a scheduler (`schedule: false`), and the front end moves when something
//! **acts** on it. T-406 built the iterative scan as a dwell policy *over the scheduler*, so the
//! obvious way to reach it from the app would be to turn the scheduler on. This module does not
//! do that, for three reasons that are worth stating because deleting the comment in `serve.rs`
//! would have been the easy way to get a green test:
//!
//! 1. **The scheduler is chosen when a segment is composed, not while it runs.** `hk_pipeline`
//!    builds either a scheduler *or* the interactive observer for a segment; flipping between them
//!    mid-run is a second composition path, which is precisely what T-406 refused to add.
//! 2. **The coverage record already has the right shape without it.** T-406's load-bearing finding
//!    was that a dwell step must write *one record per step with its true band and interval*,
//!    because a coarse record spanning a pass rasterises as "the whole band, the whole time". The
//!    interactive run already does exactly that: `hk_pipeline`'s `InteractiveObserver` closes one
//!    dwell record per **steady tune**, with that tune's own window and its own interval. A sweep
//!    that steps the interactive tune therefore writes one true record per step through the
//!    accumulator that already exists — `hk_store::coverage::spans_from_records` reads a dwell
//!    record by its window and interval, not by its reason — so the canvas lights up from the same
//!    coverage plane, with **no second accumulator**.
//! 3. **It keeps one rule for who owns the radio.** A scan built out of acts can be arbitrated
//!    against the user's acts by a single rule (below). A scan built out of a scheduler would need
//!    the scheduler's tier arbitration *and* the interactive path's, in a process where only one of
//!    them is running.
//!
//! So the scan here is a **driver over the interactive retune path**: it issues the same
//! `DeviceAction::Retune` a user's explicit tune issues, through the same `LiveControl`, the same
//! one-at-a-time `crate::live_control::DeviceGate`, recorded against the same `device_id`. It adds
//! no device path, and the always-on invariant is untouched — capture, the ring and detection never
//! stop or slow for a scan; only the tune moves.
//!
//! What it takes from T-406 is everything that is not plumbing: [`IterativeScan`] validates the
//! dwell and builds the `DwellOnly` plan, [`CompiledPlan`] tiles it into hops clipped to the
//! device's own tunable ranges (with the DC dither of `Hop::center_on_pass`), and [`ScanBudget`]
//! prices the pass. There is no second hop-tiling and no second pricing.
//!
//! # Arbitration: the user wins, the scan yields, and the scan says so
//!
//! A sweep and interactive tuning both want the front end. The rule is:
//!
//! > **An explicit user device action always wins. The scan yields at the step it was on, keeps its
//! > place, and reports what took the radio from it. The user resumes or stops it.**
//!
//! "An explicit user device action" is not a guess: by `crate::control`'s contract a client may
//! only call the five device routes for an explicit user act ("never as the continuation of a
//! pan", T-340/T-343), so *anything arriving on a device route is the user acting*. The scan is
//! in-process and never goes through the routes, so it cannot mistake itself for the user.
//!
//! The rejected alternatives, and why:
//!
//! - **Refuse the user while a scan holds the radio.** A 6 GHz pass at a 15 s dwell is ~80 minutes;
//!   refusing every tune for that long turns the tool against its user, and it would make T-444's
//!   retune-on-pan start failing mid-interaction on a route that must keep working.
//! - **Let the scan keep stepping and take the tune back at the next step.** This is the silent
//!   drop the honesty rule forbids, one layer up from T-409's clamped nudge: the user's tune stands
//!   for a few seconds and is then undone by something they cannot see.
//!
//! Yielding is the only answer where both acts are honoured. It is surfaced three ways, so it can
//! never be silent: the device action's own response carries the `scan.yielded` object it caused,
//! `GET /api/control/scan` reports `state: "yielded"` with what yielded it, and the audit entry for
//! the user's action is already there beside it.
//!
//! The scan's own step can also lose. A **transient** refusal — `device_busy`, `conflict`,
//! `timeout` — is retried on the *same* step ([`STEP_RETRIES`]); anything else, or a step that
//! keeps failing, **yields with that error**. What it never does is move on: a skipped step would
//! leave a hole in the coverage it claims to be filling.
//!
//! # A step is a retune, with a retune's costs
//!
//! Each step is one `set_center`, and nothing else: the scan never changes the user's rate, gains
//! or filter, so a pass is tiled at the span in force and every step is a single device action.
//! Where the window's content class is unchanged that is a **tune in place** (microseconds); a
//! step that crosses a class boundary re-plumbs the segment exactly as a user's retune across the
//! same boundary does, around the **still-open device** (T-399) — capture is not stopped, and the
//! step simply starts later, because the scan times its dwell from when `set_center` returns. A
//! range inside one class — which is what a survey of a band usually is — never re-plumbs at all.
//!
//! # What the user is committing to, before they commit
//!
//! [`ScanRunner::prepare`] is reachable without starting anything (`GET /api/control/scan` with a
//! proposed range and dwell), and it answers with [`ScanBudget`] — steps, pass length, duty, and
//! T-406's own sentence, which states the limit in the same breath as the capability. A 6 GHz
//! sweep at a 15 s dwell is ~80 minutes and a 0.25 % duty; the control shows that arithmetic
//! *before* the button, not after it.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use hk_core::scheduler::{
    CompiledPlan, DEFAULT_DWELL_NS, Hop, HopKind, IterativeScan, PlanWarning, ScanBudget,
    SchedulerConfig,
};
use hk_model::{FreqRange, Timestamp};
use serde_json::{Value, json};

use crate::live_control::{DeviceAction, LiveControl, LiveControlError};

/// Nanoseconds in a second.
const NS_PER_S: f64 = 1e9;

/// Longest the worker sleeps in one wait, so a shutdown or a state change is noticed promptly even
/// on a platform whose condvar wakes late.
const MAX_WAIT: Duration = Duration::from_millis(250);

/// How many times a step is **retried** before the scan yields on it.
///
/// A step whose retune is refused for a *transient* reason — the gate held by another action
/// (`device_busy`), a re-plumb in progress (`conflict`), a device that did not answer in time
/// (`timeout`) — has not lost the radio to anything the user did, and the honest response is to
/// take the same step again rather than either giving up or moving on. **Moving on is the one
/// thing it must never do**: a skipped step would leave a hole in the coverage the sweep is
/// filling and claim the band was swept when it was not. Past this many consecutive failures the
/// refusal is not transient, and the scan yields with the error.
const STEP_RETRIES: u32 = 3;

/// How long a retried step waits before trying again: longer than the device gate's own
/// `DEVICE_GATE_WAIT`, so a retry lands after the holder has finished rather than into it.
const STEP_RETRY_BACKOFF: Duration = Duration::from_millis(500);

/// Where a scan is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// No scan.
    Idle,
    /// Stepping.
    Running,
    /// Stopped at a step, keeping its place: a user device action took the radio, or a step's own
    /// retune failed. Resumable.
    Yielded,
}

impl Phase {
    /// The name on the wire.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Yielded => "yielded",
        }
    }
}

/// Why a scan stopped stepping, and where it was when it did.
#[derive(Clone, Debug, PartialEq)]
pub struct Yielded {
    /// What took the radio: a [`DeviceAction`]'s name for a user action, or `"step_failed"` when
    /// the scan's own retune was refused.
    pub to: String,
    /// When, Unix ns.
    pub at_ns: i64,
    /// The step it was on (0-based), of [`Prepared::steps`].
    pub step: usize,
    /// The reason in words, for a person.
    pub detail: String,
}

impl Yielded {
    /// The `yielded` object on the wire.
    pub fn json(&self) -> Value {
        json!({
            "to": self.to,
            "at_s": self.at_ns as f64 / NS_PER_S,
            "step": self.step,
            "detail": self.detail,
        })
    }
}

/// What a caller asked to scan. `None` fields take the honest default: the front end's own tunable
/// range, and T-406's 15 s dwell.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScanRequest {
    /// The range to sweep; `None` is everything this front end can tune.
    pub freq: Option<FreqRange>,
    /// Seconds per step; `None` is [`DEFAULT_DWELL_NS`].
    pub dwell_s: Option<f64>,
}

/// A scan refused, with the status and stable code the control API answers.
#[derive(Clone, Debug, PartialEq)]
pub struct ScanError {
    /// HTTP status.
    pub status: u16,
    /// Stable machine code.
    pub code: &'static str,
    /// Message.
    pub message: String,
}

impl ScanError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            status: 400,
            code: "invalid",
            message: message.into(),
        }
    }
    fn refused(message: impl Into<String>) -> Self {
        Self {
            status: 409,
            code: "refused",
            message: message.into(),
        }
    }
}

/// A scan compiled against the front end: the hops it will visit and what the pass costs.
#[derive(Clone, Debug)]
pub struct Prepared {
    /// The range as asked for (before clipping; the warnings say what was clipped).
    pub requested: FreqRange,
    /// Seconds per step.
    pub dwell_ns: i64,
    /// Whether the dwell is inside the 10–30 s the survey is sized for
    /// ([`IterativeScan::recommended`]). Outside it still runs — the user asked for a configurable
    /// number, not a clamped one — and this is what lets the control say it is unusual.
    pub recommended_dwell: bool,
    /// The sample rate the hops were tiled at: the one **in force**, so a scan never implies a
    /// window the run is not capturing.
    pub rate_hz: f64,
    /// The steps, in visit order.
    hops: Vec<Hop>,
    /// The price of one pass.
    pub budget: ScanBudget,
    /// What compilation had to say about the request (clipping above all).
    pub warnings: Vec<String>,
}

impl Prepared {
    /// Steps in one pass.
    pub fn steps(&self) -> usize {
        self.hops.len()
    }

    /// The plan and its price, as the control API serves them.
    pub fn json(&self) -> Value {
        let b = &self.budget;
        json!({
            "plan": {
                "f_lo_hz": self.requested.lo_hz,
                "f_hi_hz": self.requested.hi_hz,
                "dwell_s": self.dwell_ns as f64 / NS_PER_S,
                "recommended_dwell": self.recommended_dwell,
                "sample_rate_hz": self.rate_hz,
                "steps": self.hops.len(),
                "warnings": self.warnings,
            },
            "budget": {
                "steps": b.steps,
                "dwell_s": b.dwell_ns as f64 / NS_PER_S,
                "pass_s": b.revisit_s(),
                "revisit_s": b.revisit_s(),
                "span_hz": b.span_hz,
                "step_span_hz": b.step_span_hz,
                "duty": b.duty(),
                // T-406's own sentence: what the pass catches and what it does not, in one line.
                "statement": b.statement(),
            },
        })
    }
}

/// A plan warning in words. The two a caller's own range can cause are spelled out; the rest are
/// scheduler-internal and printed as themselves rather than paraphrased.
fn warning_text(w: &PlanWarning) -> String {
    match w {
        PlanWarning::ClippedToCapabilities { .. } => {
            "part of that range is outside the front end's tunable ranges and was clipped; the \
             scan covers only what the device can reach"
                .into()
        }
        PlanWarning::OutsideCapabilities { .. } => {
            "part of that range is wholly outside the front end's tunable ranges and is not \
             scanned"
                .into()
        }
        other => format!("{other:?}"),
    }
}

/// The scan in progress.
#[derive(Debug)]
struct Active {
    /// Index of the step to tune next.
    step: usize,
    /// Pass number, from 0 (the DC dither of `Hop::center_on_pass` alternates on it).
    pass: u64,
    /// Steps taken since `start`.
    steps_done: u64,
    /// When the scan started, Unix ns.
    started_ns: i64,
    /// When the current step's tune was applied, Unix ns (`None` before the first).
    step_started_ns: Option<i64>,
    /// The centre in force from this scan (`None` before the first step).
    center_hz: Option<f64>,
    /// When to tune `step`.
    due: Instant,
    /// Consecutive transient failures on `step` ([`STEP_RETRIES`]).
    retries: u32,
}

struct State {
    phase: Phase,
    prepared: Option<Prepared>,
    active: Option<Active>,
    yielded: Option<Yielded>,
    /// Bumped whenever the phase changes, so a retune that was in flight across the change is
    /// discarded rather than written over the new state.
    generation: u64,
    shutdown: bool,
}

struct Shared {
    live: Arc<dyn LiveControl>,
    state: Mutex<State>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Starts, steps and stops the in-app survey sweep over one front end.
///
/// One per server. The worker thread starts with the first scan and lives until the runner drops.
pub struct ScanRunner {
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for ScanRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScanRunner")
            .field("phase", &self.shared.lock().phase)
            .finish()
    }
}

impl ScanRunner {
    /// A runner over `live`, idle.
    pub fn new(live: Arc<dyn LiveControl>) -> Self {
        Self {
            shared: Arc::new(Shared {
                live,
                state: Mutex::new(State {
                    phase: Phase::Idle,
                    prepared: None,
                    active: None,
                    yielded: None,
                    generation: 0,
                    shutdown: false,
                }),
                wake: Condvar::new(),
            }),
            worker: Mutex::new(None),
        }
    }

    /// Whether this front end can be swept at all, and why not when it cannot.
    ///
    /// A source that cannot be tuned, or that states no frequency range, is **not** scannable, and
    /// the control says so rather than offering a button that would fail: "nothing said" about a
    /// range is not a range (the T-325 rule).
    pub fn availability(&self) -> Result<(), String> {
        let caps = self.shared.live.capabilities();
        if !caps.controllable {
            return Err("this front end cannot be retuned, so it cannot be swept".into());
        }
        if caps.frequency_ranges.is_empty() {
            return Err(
                "this front end states no tunable range, so there is nothing to sweep".into(),
            );
        }
        Ok(())
    }

    /// Compiles `req` against the front end **without starting anything**: the steps it would
    /// visit and what the pass costs.
    pub fn prepare(&self, req: &ScanRequest) -> Result<Prepared, ScanError> {
        self.availability().map_err(ScanError::refused)?;
        let caps = self.shared.live.capabilities();
        let tuning = self.shared.live.tuning();
        let dwell_s = req.dwell_s.unwrap_or(DEFAULT_DWELL_NS as f64 / NS_PER_S);
        let scan = IterativeScan::from_seconds(dwell_s)
            .map_err(|e| ScanError::invalid(format!("dwell: {e}")))?;
        let ranges: Vec<FreqRange> = match req.freq {
            Some(f) => {
                if !(f.lo_hz.is_finite() && f.hi_hz.is_finite() && f.hi_hz > f.lo_hz) {
                    return Err(ScanError::invalid(
                        "f_hi_hz must be a finite frequency above a finite f_lo_hz",
                    ));
                }
                vec![f]
            }
            None => caps
                .frequency_ranges
                .iter()
                .map(|r| FreqRange::new(r.min_hz, r.max_hz))
                .collect(),
        };
        let requested = FreqRange::new(
            ranges.iter().map(|r| r.lo_hz).fold(f64::INFINITY, f64::min),
            ranges
                .iter()
                .map(|r| r.hi_hz)
                .fold(f64::NEG_INFINITY, f64::max),
        );
        let plan = scan.plan("in-app scan", &ranges, Timestamp::now());
        let mut cfg = SchedulerConfig::from_plan(&plan)
            .map_err(|e| ScanError::invalid(format!("scan plan: {e}")))?;
        // Tile the pass at the span **actually in use**. A scan must not imply a window the run is
        // not capturing, and it must not change the user's span behind their back either: the one
        // device action a step takes is the retune, and nothing else.
        cfg.sweep_rate_hz = tuning.sample_rate_hz;
        cfg.max_span_hz = cfg.max_span_hz.max(tuning.sample_rate_hz);
        cfg.validate(caps)
            .map_err(|e| ScanError::invalid(format!("scan plan: {e}")))?;
        let compiled = CompiledPlan::compile(&plan, &cfg, caps)
            .map_err(|e| ScanError::invalid(format!("scan plan: {e}")))?;
        let hops: Vec<Hop> = compiled
            .hops
            .iter()
            .copied()
            .filter(|h| h.kind == HopKind::RegionDwell)
            .collect();
        if hops.is_empty() {
            return Err(ScanError::invalid(
                "no part of that range is inside the front end's tunable ranges, so there is \
                 nothing to sweep",
            ));
        }
        Ok(Prepared {
            requested,
            dwell_ns: scan.dwell_ns(),
            recommended_dwell: scan.recommended(),
            rate_hz: tuning.sample_rate_hz,
            budget: scan.budget(&compiled),
            warnings: compiled.warnings.iter().map(warning_text).collect(),
            hops,
        })
    }

    /// Starts a scan. Refuses (409) while one is running: a second sweep over one front end is two
    /// policies fighting for the same tune, which is the thing this module exists to prevent.
    ///
    /// Starting from `Yielded` replaces the yielded scan, which is what a user asking for a new
    /// range means.
    pub fn start(&self, req: &ScanRequest) -> Result<Prepared, ScanError> {
        let prepared = self.prepare(req)?;
        {
            let mut st = self.shared.lock();
            if st.phase == Phase::Running {
                return Err(ScanError::refused(
                    "a scan is already running on this front end; stop it before starting another",
                ));
            }
            st.phase = Phase::Running;
            st.yielded = None;
            st.prepared = Some(prepared.clone());
            st.active = Some(Active {
                step: 0,
                pass: 0,
                steps_done: 0,
                started_ns: now_ns(),
                step_started_ns: None,
                center_hz: None,
                due: Instant::now(),
                retries: 0,
            });
            st.generation += 1;
        }
        self.ensure_worker();
        self.shared.wake.notify_all();
        Ok(prepared)
    }

    /// Resumes a yielded scan at the step it stopped on. Refuses when there is nothing to resume.
    pub fn resume(&self) -> Result<(), ScanError> {
        {
            let mut st = self.shared.lock();
            match st.phase {
                Phase::Running => {
                    return Err(ScanError::refused("the scan is already running"));
                }
                Phase::Idle => {
                    return Err(ScanError::refused(
                        "there is no yielded scan to resume; start one with a range and a dwell",
                    ));
                }
                Phase::Yielded => {}
            }
            self.availability().map_err(ScanError::refused)?;
            st.phase = Phase::Running;
            st.yielded = None;
            if let Some(a) = st.active.as_mut() {
                a.due = Instant::now();
            }
            st.generation += 1;
        }
        self.ensure_worker();
        self.shared.wake.notify_all();
        Ok(())
    }

    /// Stops the scan and forgets its place. Never refused: a control that can command the radio
    /// must always be surrenderable.
    pub fn stop(&self) {
        let mut st = self.shared.lock();
        st.phase = Phase::Idle;
        st.active = None;
        st.prepared = None;
        st.yielded = None;
        st.generation += 1;
        drop(st);
        self.shared.wake.notify_all();
    }

    /// **The arbitration.** An explicit user device action arrived on a control route: a running
    /// scan yields to it at the step it was on and keeps its place.
    ///
    /// Returns what it recorded when it yielded, so the user's own response can say it — the
    /// honesty rule: a deferred action is never silently dropped, and neither is the scan it
    /// deferred.
    ///
    /// Called **before** the device call is made, so the scan has already stopped stepping by the
    /// time the user's action reaches the gate; the only remaining contention is one step's retune
    /// already in flight, which the gate answers as it always has (409 `device_busy`).
    pub fn note_user_device_action(&self, action: DeviceAction) -> Option<Yielded> {
        let mut st = self.shared.lock();
        if st.phase != Phase::Running {
            return None;
        }
        let step = st.active.as_ref().map_or(0, |a| a.step);
        let y = Yielded {
            to: action.as_str().to_owned(),
            at_ns: now_ns(),
            step,
            detail: format!(
                "an explicit {} took the front end; the sweep stopped at step {} of {} and kept \
                 its place",
                action,
                step + 1,
                st.prepared.as_ref().map_or(0, Prepared::steps),
            ),
        };
        st.phase = Phase::Yielded;
        st.yielded = Some(y.clone());
        st.generation += 1;
        drop(st);
        self.shared.wake.notify_all();
        Some(y)
    }

    /// Undoes a yield this caller caused, when the user action it yielded to **did not happen**
    /// (it was refused before it reached the device).
    ///
    /// Nothing took the radio, so nothing should have stopped the sweep. It restores `Running`
    /// only when the yield still standing is exactly `y` — so a yield someone else caused in the
    /// meantime, or a stop, is never quietly undone.
    pub fn unyield(&self, y: &Yielded) {
        let mut st = self.shared.lock();
        if st.phase != Phase::Yielded || st.yielded.as_ref() != Some(y) {
            return;
        }
        st.phase = Phase::Running;
        st.yielded = None;
        if let Some(a) = st.active.as_mut() {
            a.due = Instant::now();
        }
        st.generation += 1;
        drop(st);
        self.shared.wake.notify_all();
    }

    /// The scan's state, as `GET /api/control/scan` serves it.
    pub fn json(&self) -> Value {
        let unavailable = self.availability().err();
        let st = self.shared.lock();
        let mut out = json!({
            "state": st.phase.as_str(),
            "available": unavailable.is_none(),
            "unavailable_reason": unavailable,
            "yielded": st.yielded.as_ref().map(Yielded::json),
        });
        let o = out.as_object_mut().expect("object");
        match (&st.prepared, &st.active) {
            (Some(p), Some(a)) => {
                let j = p.json();
                o.insert("plan".into(), j["plan"].clone());
                o.insert("budget".into(), j["budget"].clone());
                let now = Instant::now();
                o.insert(
                    "progress".into(),
                    json!({
                        "step": a.step,
                        "steps": p.steps(),
                        "pass": a.pass,
                        "steps_done": a.steps_done,
                        "center_hz": a.center_hz,
                        "started_s": a.started_ns as f64 / NS_PER_S,
                        "step_started_s": a.step_started_ns.map(|n| n as f64 / NS_PER_S),
                        "next_step_in_s": a.due.saturating_duration_since(now).as_secs_f64(),
                    }),
                );
            }
            _ => {
                o.insert("plan".into(), Value::Null);
                o.insert("budget".into(), Value::Null);
                o.insert("progress".into(), Value::Null);
            }
        }
        out
    }

    fn ensure_worker(&self) {
        let mut w = self.worker.lock().unwrap_or_else(PoisonError::into_inner);
        if w.is_some() {
            return;
        }
        let shared = Arc::clone(&self.shared);
        *w = std::thread::Builder::new()
            .name("hk-scan".into())
            .spawn(move || worker(&shared))
            .ok();
    }
}

impl Drop for ScanRunner {
    fn drop(&mut self) {
        {
            let mut st = self.shared.lock();
            st.shutdown = true;
            st.phase = Phase::Idle;
            st.generation += 1;
        }
        self.shared.wake.notify_all();
        if let Some(h) = self
            .worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = h.join();
        }
    }
}

fn now_ns() -> i64 {
    Timestamp::now().as_unix_nanos()
}

/// One step's decision, taken under the lock and executed without it.
struct Step {
    generation: u64,
    index: usize,
    center_hz: f64,
    dwell_ns: i64,
}

/// The worker: wait for the step to fall due, retune, then hold for the dwell.
///
/// The retune is made **without the state lock**, so a `stop` or a user action is never blocked
/// behind a re-plumb; the generation taken with the decision is re-checked afterwards, so a result
/// that arrived across a phase change is discarded instead of overwriting the new state.
fn worker(shared: &Arc<Shared>) {
    loop {
        let step = {
            let mut st = shared.lock();
            loop {
                if st.shutdown {
                    return;
                }
                if st.phase != Phase::Running {
                    st = shared
                        .wake
                        .wait_timeout(st, MAX_WAIT)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                    continue;
                }
                let (Some(p), Some(a)) = (st.prepared.as_ref(), st.active.as_ref()) else {
                    // Running with nothing to run is not a state this module can produce; treat it
                    // as stopped rather than spinning.
                    st.phase = Phase::Idle;
                    continue;
                };
                let now = Instant::now();
                if a.due > now {
                    let wait = a.due.saturating_duration_since(now).min(MAX_WAIT);
                    st = shared
                        .wake
                        .wait_timeout(st, wait)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                    continue;
                }
                let hop = p.hops[a.step.min(p.hops.len() - 1)];
                break Step {
                    generation: st.generation,
                    index: a.step,
                    center_hz: hop.center_on_pass(a.pass),
                    dwell_ns: hop.duration_ns.max(1),
                };
            }
        };
        let result = shared.live.set_center(step.center_hz);
        let mut st = shared.lock();
        if st.generation != step.generation || st.phase != Phase::Running {
            // A stop, a resume or a user action landed while this retune was in flight: its
            // outcome belongs to a scan that no longer exists.
            continue;
        }
        match result {
            Ok(t) => {
                let steps = st.prepared.as_ref().map_or(1, Prepared::steps).max(1);
                if let Some(a) = st.active.as_mut() {
                    a.center_hz = Some(t.center_hz);
                    a.step_started_ns = Some(now_ns());
                    a.steps_done += 1;
                    a.retries = 0;
                    a.step += 1;
                    if a.step >= steps {
                        a.step = 0;
                        a.pass += 1;
                    }
                    a.due = Instant::now() + Duration::from_nanos(step.dwell_ns as u64);
                }
            }
            Err(e) => {
                // The scan's own step lost the radio. Whatever happens next, it does NOT move on:
                // skipping the step would leave a hole in the coverage this feature exists to fill
                // and claim the band was swept when it was not.
                let transient = matches!(
                    e,
                    LiveControlError::DeviceBusy { .. }
                        | LiveControlError::Conflict(_)
                        | LiveControlError::Timeout(_)
                );
                let retries = st.active.as_ref().map_or(0, |a| a.retries);
                if transient && retries < STEP_RETRIES {
                    // Nothing the user did took the radio (a user action yields the scan before it
                    // is attempted, and this result would then have been discarded above). Take the
                    // same step again.
                    if let Some(a) = st.active.as_mut() {
                        a.retries += 1;
                        a.due = Instant::now() + STEP_RETRY_BACKOFF;
                    }
                    continue;
                }
                let y = Yielded {
                    to: "step_failed".into(),
                    at_ns: now_ns(),
                    step: step.index,
                    detail: format!(
                        "step {} could not retune to {:.6} MHz{}: {e}",
                        step.index + 1,
                        step.center_hz / 1e6,
                        if retries > 0 {
                            format!(" after {retries} retries")
                        } else {
                            String::new()
                        },
                    ),
                };
                st.phase = Phase::Yielded;
                st.yielded = Some(y);
                st.generation += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use hk_core::NamedGain;
    use hk_core::source::{SampleRates, SourceCapabilities};
    use hk_model::BiasTee;

    use super::*;
    use crate::live_control::LiveTuning;

    /// A live control that records retunes and can be made to refuse them.
    #[derive(Debug)]
    struct Fake {
        caps: SourceCapabilities,
        tuning: Mutex<LiveTuning>,
        centers: Mutex<Vec<f64>>,
        refuse: Mutex<bool>,
        calls: AtomicU64,
    }

    impl Fake {
        fn new() -> Arc<Self> {
            let mut caps = SourceCapabilities::hackrf_one();
            caps.frequency_ranges = vec![hk_core::source::FrequencyRange {
                min_hz: 100e6,
                max_hz: 160e6,
            }];
            caps.sample_rates = SampleRates::Discrete(vec![20e6]);
            Arc::new(Self {
                caps,
                tuning: Mutex::new(LiveTuning {
                    center_hz: 100e6,
                    sample_rate_hz: 20e6,
                    gains: Vec::new(),
                    bias_tee: BiasTee::Unknown,
                    baseband_filter_hz: None,
                }),
                centers: Mutex::new(Vec::new()),
                refuse: Mutex::new(false),
                calls: AtomicU64::new(0),
            })
        }
        fn centers(&self) -> Vec<f64> {
            self.centers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    impl LiveControl for Fake {
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
            Some("fake:1")
        }
        fn set_center(&self, center_hz: f64) -> Result<LiveTuning, LiveControlError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if *self.refuse.lock().unwrap_or_else(PoisonError::into_inner) {
                return Err(LiveControlError::DeviceBusy {
                    device_id: Some("fake:1".into()),
                    holder: DeviceAction::Retune,
                    held_for_s: 0.1,
                    requested: DeviceAction::Retune,
                });
            }
            self.centers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(center_hz);
            let mut t = self.tuning.lock().unwrap_or_else(PoisonError::into_inner);
            t.center_hz = center_hz;
            Ok(t.clone())
        }
        fn set_rate(&self, _hz: f64) -> Result<LiveTuning, LiveControlError> {
            Ok(self.tuning())
        }
        fn set_gains(&self, _g: &[NamedGain]) -> Result<LiveTuning, LiveControlError> {
            Ok(self.tuning())
        }
        fn set_bias_tee(&self, _on: bool) -> Result<LiveTuning, LiveControlError> {
            Ok(self.tuning())
        }
        fn set_baseband_filter(&self, _hz: f64) -> Result<LiveTuning, LiveControlError> {
            Ok(self.tuning())
        }
    }

    fn wait_for(f: impl Fn() -> bool) -> bool {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(5) {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// The budget is the arithmetic the control shows before the button, and it comes from T-406's
    /// own pricing — not a second one.
    #[test]
    fn prepare_prices_the_pass_without_starting_it() {
        let live = Fake::new();
        let r = ScanRunner::new(live.clone() as Arc<dyn LiveControl>);
        let p = r
            .prepare(&ScanRequest {
                freq: None,
                dwell_s: Some(12.0),
            })
            .expect("prepared");
        // 60 MHz of tunable range at a 15 MHz usable span (0.75 x 20 Msps) is four steps.
        assert_eq!(p.steps(), 4);
        assert_eq!(p.budget.steps, 4);
        assert!((p.budget.revisit_s() - 48.0).abs() < 1e-6, "{:?}", p.budget);
        assert!((p.budget.duty() - 0.25).abs() < 1e-9);
        assert!(p.recommended_dwell);
        assert!(p.budget.statement().contains("4 steps"));
        // Preparing is not starting: the radio has not moved.
        assert!(live.centers().is_empty());
        assert_eq!(r.json()["state"], "idle");
    }

    /// A dwell outside 10-30 s runs and is reported as unusual, never clamped (T-406).
    #[test]
    fn an_unusual_dwell_is_reported_not_clamped() {
        let live = Fake::new();
        let r = ScanRunner::new(live as Arc<dyn LiveControl>);
        let p = r
            .prepare(&ScanRequest {
                freq: None,
                dwell_s: Some(2.0),
            })
            .expect("prepared");
        assert!(!p.recommended_dwell);
        assert!((p.budget.dwell_ns as f64 / NS_PER_S - 2.0).abs() < 1e-9);
    }

    /// A range the device cannot reach is refused with the reason, not silently narrowed to
    /// nothing.
    #[test]
    fn a_range_outside_the_device_is_refused_with_its_reason() {
        let live = Fake::new();
        let r = ScanRunner::new(live as Arc<dyn LiveControl>);
        let e = r
            .prepare(&ScanRequest {
                freq: Some(FreqRange::new(2.4e9, 2.5e9)),
                dwell_s: None,
            })
            .expect_err("refused");
        assert_eq!(e.status, 400);
        assert!(
            e.message.contains("frequency ranges"),
            "the refusal must name what was wrong with the range, got {:?}",
            e.message
        );
    }

    /// Steps walk the compiled hops through `set_center` — the one gated device path — and the
    /// coverage plane gets one steady tune per step.
    #[test]
    fn the_scan_steps_the_tune_through_the_device_path() {
        let live = Fake::new();
        let r = ScanRunner::new(live.clone() as Arc<dyn LiveControl>);
        r.start(&ScanRequest {
            freq: None,
            dwell_s: Some(0.02),
        })
        .expect("started");
        assert!(wait_for(|| live.centers().len() >= 4));
        let c = live.centers();
        assert!((c[0] - 107.5e6).abs() < 1e-3, "{c:?}");
        assert!((c[1] - 122.5e6).abs() < 1e-3, "{c:?}");
        assert_eq!(r.json()["state"], "running");
        r.stop();
        assert_eq!(r.json()["state"], "idle");
        assert_eq!(r.json()["plan"], Value::Null);
    }

    /// THE ARBITRATION. An explicit user device action wins; the scan yields at its step, keeps
    /// its place, and says what took the radio.
    #[test]
    fn a_user_device_action_wins_and_the_scan_says_so() {
        let live = Fake::new();
        let r = ScanRunner::new(live.clone() as Arc<dyn LiveControl>);
        r.start(&ScanRequest {
            freq: None,
            dwell_s: Some(10.0),
        })
        .expect("started");
        assert!(wait_for(|| !live.centers().is_empty()));
        let y = r
            .note_user_device_action(DeviceAction::Retune)
            .expect("the running scan yielded");
        assert_eq!(y.to, "retune");
        assert!(y.detail.contains("kept"), "{}", y.detail);
        let v = r.json();
        assert_eq!(v["state"], "yielded");
        assert_eq!(v["yielded"]["to"], "retune");
        // It kept its place: the plan and the step survive the yield.
        assert_eq!(v["plan"]["steps"], 4);
        // And it stops stepping: no further retune arrives.
        let n = live.centers().len();
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(live.centers().len(), n);
        // Resuming picks the same scan back up.
        r.resume().expect("resumed");
        assert_eq!(r.json()["state"], "running");
        assert_eq!(r.json()["yielded"], Value::Null);
        r.stop();
    }

    /// An idle scan has nothing to yield, so a user action is not reported as preempting one.
    #[test]
    fn an_idle_scan_yields_nothing() {
        let live = Fake::new();
        let r = ScanRunner::new(live as Arc<dyn LiveControl>);
        assert!(r.note_user_device_action(DeviceAction::Retune).is_none());
        assert!(r.resume().is_err());
    }

    /// A **transient** refusal — the gate briefly held elsewhere — is retried on the SAME step,
    /// never skipped and not fatal. Nothing the user did took the radio (a user action yields the
    /// scan before it is attempted), so giving up would be as wrong as moving on.
    #[test]
    fn a_transient_refusal_retries_the_same_step_and_the_sweep_continues() {
        let live = Fake::new();
        *live.refuse.lock().unwrap() = true;
        let r = ScanRunner::new(live.clone() as Arc<dyn LiveControl>);
        r.start(&ScanRequest {
            freq: None,
            dwell_s: Some(0.02),
        })
        .expect("started");
        // It tried, and it is still running on step 0 — not advanced past it, not yielded.
        assert!(wait_for(|| live.calls.load(Ordering::SeqCst) >= 2));
        let v = r.json();
        assert_eq!(
            v["state"], "running",
            "a transient refusal is not fatal: {v}"
        );
        assert_eq!(v["progress"]["step"], 0, "and the step is not skipped: {v}");
        assert_eq!(v["progress"]["steps_done"], 0);
        // When the device frees up, the sweep carries on from that same step.
        *live.refuse.lock().unwrap() = false;
        assert!(wait_for(|| !live.centers().is_empty()));
        assert!(
            (live.centers()[0] - 107.5e6).abs() < 1e-3,
            "{:?}",
            live.centers()
        );
        r.stop();
    }

    /// The scan's own step can lose the radio for good. After [`STEP_RETRIES`] it yields with the
    /// error rather than skipping the step, because a skipped step claims a band was swept when it
    /// was not.
    #[test]
    fn a_refused_step_yields_with_its_error_and_never_skips() {
        let live = Fake::new();
        *live.refuse.lock().unwrap() = true;
        let r = ScanRunner::new(live.clone() as Arc<dyn LiveControl>);
        r.start(&ScanRequest {
            freq: None,
            dwell_s: Some(10.0),
        })
        .expect("started");
        assert!(wait_for(|| r.json()["state"] == "yielded"));
        let v = r.json();
        assert_eq!(v["yielded"]["to"], "step_failed");
        assert_eq!(v["yielded"]["step"], 0);
        assert!(
            v["yielded"]["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("device_busy")
                || v["yielded"]["detail"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("busy"),
            "{v}"
        );
        // The place is kept at the failed step, not advanced past it.
        assert_eq!(v["progress"]["step"], 0);
        assert!(live.centers().is_empty());
    }

    /// A second scan over one front end is two policies fighting for one tune.
    #[test]
    fn a_second_scan_is_refused_while_one_runs() {
        let live = Fake::new();
        let r = ScanRunner::new(live as Arc<dyn LiveControl>);
        let req = ScanRequest {
            freq: None,
            dwell_s: Some(10.0),
        };
        r.start(&req).expect("started");
        let e = r.start(&req).expect_err("refused");
        assert_eq!(e.status, 409);
        r.stop();
    }
}
