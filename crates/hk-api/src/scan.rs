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
//! **The one exception is a coarse step (T-517, [`ScanStep`]).** A fine step tiles at the rate in
//! force, so from a 2 Msps window it advances 1.5 MHz (~4000 steps for 1 MHz–6 GHz). A coarse
//! step tiles at the widest power-of-two multiple of that rate at which the run's bins keep their
//! width ([`coarse_step_rate`], ~10× fewer steps), and so commits the front end to **one** rate
//! change before its first retune, through the same `LiveControl`. The plan says so before
//! anything moves (`changes_rate`), and the start answer names it.
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
    CompiledPlan, DEFAULT_DWELL_NS, Hop, HopKind, IterativeScan, PlanWarning, ScanBudget, ScanStep,
    SchedulerConfig, coarse_step_rate,
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

/// **How long a step keeps retrying a `conflict` — "a re-plumb is in progress" — before the scan
/// yields on it** (T-542).
///
/// [`STEP_RETRIES`] is a *count*, and a count is the wrong shape for this one refusal. The other
/// two transients are another act holding the device gate, which is over in the gate's own wait;
/// a `conflict` is **this run re-plumbing itself**, and the bound on that is
/// `hk_pipeline::run::REPLUMB_TIMEOUT` — 30 s, the same number the control API states to the UI.
/// Three retries at [`STEP_RETRY_BACKOFF`] is 1.5 s, so the budget for retrying was twenty times
/// shorter than the thing it was retrying.
///
/// Measured on the live HackRF, `POST /api/control/scan` over 1 MHz–6000 MHz at dwell 1 s: a
/// class-changing step re-plumbs, the re-plumb takes 8–19 s, and the sweep **yielded at step 19 of
/// 3334** with *"could not retune to 34.300000 MHz after 3 retries: a re-plumb is in progress"* —
/// 0.5 % of the survey the user asked for, and from their seat the sweep simply stopped. On a mock
/// the re-plumb is far quicker than the retry budget, which is why no mock run ever showed it.
///
/// It is deliberately longer than `REPLUMB_TIMEOUT`: past *that*, the re-plumb has failed by the
/// pipeline's own reckoning, and a refusal that outlives it is not transient any more.
const REPLUMB_RETRY_BUDGET: Duration = Duration::from_secs(35);

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
    /// How far a step advances (T-517); `None` is [`ScanStep::Fine`], today's behaviour.
    pub step: Option<ScanStep>,
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
    /// The sample rate the hops were tiled at: the one **in force** for a fine step, so a scan
    /// never implies a window the run is not capturing; for a coarse step the wider rate the scan
    /// sets before its first step (T-517).
    pub rate_hz: f64,
    /// The sample rate in force when the scan was prepared.
    pub rate_in_force_hz: f64,
    /// Fine or coarse (T-517).
    pub step: ScanStep,
    /// The detection/history bin width at [`Prepared::rate_hz`], Hz; `None` when the server
    /// cannot state it. Identical fine vs coarse by construction ([`coarse_step_rate`]).
    pub bin_hz: Option<f64>,
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

    /// Whether the scan sets a different sample rate before stepping (a coarse step from a
    /// narrower window).
    pub fn changes_rate(&self) -> bool {
        self.rate_hz != self.rate_in_force_hz
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
                "rate_in_force_hz": self.rate_in_force_hz,
                "changes_rate": self.changes_rate(),
                "step": self.step.as_str(),
                "bin_hz": self.bin_hz,
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
                // T-965: what the pass spends listening, what each step spends retuning before it
                // can, and whether anything measured the latter. `step_overhead_s` is `null` when
                // nothing has — which is not zero: `pass_s` is then a floor, and `statement` says so.
                "dwell_total_s": b.dwell_total_ns as f64 / NS_PER_S,
                "step_overhead_s": b.step_overhead_ns.map(|o| o as f64 / NS_PER_S),
                "overhead_measured": b.overhead_measured(),
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
    /// When the current run of consecutive transient failures on `step` began, for
    /// [`REPLUMB_RETRY_BUDGET`]. `None` whenever the step has not been refused (T-542).
    retry_since: Option<Instant>,
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
    /// T-965: wall time this front end's scan retunes have taken in total, ns, and how many were
    /// measured. Their mean is **the per-step cost a pass pays beyond its dwell** — the retune and,
    /// at a class or rate boundary, the run's re-plumb — which [`ScanBudget`] priced at zero and the
    /// user paid anyway (`Scan everything (fast)`: 125.4 s stated, 237 s taken).
    ///
    /// Measured here because here is the only place that sees the whole cost: the worker brackets
    /// the one `set_center`/`set_rate` pair a step makes, so what is timed is exactly what the step
    /// waits for, not an estimate divided out of a pass length. Only a **successful** retune is
    /// counted — a refused step is retried and has not happened yet, so its wait is not the price of
    /// a step that did.
    ///
    /// It outlives one scan, because it is a property of the front end and not of a pass, and it is
    /// a sum and a count rather than a smoothed figure so the served number is a plain mean with
    /// nothing tuned in it.
    retune_ns_total: i64,
    retunes_measured: u64,
}

