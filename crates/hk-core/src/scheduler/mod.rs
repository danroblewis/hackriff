//! Attention scheduler (C04, ADR-0005 first version): where the single half-duplex window points
//! at every instant. It turns a [`hk_model::ScanPlan`] into a deterministic stream of
//! [`ScheduleStep`]s, which [`StepApplier`] sends to a source through its
//! [`crate::SourceControl`]. It is a control-plane object: decisions happen at step boundaries,
//! never on the sample path.
//!
//! # Policy (v1)
//!
//! - **Discovery pass** ([`CompiledPlan`]): regions are clipped to the source's frequency ranges
//!   (what lies outside is reported, not scheduled), merged where they overlap, cut at RF-path
//!   boundaries and gain-table band edges, and tiled with hops no wider than the usable span
//!   (`sweep_rate × usable_fraction`, capped at `max_span_hz`: 15 MHz of a 20 Msps window by
//!   default), less `seam_guard_fraction` so seams can sit inside the filter passband (default 0:
//!   seams at ±7.5 MHz in the roll-off; see [`SchedulerConfig::seam_guard_fraction`] for the
//!   sensitivity vs pass-length trade-off). Bands narrower than half a span are offset-tuned off
//!   DC. Dwell-only regions contribute long region windows instead of sweep hops. The pass visits
//!   hops by region priority, then frequency, and repeats; a plan update resumes at the
//!   equivalent position instead of restarting.
//! - **Alternation:** `sweeps_per_cycle` discovery steps, then up to `dwells_per_cycle` POI dwell
//!   slots, repeating. With no POIs (or a `SweepOnly` plan) discovery simply continues.
//! - **POI dwells** ([`Poi`], from T-006/T-007 confirmations): sized to the emitter (rate,
//!   quarter-span offset from DC, duration from the burst interval). POIs share dwell slots by
//!   weighted round robin on `interestingness × region priority`, clamped to a finite range and
//!   floored at 5 % of the strongest POI's weight so no POI starves. Emitters too wide to clear
//!   DC or fit the span are reported by [`Scheduler::poi_notes`].
//! - **Revisit targets:** the tightest target caps each dwell so that one pass plus its dwell
//!   slots fits inside it ([`CompiledPlan::dwell_cap_ns`]); plans that cannot meet a target get a
//!   [`PlanWarning`] instead of a silent miss. Per-region revisit rates are a follow-up.
//! - **Verification** (S4 rules 6–7): a POI offered with `verify` gets, on its next dwell slot, a
//!   group of interleaved gain-step blocks (A/B × `gain_step_pairs`, 0.5 s each; B = LNA ± 8 dB),
//!   retune dwells at ±`retune_delta_hz` (1 MHz), and optionally a sample-rate-change dwell for
//!   clock harmonics. Retunes shrink or are skipped ([`RetunePlan`]) when ±Δ would push the
//!   emitter across DC or past the usable edge. Every group step carries a verification group id;
//!   [`Verification`] pairs only captures of one group and feeds the trust tests through
//!   [`TrustEvaluator`], never on a clipped capture (a clipped A block is never a base).
//! - **Preemption:** explicit user intent ([`Scheduler::preempt`]) overrides everything until it
//!   ends or is released; the cut slot is rolled back and re-visited. Order: user intent >
//!   running verification group > sweep/dwell cycle. Pinned decoder leases and pass-driven
//!   dwells (C22/C23/C29) are later layers between intent and the cycle.
//! - **RF paths:** boundaries come from [`crate::SourceCapabilities::rf_path_boundaries_hz`]
//!   (HackRF One 2170 / 2740 MHz, verified against firmware `tuning.c`) unless the config
//!   overrides them; every step carries `rf_path`; sweep hops never straddle a path boundary, so
//!   the floor step at a path switch (S4 §3.7, ~2.74 GHz) always falls between hops and floors
//!   are never pooled across it (T-009 follow-up "floor-step mask at sweep path boundaries").
//! - **Accessory ports:** steps in a gain-table band with an `antenna_port` carry `accessory`
//!   and the plan warns [`PlanWarning::AccessoryTrustPending`]: detection trust near notch /
//!   filter-bank edges is pending T-028.
//!
//! # Not implemented (ADR-0005 revisit)
//!
//! The ADR's **multi-armed bandit** (UCB on the C12 interestingness score, exploit vs explore
//! with ITU-R SM.1880 POI-aware dwell lengths), the C12 score itself, decoder-demand leases,
//! pass/launch-window dwells, cron schedules and hackrf_sweep firmware sweep mode are not in this
//! version. `interestingness` is a caller-supplied placeholder and priorities are static.
//!
//! # TX exclusivity (placeholder)
//!
//! The radio is half-duplex. A C37 transmit request, once gated and authorised, will be a
//! scheduler-owned exclusive slot: it never overlaps a receive step, receive stops for its
//! duration (the steps around it carry the discontinuity), and only user intent ranks above it.
//! Nothing transmits here: [`Scheduler::request_tx_slot`] always returns
//! [`SchedulerError::TxGated`], and no step purpose is a TX slot.
//!
//! # Determinism and allocation
//!
//! Output depends only on the plan, settings, capabilities, clock readings and call sequence
//! ([`SyntheticClock`] in tests, [`WallClock`] live: monotonic, anchored to wall time at start,
//! so a wall-clock step cannot reorder steps or roll back a finished one). [`Scheduler::next_step`],
//! [`Scheduler::preempt`] and [`StepApplier::apply`] do not allocate; the POI queue is
//! preallocated. Survey open/close ([`SurveyLog`]) happens only at run and plan boundaries.

mod apply;
mod clock;
mod config;
mod core;
mod plan;
mod step;
mod survey;
mod verify;

pub use apply::{AppliedChanges, StepApplier};
pub use clock::{AnchoredClock, Clock, SyntheticClock, WallClock};
pub use config::{HACKRF_ONE_RF_PATH_BOUNDARIES_HZ, MAX_GAIN_STEP_PAIRS, SchedulerConfig};
pub use core::{
    Poi, PoiNotes, RetunePlan, RetuneSkip, ScheduleStats, Scheduler, SchedulerError, TxSlotRequest,
    UserIntent,
};
pub use plan::{
    CompiledPlan, CompiledRegion, Hop, HopKind, PlanError, PlanWarning, check_gains,
    pick_baseband_filter, pick_rate, rf_path,
};
pub use step::{GainSlot, PoiKey, Purpose, ScheduleStep};
pub use survey::{MemorySurveyLog, SurveyEvent, SurveyLog};
pub use verify::{
    AMP_NOMINAL_DB, CaptureTrust, TrustEvaluator, Verification, VerificationReport, VerifyError,
};