impl State {
    /// T-965: the measured per-step cost beyond the dwell, ns — `None` until a retune has been
    /// timed. **`None` is not zero**: a pass priced without it states a floor (see
    /// [`ScanBudget::statement`]).
    fn measured_step_overhead_ns(&self) -> Option<i64> {
        (self.retunes_measured > 0).then(|| {
            self.retune_ns_total / i64::try_from(self.retunes_measured).unwrap_or(i64::MAX)
        })
    }
}

/// The detection/history bin width, Hz, at a sample rate (T-517).
pub type BinWidth = Arc<dyn Fn(f64) -> f64 + Send + Sync>;

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
    /// The run's frequency resolution as a function of rate; without it a coarse step cannot
    /// promise identical bins and is refused.
    bin_hz: Option<BinWidth>,
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
                    retune_ns_total: 0,
                    retunes_measured: 0,
                }),
                wake: Condvar::new(),
            }),
            worker: Mutex::new(None),
            bin_hz: None,
        }
    }

    /// States the run's detection/history bin width as a function of the sample rate (T-517), so
    /// a coarse step can widen the window exactly where the bins stay the same width.
    #[must_use]
    pub fn with_bin_width(mut self, bin_hz: BinWidth) -> Self {
        self.bin_hz = Some(bin_hz);
        self
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
        // Tile the pass at the span **actually in use** for a fine step. A scan must not imply a
        // window the run is not capturing, and a fine step does not change the user's span: the
        // one device action it takes is the retune. A coarse step (T-517) asks for a wider window
        // up front — the widest power-of-two multiple of the rate in force at which the bins keep
        // their width — and says so in the plan (`changes_rate`) before anything moves.
        let step = req.step.unwrap_or_default();
        let rate_in_force = tuning.sample_rate_hz;
        let rate_hz = match step {
            ScanStep::Fine => rate_in_force,
            ScanStep::Coarse => {
                let bin = self.bin_hz.as_ref().ok_or_else(|| {
                    ScanError::refused(
                        "this server cannot state its frequency resolution, so a coarse step \
                         cannot promise identical bins; use a fine step",
                    )
                })?;
                coarse_step_rate(rate_in_force, caps, |fs| bin(fs))
            }
        };
        let bin_hz = self.bin_hz.as_ref().map(|b| b(rate_hz));
        cfg.sweep_rate_hz = rate_hz;
        cfg.max_span_hz = cfg.max_span_hz.max(rate_hz);
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
            rate_hz,
            rate_in_force_hz: rate_in_force,
            step,
            bin_hz,
            // T-965: priced with what this front end's retunes have actually been measured at, so
            // the commitment line states the pass the user will wait for. Unmeasured, the budget
            // says its own figure is a floor rather than implying the dwells are the whole cost.
            budget: priced(
                scan.budget(&compiled),
                self.shared.lock().measured_step_overhead_ns(),
            ),
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
                retry_since: None,
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
                // T-965: repriced from what THIS scan's own retunes have measured, so the pass
                // length converges on the truth while the user watches rather than staying at the
                // figure the preview guessed before anything had been timed.
                let overhead = st.measured_step_overhead_ns();
                let mut p = p.clone();
                p.budget = priced(p.budget, overhead);
                let j = p.json();
                o.insert("plan".into(), j["plan"].clone());
                o.insert("budget".into(), j["budget"].clone());
                let now = Instant::now();
                // The pass length the steps taken so far imply, end to end on the wall clock: the
                // number to compare the stated budget against (T-965's "stated must match
                // measured"). `null` until a step has been timed.
                let measured_pass_s = overhead.map(|o| {
                    (p.steps() as i64).saturating_mul(o.saturating_add(p.dwell_ns)) as f64
                        / NS_PER_S
                });
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
                        "elapsed_s": (now_ns() - a.started_ns).max(0) as f64 / NS_PER_S,
                        "measured_step_overhead_s": overhead.map(|o| o as f64 / NS_PER_S),
                        "measured_pass_s": measured_pass_s,
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
    /// The rate the plan was tiled at: a step first restores it when the window in force differs
    /// (a coarse scan's first step, or a resume after the user changed the rate while yielded),
    /// because a hop tiled at one rate taken at another would leave holes or overlap.
    rate_hz: f64,
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
                    rate_hz: p.rate_hz,
                };
            }
        };
        // T-965: the step's whole cost beyond its dwell, timed where it is paid. A step is a
        // retune, and on a class or rate boundary the run re-plumbs behind it; none of that
        // advances capture time, so a pass priced from its dwells prices it at zero.
        let retune_began = Instant::now();
        let result = if shared.live.tuning().sample_rate_hz == step.rate_hz {
            shared.live.set_center(step.center_hz)
        } else {
            shared
                .live
                .set_rate(step.rate_hz)
                .and_then(|_| shared.live.set_center(step.center_hz))
        };
        let retune_took = retune_began.elapsed();
        let mut st = shared.lock();
        if st.generation != step.generation || st.phase != Phase::Running {
            // A stop, a resume or a user action landed while this retune was in flight: its
            // outcome belongs to a scan that no longer exists.
            continue;
        }
        // Set inside the `Ok` arm and folded into the front end's measured cost after it, so the
        // borrow of `st.active` does not have to span the update.
        let mut took: Option<Duration> = None;
        match result {
            Ok(t) => {
                let steps = st.prepared.as_ref().map_or(1, Prepared::steps).max(1);
                if let Some(a) = st.active.as_mut() {
                    a.center_hz = Some(t.center_hz);
                    a.step_started_ns = Some(now_ns());
                    a.steps_done += 1;
                    took = Some(retune_took);
                    a.retries = 0;
                    a.retry_since = None;
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
                // T-542: a `conflict` is this run re-plumbing itself, and that is bounded by time,
                // not by a number of attempts — see [`REPLUMB_RETRY_BUDGET`]. The count stays as
                // the floor for every transient; the budget only ever *extends* it, and only for
                // the one refusal whose duration is known.
                let waited = st
                    .active
                    .as_ref()
                    .and_then(|a| a.retry_since)
                    .map_or(Duration::ZERO, |t| t.elapsed());
                let replumbing = matches!(e, LiveControlError::Conflict(_));
                if transient
                    && (retries < STEP_RETRIES || (replumbing && waited < REPLUMB_RETRY_BUDGET))
                {
                    // Nothing the user did took the radio (a user action yields the scan before it
                    // is attempted, and this result would then have been discarded above). Take the
                    // same step again.
                    if let Some(a) = st.active.as_mut() {
                        a.retries += 1;
                        a.retry_since.get_or_insert_with(Instant::now);
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
                            format!(
                                " after {retries} retries over {:.1} s",
                                waited.as_secs_f64()
                            )
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
        // T-965: fold the step's measured cost into the front end's running mean. Outside the
        // `Ok` arm so `st.active`'s borrow does not span it, and only for a step that happened.
        if let Some(d) = took {
            st.retune_ns_total = st
                .retune_ns_total
                .saturating_add(i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));
            st.retunes_measured += 1;
        }
    }
}

/// T-965: `budget` with a measured per-step cost priced in, or left stating that it has none.
///
/// One function so the price is the same on the preview, on the running scan's status and in the
/// commitment line — the three places a user reads it — and so `None` can never quietly become a
/// zero on one of the three.
fn priced(budget: ScanBudget, overhead_ns: Option<i64>) -> ScanBudget {
    match overhead_ns {
        Some(o) => budget.with_step_overhead(o),
        None => budget,
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
        /// T-542: refuse with `conflict` ("a re-plumb is in progress") rather than `device_busy`.
        /// They are both transient, and the scan must treat them differently, because only one of
        /// them is bounded by the pipeline's re-plumb budget.
        replumbing: Mutex<bool>,
        calls: AtomicU64,
        rates: Mutex<Vec<f64>>,
        /// T-965: how long this front end takes to retune. A real one is not instant — it
        /// reprograms the synthesiser, restarts the stream, and on a class or rate boundary the run
        /// re-plumbs behind it — and that cost is what the scan's price used to omit.
        retune_delay: Mutex<Duration>,
    }

    impl Fake {
        fn new() -> Arc<Self> {
            let mut caps = SourceCapabilities::hackrf_one();
            caps.frequency_ranges = vec![hk_core::source::FrequencyRange {
                min_hz: 100e6,
                max_hz: 160e6,
            }];
            caps.sample_rates = SampleRates::Continuous {
                min_hz: 2e6,
                max_hz: 20e6,
            };
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
                replumbing: Mutex::new(false),
                calls: AtomicU64::new(0),
                rates: Mutex::new(Vec::new()),
                retune_delay: Mutex::new(Duration::ZERO),
            })
        }
        /// The same front end left at a narrow window, as a live run at 2.4 Msps is.
        fn at_rate(rate_hz: f64) -> Arc<Self> {
            let f = Self::new();
            f.tuning
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .sample_rate_hz = rate_hz;
            f
        }
        fn rates(&self) -> Vec<f64> {
            self.rates
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
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
            let delay = *self
                .retune_delay
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
            if *self
                .replumbing
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
            {
                return Err(LiveControlError::Conflict(
                    "a re-plumb is in progress".into(),
                ));
            }
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
        fn set_rate(&self, hz: f64) -> Result<LiveTuning, LiveControlError> {
            self.rates
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(hz);
            let mut t = self.tuning.lock().unwrap_or_else(PoisonError::into_inner);
            t.sample_rate_hz = hz;
            Ok(t.clone())
        }
        // T-529: the sweep's steps are `set_center` and nothing else (see the module docs), so a
        // whole-window commit is not something this fake has to model. Refusing says that out
        // loud; silently applying half of it is the defect that route exists to remove.
        fn set_window(&self, _c: f64, _r: f64) -> Result<LiveTuning, LiveControlError> {
            Err(LiveControlError::Unsupported("window"))
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
                step: None,
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

    /// T-965: **the stated pass length has to match the pass the user waits for.**
    ///
    /// The live failure: `Scan everything (fast)` priced 418 steps × 0.3 s as a 125.4 s pass and
    /// took **237 s**, because the price counted the listening and nothing else. A step is a
    /// retune — the synthesiser, the stream restart and, at a class or rate boundary, the run's
    /// re-plumb — and none of that advances the sample clock the dwells are measured on, so a
    /// price computed from the dwells alone prices it at exactly zero.
    ///
    /// Here the front end takes [`RETUNE_DELAY`] to retune, which the worker times where it is
    /// paid. What this asserts:
    ///
    /// 1. Before anything has been timed, the price is the dwells and **says it is a floor** — the
    ///    `None`-is-not-zero rule, the same one `Coverage::Unobserved` and `BiasTee::Unknown` obey.
    /// 2. Once steps have run, `step_overhead_s` is the measured retune cost and `pass_s` is
    ///    `steps × (dwell + it)` — so the served number is the wall-clock pass.
    /// 3. And the dwell-only figure **understates** it by the whole per-step cost. That is the
    ///    defect's own shape as an assertion: drop the overhead back out of `pass_s` and this fails.
    #[test]
    fn the_priced_pass_matches_the_pass_the_retunes_actually_cost() {
        /// Long enough to dominate the dwell below (a real HackRF step cost ~268 ms), short enough
        /// that four steps are a fraction of a second.
        const RETUNE_DELAY: Duration = Duration::from_millis(30);
        const DWELL_S: f64 = 0.02;

        let live = Fake::new();
        *live
            .retune_delay
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = RETUNE_DELAY;
        let r = ScanRunner::new(live.clone() as Arc<dyn LiveControl>);

        // 1. Nothing measured yet: the dwells, and an admission that they are not the whole cost.
        let p = r
            .prepare(&ScanRequest {
                freq: None,
                dwell_s: Some(DWELL_S),
                step: None,
            })
            .expect("prepared");
        assert_eq!(p.budget.step_overhead_ns, None);
        assert!(!p.budget.overhead_measured());
        assert_eq!(p.budget.pass_ns, p.budget.dwell_total_ns);
        assert!(
            p.budget.statement().contains("a floor, not the answer"),
            "{}",
            p.budget.statement()
        );

        r.start(&ScanRequest {
            freq: None,
            dwell_s: Some(DWELL_S),
            step: None,
        })
        .expect("started");
        assert!(wait_for(|| live.centers().len() >= 4));

        // 2. The served budget prices the measured retune, once per step.
        let j = r.json();
        let b = &j["budget"];
        assert_eq!(b["overhead_measured"], json!(true), "{b}");
        let overhead_s = b["step_overhead_s"].as_f64().expect("measured");
        let want = RETUNE_DELAY.as_secs_f64();
        assert!(
            (want..want + 0.5).contains(&overhead_s),
            "measured {overhead_s} s per step against a {want} s retune"
        );
        let steps = b["steps"].as_f64().expect("steps");
        let dwell_total_s = b["dwell_total_s"].as_f64().expect("dwell total");
        let pass_s = b["pass_s"].as_f64().expect("pass");
        assert!(
            (pass_s - (dwell_total_s + steps * overhead_s)).abs() < 1e-6,
            "pass {pass_s} s against {dwell_total_s} s of dwell + {steps} x {overhead_s} s"
        );
        // The progress block states the same measurement, so a watching client can compare the
        // stated pass with the measured one without recomputing either.
        let pr = &j["progress"];
        assert_eq!(
            pr["measured_step_overhead_s"].as_f64(),
            Some(overhead_s),
            "{pr}"
        );
        assert!((pr["measured_pass_s"].as_f64().expect("measured pass") - pass_s).abs() < 1e-6);

        // 3. And the old, dwell-only figure understates the pass by the whole per-step cost.
        assert!(
            pass_s > dwell_total_s * 1.5,
            "a {want} s retune against a {DWELL_S} s dwell must dominate the pass: {pass_s} s \
             priced, {dwell_total_s} s of listening"
        );
        assert!(
            !j["budget"]["statement"]
                .as_str()
                .expect("statement")
                .contains("a floor, not the answer"),
            "{}",
            j["budget"]["statement"]
        );
        r.stop();
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
                step: None,
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
                step: None,
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
            step: None,
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
            step: None,
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
            step: None,
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
            step: None,
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

    /// **T-542: a sweep must outlast a re-plumb, because a re-plumb is twenty times longer than
    /// the retry budget used to be.**
    ///
    /// The user's report: a `POST /api/control/scan` over 1 MHz–6000 MHz at dwell 1 s takes the
    /// backend down. On the live HackRF it yielded at **step 19 of 3334** —
    /// *"could not retune to 34.300000 MHz after 3 retries: a re-plumb is in progress"* — because a
    /// step that crosses a content-class boundary re-plumbs the segment, a re-plumb there takes
    /// 8–19 s, and three retries at [`STEP_RETRY_BACKOFF`] gave up after 1.5 s. Every mock run
    /// passed, because a mock re-plumbs far inside that.
    ///
    /// The refusal is transient and its duration is *known* ([`REPLUMB_RETRY_BUDGET`]), so the
    /// step waits it out. Note what is not asserted: that the step is skipped. It never is.
    #[test]
    fn a_step_waits_out_a_re_plumb_far_past_the_retry_count() {
        let live = Fake::new();
        *live
            .replumbing
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = true;
        let r = ScanRunner::new(live.clone() as Arc<dyn LiveControl>);
        r.start(&ScanRequest {
            freq: None,
            dwell_s: Some(10.0),
            step: None,
        })
        .expect("started");

        // Past STEP_RETRIES × STEP_RETRY_BACKOFF (1.5 s) by a clear margin, and well inside
        // REPLUMB_RETRY_BUDGET: before the fix the scan had yielded by now.
        assert!(
            wait_for(|| live.calls.load(Ordering::SeqCst) >= STEP_RETRIES as u64 + 2),
            "the step stopped being retried"
        );
        let v = r.json();
        assert_eq!(
            v["state"], "running",
            "the sweep gave up on a re-plumb it should have waited out: {v}"
        );
        assert_eq!(v["progress"]["step"], 0, "and never skipped the step: {v}");
        assert!(live.centers().is_empty());

        // The re-plumb finishes; the sweep carries on from the same step, having lost nothing.
        *live
            .replumbing
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = false;
        assert!(wait_for(|| !live.centers().is_empty()));
        assert!(
            (live.centers()[0] - 107.5e6).abs() < 1e-3,
            "{:?}",
            live.centers()
        );
        assert_eq!(r.json()["state"], "running");
        r.stop();
    }

    /// A second scan over one front end is two policies fighting for one tune.
    #[test]
    fn a_second_scan_is_refused_while_one_runs() {
        let live = Fake::new();
        let r = ScanRunner::new(live as Arc<dyn LiveControl>);
        let req = ScanRequest {
            freq: None,
            dwell_s: Some(10.0),
            step: None,
        };
        r.start(&req).expect("started");
        let e = r.start(&req).expect_err("refused");
        assert_eq!(e.status, 409);
        r.stop();
    }

    /// The pipeline's bin-width rule (`hk_pipeline::detection_resolution`), restated for tests;
    /// the contract test in hk-cli checks the served value against the real one.
    fn bins() -> BinWidth {
        Arc::new(|fs: f64| {
            let fft = ((fs / 5_000.0).ceil() as usize)
                .next_power_of_two()
                .clamp(512, 4096);
            fs / fft as f64
        })
    }

    /// T-517: coarse widens the window and keeps the bins. From 2.4 Msps (1.8 MHz usable) the
    /// fake's 60 MHz is 34 fine steps; coarse tiles at 19.2 Msps (14.4 MHz usable): 5 steps, with
    /// the SAME bin width, and it says before anything moves that it will change the rate.
    #[test]
    fn coarse_is_fewer_wider_steps_at_the_same_bin_width() {
        let live = Fake::at_rate(2.4e6);
        let r = ScanRunner::new(live.clone() as Arc<dyn LiveControl>).with_bin_width(bins());
        let price = |step| {
            r.prepare(&ScanRequest {
                freq: None,
                dwell_s: Some(0.5),
                step,
            })
            .expect("prepared")
        };
        let (omitted, fine, coarse) = (
            price(None),
            price(Some(ScanStep::Fine)),
            price(Some(ScanStep::Coarse)),
        );
        assert_eq!(
            omitted.steps(),
            fine.steps(),
            "omitting the step is today's fine step"
        );
        assert_eq!(fine.steps(), 34);
        assert_eq!(coarse.steps(), 5);
        assert!((fine.budget.step_span_hz - 1.8e6).abs() < 1e-3);
        assert!((coarse.budget.step_span_hz - 14.4e6).abs() < 1e-3);
        assert_eq!(coarse.rate_hz, 19.2e6);
        assert!(coarse.changes_rate() && !fine.changes_rate());
        // FREQUENCY RESOLUTION IS UNCHANGED: coarse is fewer windows, never blurrier ones.
        assert_eq!(fine.bin_hz, Some(4_687.5));
        assert_eq!(coarse.bin_hz, fine.bin_hz);
        let j = coarse.json();
        assert_eq!(j["plan"]["step"], "coarse");
        assert_eq!(j["plan"]["bin_hz"], json!(4_687.5));
        assert_eq!(j["plan"]["changes_rate"], json!(true));
        // Pricing moved nothing.
        assert!(live.rates().is_empty() && live.centers().is_empty());
    }

    /// Without a stated resolution a coarse step cannot promise identical bins, so it is refused
    /// rather than taken blind; fine still works.
    #[test]
    fn coarse_without_a_stated_resolution_is_refused() {
        let live = Fake::at_rate(2.4e6);
        let r = ScanRunner::new(live as Arc<dyn LiveControl>);
        let e = r
            .prepare(&ScanRequest {
                step: Some(ScanStep::Coarse),
                ..ScanRequest::default()
            })
            .expect_err("refused");
        assert_eq!(e.status, 409);
        assert!(r.prepare(&ScanRequest::default()).is_ok());
    }

    /// A coarse scan sets its wider rate once, through the same device path, and then steps by
    /// retunes alone.
    #[test]
    fn a_coarse_scan_sets_its_rate_once_then_retunes() {
        let live = Fake::at_rate(2.4e6);
        let r = ScanRunner::new(live.clone() as Arc<dyn LiveControl>).with_bin_width(bins());
        r.start(&ScanRequest {
            freq: None,
            dwell_s: Some(0.02),
            step: Some(ScanStep::Coarse),
        })
        .expect("started");
        assert!(wait_for(|| live.centers().len() >= 6));
        r.stop();
        assert_eq!(
            live.rates(),
            vec![19.2e6],
            "one rate change, before the first step"
        );
        let c = live.centers();
        assert!(
            (c[0] - 106e6).abs() < 1e-3,
            "60 MHz in five 12 MHz slices: {c:?}"
        );
    }
}
