//! Presence intervals: **when** an emitter was on the air (docs/07 §2.27, [ADR-0017] §1.1, stage
//! TM-5).
//!
//! An Emitter is an **identity that owns an ordered set of disjoint presence intervals**. The
//! interval is where a signal's time extent lives — not on the emitter, which carries only a
//! `first_seen`/`last_seen` **hull** and a lifetime `count`.
//!
//! The interval set is not new and needs no table: `emitter_observation` (migration 0001) is one
//! row per sighting source, carrying `(emitter_id, t_start, t_end, count, f_center)`. This module
//! is the rule that turns those raw source rows into intervals, and the derivations that read
//! them. Migration 0012 adds the only thing that was missing — an index on time.
//!
//! # The close/revive rule
//!
//! Source rows are sorted by start and folded together while the silence between them is at most
//! [`IdleGap`]:
//!
//! - **Overlapping rows normalise into one interval.** A track and a decode of the same minutes
//!   are one span of air time, not two — the existing "overlapping observations not counted twice"
//!   rule of docs/07 §2.11, given a name.
//! - **A silence longer than the idle gap closes the interval.** The next row starts a **new**
//!   interval on the **same emitter**. Nothing is deleted and nothing is rewritten; closure is a
//!   measurement fact (evidence stopped arriving), and it is permanent History.
//! - **…unless the signal comes back inside one further idle gap, which *revokes* the end**
//!   ([`IdleGap::revocable_nanos`], T-413). The two rows are then one interval again, the silence
//!   between them is recorded on it as a [revoked gap](PresenceInterval::revoked), and **no part of
//!   that silence is ever counted as time on air**. See "The end is revocable" below.
//! - **`open` is derived, never stored** ([`PresenceInterval::open`]): an interval is open while
//!   `now − t_end ≤ idle_gap`. Only the latest interval can be open, by construction. Storing a
//!   decision made under one parameter value is the measurement-versus-interpretation mistake
//!   docs/07's first rule forbids, so there is deliberately no `closed_at` column (ADR-0017 §8.3).
//! - **Revival appends.** A returning signal that entity resolution places on the same emitter
//!   gets a new row, and therefore a new interval, on the same `emitter_id`.
//!
//! **Closing is not decay.** Closing is a measurement fact and it is permanent History. Decay
//! lowers a *candidate's confidence in its hypothesis*; it never deletes and never touches
//! History (ADR-0017 §5), and it is the next section.
//!
//! # Decay: confidence as a function of observed absence (T-251, ADR-0017 TM-6)
//!
//! **What was left for decay to own, after window-scoping.** A candidate whose signal stopped
//! hours ago is not decayed out of the live list — it is simply **not in the window** (TM-3), and
//! no logic here is involved. Decay owns exactly one case: **a signal that stopped *inside* the
//! viewed window.** Its box is on screen, so it must stay listed; what it needs is an honest
//! state and a **lower rank**, never expiry. `on_air_s` cannot supply that rank on its own,
//! because it is blind to *when* inside the window the signal was on: a row that transmitted for
//! the window's first five seconds and one transmitting right now for five seconds have the same
//! `on_air_s` and therefore the same rank. [`Presence::confidence`] is the term that separates
//! them.
//!
//! **The law.** Let `silence` be the time from the latest in-window interval's `t_end` to the
//! window's live edge:
//!
//! ```text
//! confidence = 1                                while the interval is open (silence ≤ idle_gap)
//! confidence = exp(−(silence − idle_gap) / idle_gap)          once it has closed
//! confidence = 0                                when no interval intersects the window at all
//! ```
//!
//! **The time constant is the idle gap, and it is not a new number.** τ = [`IdleGap`] =
//! `clamp(2 × revisit_period, 1 s, 60 s)`, every constant of which was already derived and
//! measured in TM-5 (the section above). The reason it is the right τ is the same measurement
//! argument that sets the gap in the first place: **the idle gap is one unit of *observed*
//! absence.** A silence shorter than it is not evidence the emitter stopped — the receiver was
//! not listening — which is why confidence is flat at 1 there and the interval reads open. Past
//! it, the receiver can only learn "still nothing" once per gap, so the number of independent
//! absence observations accumulated is `(silence − idle_gap) / idle_gap`, and confidence falls by
//! `1/e` per observation. That is a likelihood shape with **no free parameter**: there is nothing
//! here to tune to make one screenshot look right, and changing the revisit period moves the
//! decay exactly as far as it moves closure.
//!
//! **What it is not.** It is not a per-tick decrement — ADR-0017 §5 calls that "the same pathology
//! inverted", a number that moves for reasons unrelated to evidence. Nothing here is incremented
//! or decremented by a clock: `confidence` is a pure function of interval boundaries and the view
//! edge, recomputed on every read, so it is **reversible by construction**. A returning signal
//! appends a new interval on the same emitter (TM-5), the latest in-window interval is open again,
//! and confidence is 1 — no revival path, no un-expiry, and no row to resurrect, because **decay
//! never deletes**. Nothing in this module writes.
//!
//! [`NEGLIGIBLE_CONFIDENCE`] is where the hypothesis stops being worth re-checking, and
//! [`recheck_horizon_s`] turns it back into a silence — the scheduler's re-verification horizon.
//!
//! # The idle gap is derived from the revisit period, not set per band
//!
//! *(ADR-0017 §11 open question 4, answered in TM-5.)* The reason is a measurement one: **a gap
//! shorter than the revisit period is not evidence of absence.** A receiver that returns to a
//! region every 2 s and sees a burst at `t = 0` and another at `t = 1.9` s has no evidence the
//! emitter was off in between — it was not listening. Closing an interval there would record an
//! absence nobody observed. So the gap that separates two intervals has to come from **how often
//! this receiver looked**, and [`IdleGap::from_revisit_s`] is the whole rule:
//!
//! ```text
//! idle_gap = clamp(REVISIT_FACTOR × revisit_period, MIN_IDLE_GAP_S, MAX_IDLE_GAP_S)
//! ```
//!
//! Every constant in it is taken from a rule that already exists in this codebase, so none of them
//! is a dial:
//!
//! | Constant | Value | Where it comes from |
//! |---|---|---|
//! | [`REVISIT_FACTOR`] | 2 | Absence needs **two** consecutive missed revisits; one missed visit is a scheduler skip. The bandit's own point-of-interest reporting already calls a gap real only past `2 ×` the nominal revisit (`hk_core::scheduler::bandit::poi`). |
//! | [`MIN_IDLE_GAP_S`] | 1 s | The tracker's `max_transition_gap_s`: it does not itself treat a silence this short as the end of an emission. Below it we would split what the detector joined. |
//! | [`MAX_IDLE_GAP_S`] | 60 s | The tracker's `idle_timeout_s`: past it the tracker has **already** closed the track, so two source rows further apart than this were judged discontinuous upstream. Re-joining them here would overrule a measurement with a parameter. |
//!
//! When the revisit period is unknown, [`IdleGap::conservative`] takes the 60 s end: claim no
//! absence that cannot be shown. (Measured check: the user's own stopped-and-returned FM station
//! has a 72 s silence, so it reads as two intervals even at the most conservative setting.)
//!
//! **"Unknown" has to mean unknown** (T-410, ADR-0019 §3). The gap *closes the interval*, so it is
//! also the **end detector's latency** — and once a box runs to the live edge until an END is
//! detected (ADR-0019 §1), a 60 s gap is a box over-claiming 60 s of silent air. Every reader in
//! the served path took `conservative()` because no caller declared a revisit period, yet a
//! receiver dwelling on one centre revisits that band every STFT frame and is not remotely
//! unknown. [`IdleGap::from_coverage`] measures the period off the spans the receiver was actually
//! observing in — the IQ ring's tune journal, which is the only thing that knows — and
//! [`IdleGap::continuous`] names the contiguous case that `from_revisit_s(0.0)` could not express.
//! `conservative()` is then reserved for its real meaning: **nobody recorded whether the receiver
//! looked.**
//!
//! **What this does to a burst source, stated plainly.** In the 902–928 MHz ISM playground
//! (T-254/T-255) a chatty sensor firing a 20 ms burst every 30 s is **fifty intervals, not one** —
//! 30 s of silence is far longer than any revisit-derived gap. That is the intended reading:
//! "50 events, 1.0 s on air" is honest, each burst's box is 20 ms tall, and bursts look like
//! bursts. Fifty intervals are cheap precisely because they are fifty intervals on **one** emitter,
//! not fifty rows. The fact that the sensor is *periodic* is said by `EmissionFeatures`
//! (period, duty cycle, burst length — ADR-0016 §5), which is where that belongs; it is not said by
//! smearing fifty transmissions into one span of mostly silence.
//!
//! # The end is revocable, and the window is one further idle gap (T-413, ADR-0019 §6.1)
//!
//! *(The user, 2026-09-17, answering the question ADR-0019 §6.1 flagged rather than decided.)* A
//! detected end is **provisional**: if the signal resumes within tolerance, **null the end and keep
//! the one interval open** rather than splitting it or spawning a new emitter. The tolerance is the
//! **existing** [`IdleGap`] — no new parameter.
//!
//! **Where the window is anchored, and why it cannot be the measured end.** An interval closes only
//! after a *full* idle gap of observed silence, so by the time an end exists to revoke, the silence
//! since the **measured** end is already exactly one gap. Measuring the revocation window from the
//! measured end would make it unreachable by construction: every resumption after an end is more
//! than one gap past it. So the window runs from **the end *event*** — the instant the decision was
//! taken, which is one gap past the measurement that provoked it:
//!
//! ```text
//! silence ≤ gap          → no end is detected at all      (the interval simply continues)
//! gap < silence ≤ 2×gap  → an end fired, and a resumption REVOKES it: ONE interval
//! silence > 2×gap        → the end stands; a resumption is a genuinely new interval
//! silence > 60 s         → never revocable, whatever the gap: the tracker already closed the
//!                          track, so the discontinuity is a measurement, not a parameter
//! ```
//!
//! **So the effective join tolerance is `2 × gap` although only one constant exists**, and the two
//! gaps are not the same measurement twice: the first is the observed absence that *justifies* the
//! end, the second is the observed absence that *confirms* it. One is the detection, the other is
//! its confirmation, and both are one unit of observed absence — the only unit this module has.
//! Equivalently, and with no arithmetic at all: **the end stands revocable for exactly as long as
//! [`confidence_after_silence`] is still above `1/e`**, its first e-fold, which is by definition one
//! independent absence observation after the one that closed the interval.
//!
//! **Time on air is never claimed across a revoked gap.** The rejoined interval keeps the silence on
//! itself ([`PresenceInterval::revoked`]) and [`PresenceInterval::duration_s`] and
//! [`Presence::on_air_s`] both subtract it. Without that, a sensor chattering at a 1.5 s cadence
//! under a 1 s gap would read as one interval of 75 s "on air" holding 1 s of emission — the
//! ADR-0017 hull pathology, reintroduced one level down. With it, the count of events changes and
//! the air time does not, which is the only honest way for a join to be free.
//!
//! **The ISM reading is untouched**, and that is a numeric fact rather than a hope: under contiguous
//! coverage the gap is 1 s, so the window closes 2 s after the measured end, and a sensor firing
//! every 30 s is fifty intervals exactly as before
//! ([`tests::a_chatty_burst_source_reads_as_one_interval_per_burst`]). Any revocation window wide
//! enough to swallow that cadence would have to be more than fifteen times the one derived here.
//!
//! **Why per-band is worse.** A per-band idle gap has no measurement behind it: it is a number
//! chosen until one screen looks right. It would make the same sensor read as one interval or
//! fifty depending only on which band it sits in, so moving a device from 433 MHz to 915 MHz would
//! change its event count with no change in the air. And a per-band gap wide enough to make an ISM
//! burst source "one interval" reintroduces exactly the pathology ADR-0017 exists to remove — a
//! span that is 99.97 % silence, presented as time on air — one level further down, where it is
//! harder to see.
//!
//! # Ongoing until an end is *observed* (T-940)
//!
//! **An interval is open until the receiver has *observed* one idle gap of silence after it** —
//! ADR-0019 §3 word for word, and the invariant it serves: a signal is ongoing, `end = null`, until
//! an end is affirmatively detected. Until T-940 the silence was `now − t_end` on the wall clock,
//! and on staging (2026-09-25) every FM station on the air, the Confirmed 101.3 MHz one included,
//! read `ended`. Two kinds of time were being counted as quiet that were never observed as quiet:
//!
//! - **Time the receiver spent tuned elsewhere.** [`Watched`] is the band's coverage (the IQ ring's
//!   tune journal — the same spans [`IdleGap::from_coverage`] measures the gap from), and only the
//!   part of a silence inside it counts. A band the receiver retuned away from is not a band that
//!   went quiet; `Coverage::Unobserved` is not quiet, on the time axis as on the frequency axis.
//! - **Time the detector had not yet reported.** An open track's row is refreshed as the tracker
//!   measures more of the emission, but between refreshes — and while a continuous carrier's burst
//!   is still in flight between split records — `t_end` trails the live edge by more than the 1 s
//!   floor. So a row whose source is still followed carries the tracker's own observed silence
//!   ([`ObservationSpan::live_silence_ns`]), and nothing past that report is read as silence.
//!
//! ```text
//! any source of the latest interval still followed → silence = what the tracker reported
//! every source closed                                → silence = watched part of [t_end, now]
//! open                                               ⇔ silence ≤ idle_gap
//! ```
//!
//! Both are measurements; the gap stays a parameter of the reading. What this deliberately does not
//! change is **joining**: two rows separated by an unobserved stretch remain two intervals, because
//! joining them would claim the stretch as time on air, which is the hull pathology ADR-0017 exists
//! to remove. The latest one reads open, so the emitter reads live — the only question liveness
//! asks.
//!
//! # `count` is History only
//!
//! Nothing in this module reads a sighting count. Liveness, on-air time and interval boundaries
//! come from `t_start`/`t_end` alone. `count` rides along on the interval as a lifetime total for
//! the History surface, where a monotonic counter is exactly right, and is **excluded from every
//! liveness decision and from live-list ranking** (docs/07 §2.11, ADR-0017 §5): it was the only
//! column that could hold "this is still here", which is why it grew to 582,500/h. What
//! accumulates instead is the open interval's `t_end`.
//!
//! [ADR-0017]: https://docs/adr/0017-time-extent-signal-model.md

use serde::{Deserialize, Serialize};

use crate::region::TimeRange;
use crate::time::Timestamp;

// ---------------------------------------------------------------------------------------------
// The idle gap. Every constant is taken from an existing measured rule; see the module docs.
// ---------------------------------------------------------------------------------------------

/// Consecutive missed revisits before a silence counts as absence (2): one missed visit is a
/// scheduler skip, not evidence the emitter stopped.
pub const REVISIT_FACTOR: f64 = 2.0;

/// Smallest idle gap, s (1). The tracker's `max_transition_gap_s`: a silence this short is not the
/// end of an emission even to the tracker, so nothing below it may split an interval.
pub const MIN_IDLE_GAP_S: f64 = 1.0;

/// Largest idle gap, s (60). The tracker's `idle_timeout_s`: past it the track was already closed
/// upstream, so two source rows further apart than this were judged discontinuous by a
/// measurement, and this module must not re-join them.
pub const MAX_IDLE_GAP_S: f64 = 60.0;

/// Confidence below which a hypothesis is not worth spending a dwell on (0.05).
///
/// Not a new dial: it is the scheduler's own `MIN_DWELL_SHARE` (`hk_core::scheduler`), the share
/// of the strongest point of interest's weight below which a POI is effectively starved of dwell
/// slots. A hypothesis the scheduler would no longer schedule is one there is no point
/// re-checking, so the same number bounds [`recheck_horizon_s`]. `hk-pipeline` asserts the two are
/// equal, so this cannot drift out of step with the scheduler it is taken from.
pub const NEGLIGIBLE_CONFIDENCE: f64 = 0.05;

const NS_PER_S: f64 = 1e9;

/// The silence after which a presence interval closes, derived from the revisit period.
///
/// See the module docs for the derivation and for what it does to a burst source. This is a
/// *parameter of the reading*, never stored: change it and closure re-derives correctly, which is
/// the whole reason no `closed_at` column exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct IdleGap(i64);

impl IdleGap {
    /// The gap implied by a revisit period of `revisit_s`:
    /// `clamp(2 × revisit, 1 s, 60 s)`. A non-finite or non-positive period means "unknown", which
    /// takes [`Self::conservative`] — never a shorter gap, because a shorter gap claims an absence
    /// that was not observed.
    pub fn from_revisit_s(revisit_s: f64) -> Self {
        if !revisit_s.is_finite() || revisit_s <= 0.0 {
            return Self::conservative();
        }
        let s = (REVISIT_FACTOR * revisit_s).clamp(MIN_IDLE_GAP_S, MAX_IDLE_GAP_S);
        Self((s * NS_PER_S) as i64)
    }

    /// The gap to use when the revisit period is not known: the [`MAX_IDLE_GAP_S`] end, so no
    /// absence is claimed that cannot be shown.
    pub fn conservative() -> Self {
        Self((MAX_IDLE_GAP_S * NS_PER_S) as i64)
    }

    /// The gap for a band the receiver **never looked away from**: the [`MIN_IDLE_GAP_S`] floor.
    ///
    /// Not a fourth option and not a new number — it is [`Self::from_revisit_s`] of a revisit
    /// period shorter than half the floor, which clamps there. It exists as a *name* because
    /// `from_revisit_s(0.0)` cannot express it: a zero period reads as "unknown" and takes the
    /// conservative 60 s, so continuous dwell and no-information-at-all were indistinguishable
    /// (ADR-0019 §3). A receiver dwelling on one centre revisits that band **every STFT frame**;
    /// calling that unknown discards a measurement the run has in hand.
    ///
    /// The floor itself is the tracker's `max_transition_gap_s`: below it we would split what the
    /// detector joined, so no evidence of continuous listening can push the gap any lower.
    pub fn continuous() -> Self {
        Self((MIN_IDLE_GAP_S * NS_PER_S) as i64)
    }

    /// The gap implied by the **coverage** of a band: how often this receiver actually looked at
    /// it, read off the spans it was observed in (ADR-0019 §3).
    ///
    /// This is the same rule as [`Self::from_revisit_s`] with the revisit period **measured**
    /// rather than declared. The revisit period of a band is the largest silence between
    /// consecutive observations of it inside `window`, so:
    ///
    /// - **contiguous coverage** (one span, or spans that touch) ⇒ [`Self::continuous`];
    /// - **combed coverage**, largest silence `P` ⇒ `from_revisit_s(P)`;
    /// - **no spans at all** ⇒ [`Self::conservative`] — nobody recorded whether the receiver
    ///   looked, which is the one case that genuinely is unknown.
    ///
    /// Only the part of each span inside `window` counts, and the silences before the first span
    /// and after the last are **not** revisit gaps: they are the window reaching past the coverage
    /// the caller asked about, not the receiver looking away mid-watch. Spans may arrive in any
    /// order and may overlap (several front ends on one band, or a retune inside a dwell).
    pub fn from_coverage(spans: &[TimeRange], window: TimeRange) -> Self {
        let (w0, w1) = (window.start.as_unix_nanos(), window.end.as_unix_nanos());
        let mut clipped: Vec<(i64, i64)> = spans
            .iter()
            .map(|s| {
                (
                    s.start.as_unix_nanos().max(w0),
                    s.end.as_unix_nanos().min(w1),
                )
            })
            .filter(|(a, b)| b > a)
            .collect();
        if clipped.is_empty() {
            return Self::conservative();
        }
        clipped.sort_unstable();
        // The largest silence *between* observations. Coalescing as we go means overlapping spans
        // contribute no gap at all, which is what "two devices watched the same band" means.
        let (mut worst_ns, mut reach) = (0i64, clipped[0].1);
        for &(a, b) in &clipped[1..] {
            worst_ns = worst_ns.max(a.saturating_sub(reach).max(0));
            reach = reach.max(b);
        }
        if worst_ns <= 0 {
            return Self::continuous();
        }
        Self::from_revisit_s(worst_ns as f64 / NS_PER_S)
    }

    /// The gap in nanoseconds.
    pub const fn as_nanos(self) -> i64 {
        self.0
    }

    /// The silence a resumption may cross and still **revoke** the end that fired inside it:
    /// `2 × gap` (T-413, ADR-0019 §6.1; see the module docs for the derivation).
    ///
    /// Not a second parameter, and not a wider gap. It is this gap twice, for two different
    /// statements: the first is the observed absence that closes the interval, the second the
    /// observed absence that confirms the closure. A signal returning before the second has
    /// accumulated nulls the end and the interval stays **one** interval; one returning after it
    /// starts a new one. Anchored on the end *event* rather than on the measured end, because the
    /// end event is already one gap past the measurement and a window measured from the measurement
    /// could never be reached.
    ///
    /// **Clamped by [`MAX_IDLE_GAP_S`], which is the one ceiling revocation may not lift.** Past it
    /// the tracker has already closed the track, so two source rows further apart than 60 s were
    /// judged discontinuous by a **measurement** upstream, and ADR-0019 §6 is explicit that they
    /// "can never be rejoined" — revoking there would overrule a measurement with a parameter,
    /// which is exactly what the clamp on the gap itself exists to prevent. It binds only when the
    /// revisit period is unknown or very long: under a live dwell the gap is 1 s and the window is
    /// 2 s, nowhere near it. (Measured check: the user's stopped-and-returned FM station has a 72 s
    /// silence, and stays two intervals.)
    pub const fn revocable_nanos(self) -> i64 {
        let max = (MAX_IDLE_GAP_S * NS_PER_S) as i64;
        let doubled = self.0.saturating_mul(2);
        if doubled > max { max } else { doubled }
    }

    /// The gap in seconds.
    pub fn as_secs_f64(self) -> f64 {
        self.0 as f64 / NS_PER_S
    }
}

impl Default for IdleGap {
    fn default() -> Self {
        Self::conservative()
    }
}

// ---------------------------------------------------------------------------------------------
// Decay (T-251, ADR-0017 TM-6). Derived on every read; nothing here is stored, incremented or
// decremented, and nothing here deletes. See the module docs for the law and its time constant.
// ---------------------------------------------------------------------------------------------

/// Confidence in a candidate's hypothesis after `silence_s` of silence, under `gap`:
/// `1` while the silence is no longer than the gap (the receiver has observed no absence at all),
/// then `exp(−(silence − gap) / gap)` — one `1/e` per further gap of *observed* absence.
///
/// The time constant is the gap itself, because the gap is one unit of observed absence
/// ([`IdleGap`]); there is no free parameter. A non-finite silence reads as no evidence of
/// absence rather than as total absence: a missing measurement never manufactures decay.
pub fn confidence_after_silence(silence_s: f64, gap: IdleGap) -> f64 {
    let g = gap.as_secs_f64();
    if !silence_s.is_finite() || g <= 0.0 || silence_s <= g {
        return 1.0;
    }
    (-((silence_s - g) / g)).exp()
}

/// The silence at which [`confidence_after_silence`] reaches [`NEGLIGIBLE_CONFIDENCE`], s:
/// `gap × (1 − ln 0.05)` ≈ `4 × gap`. Past it the hypothesis is no longer worth a dwell, so it is
/// the scheduler's re-verification horizon — the point at which a stopped candidate stops being
/// re-checked. Both constants in it are already-measured ones.
pub fn recheck_horizon_s(gap: IdleGap) -> f64 {
    gap.as_secs_f64() * (1.0 - NEGLIGIBLE_CONFIDENCE.ln())
}

// ---------------------------------------------------------------------------------------------
// The objects
// ---------------------------------------------------------------------------------------------

/// One raw source row of the observation ledger, before normalisation: what a single track or
/// decode sighting saw. The input to [`intervals_from_spans`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ObservationSpan {
    /// The span this source was observed over.
    pub time: TimeRange,
    /// Sightings it counted (History only — never read by any derivation here).
    pub count: u64,
    /// Observed centre, Hz, when the source recorded one.
    pub f_center_hz: Option<f64>,
    /// **`Some` while the source is a track the pipeline is still following** (T-940): the silence
    /// the tracker had *observed* since [`Self::time`]'s end at its latest report, ns — 0 while a
    /// burst is in flight. `None` for a closed source, whose end is final.
    ///
    /// This is what lets an open track read open without the wall clock deciding it: the time since
    /// the tracker's last report is time the detector has not yet analysed, and unreported is not
    /// quiet. See "Ongoing until an end is observed" in the module docs.
    pub live_silence_ns: Option<i64>,
}

/// A maximal span during which one emitter was continuously on the air, within the detector's
/// ability to tell continuity from gaps (docs/07 §2.27).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PresenceInterval {
    /// The time extent. **This**, not the emitter's hull, is how long the signal was on air —
    /// less [`Self::revoked`], which is measured silence and is never on-air time.
    pub time: TimeRange,
    /// Source rows normalised into this interval (≥ 1).
    pub sources: u32,
    /// Sightings summed over those rows: a **History total**. Never read by liveness or ranking,
    /// and — because overlapping sources describing the same minutes each keep their own count —
    /// not a count of distinct transmissions.
    pub count: u64,
    /// The latest centre measured within this interval, Hz, when a source recorded one.
    pub f_center_hz: Option<f64>,
    /// Derived, never stored: the **observed** silence after `t_end` is at most the idle gap (T-940;
    /// see the module docs). Only the latest interval can be open.
    pub open: bool,
    /// Silences inside this interval that a resumption **revoked the end of** (T-413): each is
    /// longer than one [`IdleGap`] — long enough that an end was detected in it — and no longer
    /// than [`IdleGap::revocable_nanos`], so the signal came back inside the revocation window and
    /// the interval stayed one interval on one emitter.
    ///
    /// **Measured silence, never time on air.** [`Self::duration_s`] and [`Presence::on_air_s`]
    /// both subtract it, so revoking an end changes how many *events* were seen and never how much
    /// air was claimed. Empty for the overwhelming majority of intervals; it exists so that "one
    /// interval" can never quietly become "on air throughout".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub revoked: Vec<TimeRange>,
}

impl PresenceInterval {
    /// Time on air in this interval, s: its extent **less** every [revoked gap](Self::revoked),
    /// which is silence the receiver measured and this interval must not claim.
    pub fn duration_s(&self) -> f64 {
        let ns = self.time.duration_ns().max(0) - self.revoked_ns();
        ns.max(0) as f64 / NS_PER_S
    }

    /// Total measured silence inside this interval whose end was revoked, ns.
    pub fn revoked_ns(&self) -> i64 {
        self.revoked
            .iter()
            .map(|g| g.duration_ns().max(0))
            .fold(0i64, i64::saturating_add)
    }

    /// Total measured silence inside this interval whose end was revoked, s. The number the row and
    /// the box state, so "one interval" is never read as "on air throughout".
    pub fn revoked_s(&self) -> f64 {
        self.revoked_ns() as f64 / NS_PER_S
    }
}

/// Whether an emitter is on the air, as seen through one view window (docs/07 §2.11, ADR-0017
/// §2.3). Derived, never stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Liveness {
    /// An interval intersecting the window is open at the live edge.
    Live,
    /// Its latest interval intersecting the window is closed: the signal happened, and stopped.
    Ended,
    /// No interval intersects the window. A Candidate in this state is simply not listed; a
    /// Confirmed row is (it is a catalogue entry), marked absent.
    Absent,
}

impl Liveness {
    /// The wire form (`live` / `ended` / `absent`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Ended => "ended",
            Self::Absent => "absent",
        }
    }
}

/// An emitter's presence as seen through one view window `[t0, t1]`. Every field is derived from
/// interval boundaries; none is derived from `count`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Presence {
    /// Intervals intersecting the window.
    pub intervals: u64,
    /// Time on air **inside** the window, s: Σ of each interval's intersection with it, **less**
    /// every [revoked gap](PresenceInterval::revoked) inside it (T-413). This is the honest
    /// replacement for ranking by lifetime `count`, and subtracting the revoked silence is what
    /// stops a revoked end buying air time it was never measured to have.
    pub on_air_s: f64,
    /// The latest interval intersecting the window, if any.
    pub last_interval: Option<PresenceInterval>,
    /// Liveness in this window.
    pub liveness: Liveness,
    /// When the signal stopped, for [`Liveness::Ended`] — the end of `last_interval`. `None`
    /// while live or absent. This is the "ended 4 minutes ago" the product previously could not
    /// say.
    pub ended_t: Option<Timestamp>,
    /// Silence from the latest in-window interval's end to the window's live edge, s. `None` when
    /// no interval intersects the window. 0 while the signal is still on the air.
    pub silence_s: Option<f64>,
    /// Confidence in the candidate's hypothesis, 0–1 (T-251, ADR-0017 TM-6): `1` while live,
    /// `exp(−(silence − idle_gap) / idle_gap)` once its latest in-window interval has closed, `0`
    /// when none intersects the window. See the module docs for the law and its time constant.
    ///
    /// **A rank, not a lifetime.** It orders a candidate that stopped inside the window below one
    /// transmitting now; it never expires a row, never deletes anything and never touches
    /// History. It is re-derived from interval boundaries on every read, so a returning signal's
    /// new interval restores it with no revival path of its own.
    pub confidence: f64,
}

// ---------------------------------------------------------------------------------------------
// The rule
// ---------------------------------------------------------------------------------------------

/// When the receiver was actually looking at one band: the spans it observed it in (T-940).
///
/// The silence that closes an interval is **observed** silence — ADR-0019 §3's rule, and the
/// reason the idle gap exists at all: *a gap shorter than the revisit period is not evidence of
/// absence, because the receiver was not listening.* The gap was already measured off coverage
/// ([`IdleGap::from_coverage`]); the silence it is compared against was still the wall clock, so a
/// band the receiver had retuned away from read as a band that had gone quiet. This is the same
/// coverage, asked the second question.
///
/// **The record has a horizon.** The tune journal it comes from (the IQ ring) remembers minutes,
/// not the whole history, so before [`Self::recorded`]'s `from` nobody can say whether the receiver
/// looked. That stretch keeps the only reading available — elapsed time — rather than being read
/// as unobserved: a station last measured hours ago, on a band the ring no longer remembers
/// watching, must not come back as "still on the air". Unknown is neither quiet nor on the air, and
/// elapsed time is the reading every surface already used for it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Watched {
    /// Start of the coverage record, Unix ns. `None`: nothing recorded at all.
    from: Option<i64>,
    /// Coalesced, sorted `[start, end)` spans in Unix ns, inside the record.
    spans: Vec<(i64, i64)>,
}

impl Watched {
    /// No record of when the receiver looked: silence is elapsed time (the pre-T-940 reading,
    /// unchanged for a replay store or a server without an IQ ring).
    pub fn unrecorded() -> Self {
        Self::default()
    }

    /// A coverage record that begins at `from`, in which the receiver watched the band over `spans`
    /// (any order, may overlap). After `from`, only time inside `spans` is observed; an empty list
    /// is therefore a **record that it never looked**, in which no silence at all was observed.
    /// Before `from`, silence is elapsed time ([`Self::unrecorded`]'s reading).
    pub fn recorded(from: Timestamp, spans: &[TimeRange]) -> Self {
        let mut v: Vec<(i64, i64)> = spans
            .iter()
            .map(|s| (s.start.as_unix_nanos(), s.end.as_unix_nanos()))
            .filter(|(a, b)| b > a)
            .collect();
        v.sort_unstable();
        let mut out: Vec<(i64, i64)> = Vec::with_capacity(v.len());
        for (a, b) in v {
            match out.last_mut() {
                Some(last) if a <= last.1 => last.1 = last.1.max(b),
                _ => out.push((a, b)),
            }
        }
        Self {
            from: Some(from.as_unix_nanos()),
            spans: out,
        }
    }

    /// Whether any coverage record exists — i.e. whether [`Self::observed_ns`] measures watched
    /// time (judged against the continuous floor) or elapsed time (judged against the gap).
    pub fn is_recorded(&self) -> bool {
        self.from.is_some()
    }

    /// How much of `[a, b]` counts as observed, ns: the only part of a silence that is evidence of
    /// absence. Elapsed time before the record's start, watched time inside it.
    pub fn observed_ns(&self, a: i64, b: i64) -> i64 {
        if b <= a {
            return 0;
        }
        let Some(from) = self.from else {
            return b - a;
        };
        let unknown = (b.min(from) - a).max(0);
        let a = a.max(from);
        let watched = self
            .spans
            .iter()
            .map(|&(s, e)| (e.min(b) - s.max(a)).max(0))
            .fold(0i64, i64::saturating_add);
        unknown.saturating_add(watched)
    }
}

/// Normalises raw source rows into the emitter's ordered set of **disjoint** presence intervals
/// (module docs): overlapping rows and rows separated by at most `gap` fold together; a silence
/// past `gap` detects an end, which a row arriving within one *further* gap **revokes**
/// ([`IdleGap::revocable_nanos`], T-413) — the rows fold together and the silence is recorded as a
/// [revoked gap](PresenceInterval::revoked) rather than claimed as air; a longer silence closes the
/// interval for good and the next row opens a new one. `now` is the live edge, and decides only
/// which interval reads as open — a revoked end never delays closure, so the end detector's latency
/// is exactly what it was.
///
/// Reads `time` only. `count` is carried through untouched.
pub fn intervals_from_spans(
    spans: &[ObservationSpan],
    gap: IdleGap,
    now: Timestamp,
) -> Vec<PresenceInterval> {
    intervals_observed(spans, gap, now, &Watched::unrecorded())
}

/// [`intervals_from_spans`] with the silence that decides `open` measured as **observed** silence
/// (T-940): only the part of it the receiver was watching (`watched`), and — for a source the
/// pipeline is still following — only what the tracker has reported
/// ([`ObservationSpan::live_silence_ns`]). See "Ongoing until an end is observed" in the module
/// docs. Joining rows into intervals is unchanged.
pub fn intervals_observed(
    spans: &[ObservationSpan],
    gap: IdleGap,
    now: Timestamp,
    watched: &Watched,
) -> Vec<PresenceInterval> {
    let mut spans = spans.to_vec();
    spans.sort_by_key(|s| (s.time.start, s.time.end));
    let mut out: Vec<PresenceInterval> = Vec::with_capacity(spans.len());
    // T-940: the reports of sources still followed live, per interval: `(t_end, silence)`.
    let mut followed: Vec<Vec<(i64, i64)>> = Vec::with_capacity(spans.len());
    for s in spans {
        // The silence this row would have to cross to join the interval in hand. Negative when the
        // rows overlap, which is the "two sources describing the same minutes" case.
        let silence = out.last().map(|cur| {
            s.time
                .start
                .as_unix_nanos()
                .saturating_sub(cur.time.end.as_unix_nanos())
        });
        // Past one gap an end was detected; inside two, this row is the resumption that revokes it,
        // and the silence it crossed is carried on the interval so nothing counts it as air.
        let revoked = match silence {
            Some(s_ns) if s_ns > gap.as_nanos() && s_ns <= gap.revocable_nanos() => Some(s_ns),
            _ => None,
        };
        let joins = silence.is_some_and(|s_ns| s_ns <= gap.revocable_nanos());
        match out.last_mut() {
            Some(cur) if joins => {
                if let Some(ns) = revoked {
                    debug_assert!(ns > 0, "a revoked gap is a real silence");
                    cur.revoked.push(TimeRange::new(cur.time.end, s.time.start));
                }
                if s.time.end > cur.time.end {
                    cur.time.end = s.time.end;
                }
                cur.sources = cur.sources.saturating_add(1);
                cur.count = cur.count.saturating_add(s.count);
                // Spans are in start order, so the last source with a centre is the latest
                // measurement inside this interval.
                if s.f_center_hz.is_some() {
                    cur.f_center_hz = s.f_center_hz;
                }
            }
            _ => {
                out.push(PresenceInterval {
                    time: s.time,
                    sources: 1,
                    count: s.count,
                    f_center_hz: s.f_center_hz,
                    open: false,
                    revoked: Vec::new(),
                });
                followed.push(Vec::new());
            }
        }
        if let (Some(silence), Some(f)) = (s.live_silence_ns, followed.last_mut()) {
            f.push((s.time.end.as_unix_nanos(), silence.max(0)));
        }
    }
    // Only the latest interval can be open: every earlier one was followed by a row the receiver
    // measured, which is itself proof it was watching after that interval ended.
    if let (Some(last), Some(reports)) = (out.last_mut(), followed.last()) {
        let end = last.time.end.as_unix_nanos();
        let now_ns = now.as_unix_nanos();
        // Whether the silence below is **observed** time (watched on the band, or reported by the
        // tracker) rather than elapsed time. The idle gap `clamp(2 × revisit, 1 s, 60 s)` exists
        // to discount the time the receiver was *not* looking — "two missed revisits" of wall
        // clock. Observed silence has already had that time taken out, so comparing it with a
        // revisit-scaled gap would discount it twice: under a sweep with revisit P and dwell d, a
        // stopped burst would stay open for ~2·P²/d of wall time — minutes, up to the IQ ring's
        // horizon — where ADR-0019 §3 promises ~2·P (T-940 review). So observed silence is
        // judged against the continuous floor: one [`MIN_IDLE_GAP_S`] of *watching* and hearing
        // nothing, which a sweep accumulates in about two visits — the same "two missed revisits"
        // the gap encodes, now counted where it was measured. Elapsed silence keeps `gap`.
        let (silence, observed) = if reports.is_empty() {
            // Every source is closed: its end is final, and the silence after it is what the
            // receiver watched of the band since — never time it spent tuned elsewhere.
            (watched.observed_ns(end, now_ns), watched.is_recorded())
        } else {
            // A source is still followed live. The tracker is the end detector for it, and what it
            // has reported is all that is known: silence past its latest report is time the
            // detector has not yet analysed, which is not quiet. The **freshest** report decides —
            // the one reaching furthest (`t_end + silence`) — measured from the interval's own
            // latest end, so an older report left on a superseded row can neither hold the
            // interval open nor close it.
            let horizon = reports
                .iter()
                .map(|&(t_end, silence)| t_end.saturating_add(silence))
                .max()
                .unwrap_or(end);
            ((horizon.min(now_ns) - end).max(0), true)
        };
        let threshold = if observed { IdleGap::continuous() } else { gap };
        last.open = silence <= threshold.as_nanos();
    }
    out
}

/// Projects an emitter's intervals onto a view window: what ADR-0017 §2.1 makes the live list and
/// the History surface read.
///
/// `intervals` is [`intervals_from_spans`]' output (ordered, disjoint). Nothing here reads a
/// count: a row with 582,500 sightings and a row with 38 whose intervals are identical are
/// identically live and identically ranked.
///
/// `gap` sets the decay time constant of [`Presence::confidence`] (T-251) and nothing else; the
/// silence it decays over is measured to `window.end`, which **is** the caller's live edge — the
/// same edge `open` is derived against (docs/api.md, `/api/inventory` `presence` "Scope"). A live
/// row is confident by definition, so the two can never disagree.
pub fn presence_in_window(
    intervals: &[PresenceInterval],
    window: TimeRange,
    gap: IdleGap,
) -> Presence {
    let (t0, t1) = (window.start.as_unix_nanos(), window.end.as_unix_nanos());
    let mut n = 0u64;
    let mut on_air_ns = 0i128;
    let mut last: Option<PresenceInterval> = None;
    let mut live = false;
    for i in intervals.iter().filter(|i| i.time.overlaps(&window)) {
        n += 1;
        let lo = i.time.start.as_unix_nanos().max(t0);
        let hi = i.time.end.as_unix_nanos().min(t1);
        on_air_ns += i128::from(hi - lo).max(0);
        // A revoked end joined two spans across silence the receiver measured. The join is what the
        // user asked for; claiming that silence as air is not, so the in-window part of it comes
        // straight back off (T-413).
        for g in &i.revoked {
            let (glo, ghi) = (
                g.start.as_unix_nanos().max(lo),
                g.end.as_unix_nanos().min(hi),
            );
            on_air_ns -= i128::from(ghi - glo).max(0);
        }
        live |= i.open;
        if last.as_ref().is_none_or(|l| i.time.end >= l.time.end) {
            last = Some(i.clone());
        }
    }
    let liveness = match (n, live) {
        (0, _) => Liveness::Absent,
        (_, true) => Liveness::Live,
        (_, false) => Liveness::Ended,
    };
    let silence_s = last
        .as_ref()
        .map(|l| (t1.saturating_sub(l.time.end.as_unix_nanos())).max(0) as f64 / NS_PER_S);
    // Gated on `liveness`, not on a recomputed silence, so "confident" and "live" are the same
    // statement: an open interval is one the receiver has observed no absence for at all.
    let confidence = match liveness {
        Liveness::Absent => 0.0,
        Liveness::Live => 1.0,
        Liveness::Ended => confidence_after_silence(silence_s.unwrap_or(0.0), gap),
    };
    Presence {
        intervals: n,
        on_air_s: on_air_ns.max(0) as f64 / NS_PER_S,
        ended_t: (liveness == Liveness::Ended)
            .then(|| last.as_ref().expect("ended has an interval").time.end),
        last_interval: last,
        liveness,
        silence_s,
        confidence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(sec: f64) -> Timestamp {
        Timestamp::from_unix_nanos((sec * NS_PER_S) as i64)
    }

    fn span(a: f64, b: f64, count: u64) -> ObservationSpan {
        ObservationSpan {
            time: TimeRange::new(t(a), t(b)),
            count,
            f_center_hz: None,
            live_silence_ns: None,
        }
    }

    /// The gap comes from the revisit period, clamped by two tracker constants — never dialled in.
    #[test]
    fn the_idle_gap_is_derived_from_the_revisit_period() {
        // A full 1 MHz-6 GHz sweep revisits in ~0.75 s (hk-sim): two missed revisits = 1.5 s.
        assert_eq!(IdleGap::from_revisit_s(0.75).as_secs_f64(), 1.5);
        // A continuous dwell revisits every frame; the floor stops a millisecond gap splitting
        // what the tracker itself would have joined.
        assert_eq!(IdleGap::from_revisit_s(0.001).as_secs_f64(), MIN_IDLE_GAP_S);
        // Past the tracker's own idle timeout the track was already closed upstream.
        assert_eq!(IdleGap::from_revisit_s(600.0).as_secs_f64(), MAX_IDLE_GAP_S);
        // Unknown revisit claims no absence it cannot show.
        for unknown in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(IdleGap::from_revisit_s(unknown), IdleGap::conservative());
        }
        assert_eq!(IdleGap::conservative().as_secs_f64(), MAX_IDLE_GAP_S);
        assert_eq!(IdleGap::default(), IdleGap::conservative());
    }

    /// The gap can be **measured** off coverage instead of declared, and the three readings are
    /// distinguishable: continuously watched, combed, and not recorded at all (ADR-0019 §3).
    #[test]
    fn the_idle_gap_is_measured_from_the_coverage_of_the_band() {
        let w = TimeRange::new(t(0.0), t(100.0));
        let span = |a: f64, b: f64| TimeRange::new(t(a), t(b));

        // Nobody recorded whether the receiver looked: the one genuinely unknown case.
        assert_eq!(IdleGap::from_coverage(&[], w), IdleGap::conservative());

        // A single dwell covering the window, and two that touch: the receiver never looked away.
        assert_eq!(
            IdleGap::from_coverage(&[span(0.0, 100.0)], w),
            IdleGap::continuous()
        );
        assert_eq!(
            IdleGap::from_coverage(&[span(0.0, 50.0), span(50.0, 100.0)], w),
            IdleGap::continuous()
        );
        assert_eq!(IdleGap::continuous().as_secs_f64(), MIN_IDLE_GAP_S);
        // Which is exactly `from_revisit_s` of a frame-rate revisit — not a fourth rule.
        assert_eq!(IdleGap::from_revisit_s(0.02), IdleGap::continuous());

        // Combed coverage: a sweep back every 10 s reads as a 20 s gap — two missed revisits.
        let comb: Vec<TimeRange> = (0..9)
            .map(|i| span(f64::from(i) * 10.0, f64::from(i) * 10.0 + 1.0))
            .collect();
        assert_eq!(IdleGap::from_coverage(&comb, w).as_secs_f64(), 18.0);

        // Order-independent, and overlapping spans (two front ends on one band) close no gap.
        assert_eq!(
            IdleGap::from_coverage(&[span(40.0, 100.0), span(0.0, 60.0)], w),
            IdleGap::continuous()
        );

        // The window reaching past the coverage is not the receiver looking away mid-watch: only
        // silences *between* observations are revisit gaps.
        assert_eq!(
            IdleGap::from_coverage(&[span(40.0, 60.0)], w),
            IdleGap::continuous()
        );
        // A span wholly outside the window contributes nothing at all.
        assert_eq!(
            IdleGap::from_coverage(&[span(-500.0, -400.0)], w),
            IdleGap::conservative()
        );
    }

    /// Overlapping source rows are one span of air time, not two (docs/07 §2.11).
    #[test]
    fn overlapping_sources_normalise_into_one_interval() {
        let gap = IdleGap::from_revisit_s(0.5);
        let spans = [span(10.0, 20.0, 4), span(15.0, 25.0, 3)];
        let got = intervals_from_spans(&spans, gap, t(25.0));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].sources, 2);
        assert_eq!(got[0].time, TimeRange::new(t(10.0), t(25.0)));
        assert_eq!(got[0].count, 7, "count is carried, not recomputed");
    }

    /// A silence longer than the gap closes the interval; the next row opens a new one.
    #[test]
    fn a_silence_longer_than_the_idle_gap_closes_the_interval() {
        let gap = IdleGap::from_revisit_s(1.0); // 2 s
        let spans = [span(0.0, 10.0, 1), span(11.5, 12.0, 1), span(30.0, 31.0, 1)];
        let got = intervals_from_spans(&spans, gap, t(31.0));
        // 1.5 s of silence is inside the gap and bridges; 18 s closes.
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].time, TimeRange::new(t(0.0), t(12.0)));
        assert_eq!(got[1].time, TimeRange::new(t(30.0), t(31.0)));
        assert!(!got[0].open, "only the latest interval can be open");
        assert!(got[1].open);
    }

    /// **The revocation rule and its boundary** (T-413, ADR-0019 §6.1). The window is anchored on
    /// the end *event*, one gap past the measured end, so the join tolerance is `2 × gap` — and the
    /// boundary is asserted on both sides rather than assumed.
    #[test]
    fn a_resumption_within_one_further_gap_revokes_the_detected_end() {
        let gap = IdleGap::from_revisit_s(1.0); // 2 s
        let (g, g2) = (gap.as_secs_f64(), gap.revocable_nanos() as f64 / NS_PER_S);
        assert_eq!(
            g2,
            2.0 * g,
            "one gap to the end event, one more to confirm it"
        );
        // Never past the tracker's own idle timeout: there the discontinuity was measured upstream
        // and no revocation may rejoin it. The user's 72 s FM dropout stays two intervals.
        assert_eq!(
            IdleGap::conservative().revocable_nanos() as f64 / NS_PER_S,
            MAX_IDLE_GAP_S,
            "revocation may not lift the ceiling the gap is clamped to"
        );
        let fm = intervals_from_spans(
            &[span(0.0, 300.0, 1), span(372.0, 400.0, 1)],
            IdleGap::conservative(),
            t(400.0),
        );
        assert_eq!(fm.len(), 2, "a 72 s silence is two intervals: {fm:?}");

        // Inside the window: ONE interval, the end nulled, and the silence recorded on it.
        let joined = intervals_from_spans(
            &[span(0.0, 10.0, 1), span(10.0 + g2, 12.0, 1)],
            gap,
            t(12.0),
        );
        assert_eq!(joined.len(), 1, "the end was revoked: {joined:?}");
        assert_eq!(joined[0].revoked.len(), 1);
        assert_eq!(joined[0].revoked[0], TimeRange::new(t(10.0), t(10.0 + g2)));
        assert!(joined[0].open, "one interval, open at the live edge");

        // One nanosecond past it: the end stands, and the resumption is a new interval.
        let split = intervals_from_spans(
            &[span(0.0, 10.0, 1), span(10.0 + g2 + 1e-9, 12.0, 1)],
            gap,
            t(12.0),
        );
        assert_eq!(
            split.len(),
            2,
            "past the window the end is final: {split:?}"
        );
        assert!(split.iter().all(|i| i.revoked.is_empty()));

        // And a silence inside ONE gap never detected an end at all, so nothing is revoked.
        let never =
            intervals_from_spans(&[span(0.0, 10.0, 1), span(10.0 + g, 12.0, 1)], gap, t(12.0));
        assert_eq!(never.len(), 1);
        assert!(
            never[0].revoked.is_empty(),
            "no end fired here, so there is none to revoke"
        );
    }

    /// **The join is free, and time on air pays nothing for it.** Rejoining across measured silence
    /// is exactly the ADR-0017 hull pathology unless the silence comes back off the air time, so it
    /// does — on the interval, and on the window projection, clipped like everything else.
    #[test]
    fn a_revoked_gap_is_never_counted_as_time_on_air() {
        let gap = IdleGap::from_revisit_s(1.0); // 2 s, revocable to 4 s
        let got = intervals_from_spans(&[span(0.0, 10.0, 1), span(13.0, 20.0, 1)], gap, t(20.0));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].time, TimeRange::new(t(0.0), t(20.0)));
        assert_eq!(got[0].revoked_s(), 3.0);
        assert_eq!(got[0].duration_s(), 17.0, "20 s of extent, 17 s of air");

        let p = presence_in_window(&got, TimeRange::new(t(0.0), t(20.0)), gap);
        assert_eq!(p.on_air_s, 17.0, "the window projection subtracts it too");
        assert_eq!(p.intervals, 1);

        // Clipped, not all-or-nothing: a window covering half the revoked gap subtracts half.
        let half = presence_in_window(&got, TimeRange::new(t(11.5), t(20.0)), gap);
        assert_eq!(
            half.on_air_s, 7.0,
            "8.5 s of extent less 1.5 s of revoked silence"
        );
        // A window ending before the gap subtracts nothing.
        let before = presence_in_window(&got, TimeRange::new(t(0.0), t(10.0)), gap);
        assert_eq!(before.on_air_s, 10.0);

        // The pathology this closes, at the new tolerance: a 1.5 s cadence under a 1 s gap folds
        // into one interval, and still reads its true air time rather than the span it covers.
        let fast = IdleGap::continuous();
        let bursts: Vec<ObservationSpan> = (0..50)
            .map(|i| span(f64::from(i) * 1.5, f64::from(i) * 1.5 + 0.02, 1))
            .collect();
        let got = intervals_from_spans(&bursts, fast, t(73.52));
        assert_eq!(got.len(), 1, "1.5 s is inside the 2 s revocation window");
        let air: f64 = got.iter().map(PresenceInterval::duration_s).sum();
        assert!((air - 1.0).abs() < 1e-6, "1.0 s on air over 73.5 s: {air}");
    }

    /// A revoked end never delays closure: the interval still reads open for exactly one gap past
    /// its measured end, so the end detector's latency is what T-410 made it.
    #[test]
    fn revocation_does_not_widen_the_window_an_interval_reads_open_over() {
        let gap = IdleGap::from_revisit_s(1.0); // 2 s
        let one = intervals_from_spans(&[span(0.0, 10.0, 1)], gap, t(12.0));
        assert!(one[0].open, "still inside one gap of the measured end");
        let past = intervals_from_spans(&[span(0.0, 10.0, 1)], gap, t(12.5));
        assert!(
            !past[0].open,
            "closed at one gap, not two - the end fires, and only then is it revocable"
        );
    }

    /// The ISM consequence, stated as a test: a chatty burst source is one interval per burst.
    #[test]
    fn a_chatty_burst_source_reads_as_one_interval_per_burst() {
        let gap = IdleGap::from_revisit_s(0.75);
        let bursts: Vec<ObservationSpan> = (0..50)
            .map(|i| span(f64::from(i) * 30.0, f64::from(i) * 30.0 + 0.02, 1))
            .collect();
        let got = intervals_from_spans(&bursts, gap, t(1470.02));
        assert_eq!(got.len(), 50, "fifty events, deliberately - not one span");
        // T-413: the revocation window has to be checked against this, not assumed clear of it. A
        // 30 s cadence sits ten times outside a 3 s window, so no end here is ever revoked.
        assert!(
            (gap.revocable_nanos() as f64 / NS_PER_S) < 30.0,
            "a revocation window this wide would swallow the ISM cadence"
        );
        assert!(got.iter().all(|i| i.revoked.is_empty()));
        let on_air: f64 = got.iter().map(PresenceInterval::duration_s).sum();
        assert!(
            (on_air - 1.0).abs() < 1e-6,
            "1.0 s on air over 24.5 min: {on_air}"
        );
    }

    /// Liveness and on-air time never read `count` (ADR-0017 §5).
    #[test]
    fn liveness_and_on_air_never_read_the_sighting_count() {
        let gap = IdleGap::from_revisit_s(1.0);
        let quiet = intervals_from_spans(&[span(0.0, 10.0, 38)], gap, t(600.0));
        let chatty = intervals_from_spans(&[span(0.0, 10.0, 582_500)], gap, t(600.0));
        let window = TimeRange::new(t(0.0), t(600.0));
        let (a, b) = (
            presence_in_window(&quiet, window, gap),
            presence_in_window(&chatty, window, gap),
        );
        assert_eq!(a.liveness, Liveness::Ended);
        assert_eq!(a.liveness, b.liveness);
        assert_eq!(a.on_air_s, b.on_air_s);
        assert_eq!(a.intervals, b.intervals);
        assert_eq!(a.ended_t, Some(t(10.0)));
    }

    /// A window the signal never reached is `absent`, however large its lifetime count.
    #[test]
    fn a_window_past_the_last_interval_reads_absent() {
        let gap = IdleGap::from_revisit_s(1.0);
        let got = intervals_from_spans(&[span(0.0, 10.0, 582_500)], gap, t(600.0));
        let p = presence_in_window(&got, TimeRange::new(t(100.0), t(600.0)), gap);
        assert_eq!(p.liveness, Liveness::Absent);
        assert_eq!(p.intervals, 0);
        assert_eq!(p.on_air_s, 0.0);
        assert_eq!(p.ended_t, None);
        assert_eq!(p.silence_s, None);
        assert_eq!(p.confidence, 0.0, "no interval here, no hypothesis to rank");
    }

    /// On-air time is clipped to the window, so scrubbing changes it honestly.
    #[test]
    fn on_air_time_is_clipped_to_the_window() {
        let gap = IdleGap::from_revisit_s(1.0);
        let got = intervals_from_spans(&[span(0.0, 100.0, 1)], gap, t(100.0));
        let p = presence_in_window(&got, TimeRange::new(t(90.0), t(140.0)), gap);
        assert_eq!(p.intervals, 1);
        assert_eq!(p.on_air_s, 10.0);
        assert_eq!(p.liveness, Liveness::Live, "open at the live edge");
        assert_eq!(p.ended_t, None);
    }

    // -----------------------------------------------------------------------------------------
    // Decay (T-251, ADR-0017 TM-6)
    // -----------------------------------------------------------------------------------------

    /// The law and its time constant: flat at 1 while the receiver has observed no absence at
    /// all, then exactly one `1/e` per further idle gap of observed absence. The constant is the
    /// gap itself, so changing the revisit period moves the decay and nothing else does.
    #[test]
    fn confidence_decays_one_e_fold_per_idle_gap_of_observed_silence() {
        let gap = IdleGap::from_revisit_s(1.0); // 2 s
        let g = gap.as_secs_f64();
        // Shorter than the gap is not evidence of absence: the receiver was not listening.
        for quiet in [0.0, 0.5 * g, g] {
            assert_eq!(confidence_after_silence(quiet, gap), 1.0, "silence {quiet}");
        }
        for k in 1..=4 {
            let c = confidence_after_silence(g * (1.0 + f64::from(k)), gap);
            assert!(
                (c - (-f64::from(k)).exp()).abs() < 1e-12,
                "{k} gaps of silence gave {c}"
            );
        }
        // A missing measurement never manufactures decay.
        assert_eq!(confidence_after_silence(f64::NAN, gap), 1.0);
        // The constant *is* the gap: a receiver that revisits half as often has observed half as
        // much absence in the same silence, and is correspondingly more confident.
        let slow = IdleGap::from_revisit_s(2.0);
        assert_eq!(slow.as_secs_f64(), 2.0 * g);
        assert!(confidence_after_silence(5.0 * g, slow) > confidence_after_silence(5.0 * g, gap));
    }

    /// The re-check horizon is not chosen: it is where the law reaches the scheduler's own
    /// dwell-share floor, so both numbers in it were already measured.
    #[test]
    fn the_recheck_horizon_is_where_confidence_reaches_the_dwell_share_floor() {
        for revisit in [0.5, 1.0, 30.0] {
            let gap = IdleGap::from_revisit_s(revisit);
            let h = recheck_horizon_s(gap);
            assert!(
                (confidence_after_silence(h, gap) - NEGLIGIBLE_CONFIDENCE).abs() < 1e-12,
                "horizon {h} s under a {} s gap",
                gap.as_secs_f64()
            );
            // Always the same multiple of the gap — nothing else sets it.
            assert!((h / gap.as_secs_f64() - 3.9957).abs() < 1e-3, "{h}");
        }
    }

    /// **The case window-scoping cannot answer** (ADR-0017 §5). Two rows with the *same* in-window
    /// on-air time: one transmitting now, one that stopped early inside the window. `on_air_s`
    /// ranks them equal; confidence separates them — and the stopped row stays listed, with its
    /// interval and its box intact.
    #[test]
    fn a_candidate_that_stopped_inside_the_window_ranks_below_one_transmitting_now() {
        let gap = IdleGap::from_revisit_s(1.0); // 2 s
        let window = TimeRange::new(t(0.0), t(20.0));
        let stopped = intervals_from_spans(&[span(0.0, 5.0, 1)], gap, t(20.0));
        let live = intervals_from_spans(&[span(15.0, 20.0, 1)], gap, t(20.0));
        let (a, b) = (
            presence_in_window(&stopped, window, gap),
            presence_in_window(&live, window, gap),
        );
        assert_eq!(
            a.on_air_s, b.on_air_s,
            "5 s each: on_air_s cannot tell them apart"
        );
        assert_eq!((a.liveness, b.liveness), (Liveness::Ended, Liveness::Live));
        assert_eq!((a.silence_s, b.silence_s), (Some(15.0), Some(0.0)));
        assert_eq!(b.confidence, 1.0, "live is confident by definition");
        assert!(a.confidence < b.confidence, "{a:?} vs {b:?}");
        // Ranked lower, never expired: it happened, and its box is on screen.
        assert_eq!(a.intervals, 1);
        assert!(a.last_interval.is_some());
        assert_eq!(a.ended_t, Some(t(5.0)));
    }

    /// Reversible with no revival path at all, because nothing was stored: the returning signal's
    /// new interval on the same emitter restores confidence, and both intervals are kept.
    #[test]
    fn a_returning_signal_restores_confidence_and_nothing_is_deleted() {
        let gap = IdleGap::from_revisit_s(1.0); // 2 s
        let stopped = intervals_from_spans(&[span(0.0, 5.0, 1)], gap, t(40.0));
        let faded = presence_in_window(&stopped, TimeRange::new(t(0.0), t(40.0)), gap);
        assert_eq!(faded.liveness, Liveness::Ended);
        assert!(faded.confidence < 0.01, "35 s silent: {faded:?}");

        let returned =
            intervals_from_spans(&[span(0.0, 5.0, 1), span(58.0, 60.0, 1)], gap, t(60.0));
        let back = presence_in_window(&returned, TimeRange::new(t(0.0), t(60.0)), gap);
        assert_eq!(back.liveness, Liveness::Live);
        assert_eq!(back.confidence, 1.0);
        assert_eq!(
            back.intervals, 2,
            "decay deletes nothing; both events stand"
        );
    }

    /// Confidence reads interval boundaries only — never the sighting count (ADR-0017 §5).
    #[test]
    fn confidence_never_reads_the_sighting_count() {
        let gap = IdleGap::from_revisit_s(1.0);
        let window = TimeRange::new(t(0.0), t(600.0));
        let quiet = intervals_from_spans(&[span(0.0, 10.0, 38)], gap, t(600.0));
        let chatty = intervals_from_spans(&[span(0.0, 10.0, 582_500)], gap, t(600.0));
        assert_eq!(
            presence_in_window(&quiet, window, gap).confidence,
            presence_in_window(&chatty, window, gap).confidence
        );
    }

    fn followed(a: f64, b: f64, silence_s: f64) -> ObservationSpan {
        ObservationSpan {
            live_silence_ns: Some((silence_s * NS_PER_S) as i64),
            ..span(a, b, 1)
        }
    }

    /// T-940: a track the pipeline is still following reads open by **what the tracker reported**,
    /// not by how long ago its row was last written. Staging's defect in one assertion: a station
    /// whose row was refreshed 4 s ago under a 1 s gap read `ended` while it was on the air.
    #[test]
    fn a_followed_track_is_open_until_the_tracker_reports_an_observed_silence() {
        let gap = IdleGap::continuous();
        let now = t(104.0);
        // On the air, last reported 4 s ago with no silence observed: the 4 s is time the detector
        // has not reported yet, never quiet.
        let on = intervals_observed(
            &[followed(0.0, 100.0, 0.0)],
            gap,
            now,
            &Watched::unrecorded(),
        );
        assert!(on[0].open, "{on:?}");
        // The pre-T-940 reading of the same row, for the record: the wall clock closed it.
        assert!(!intervals_from_spans(&[span(0.0, 100.0, 1)], gap, now)[0].open);
        // The tracker observed 0.8 s of silence: inside the gap, still open.
        let quiet = intervals_observed(
            &[followed(0.0, 100.0, 0.8)],
            gap,
            now,
            &Watched::unrecorded(),
        );
        assert!(quiet[0].open);
        // It observed 1.5 s: the end is detected, whatever the clock says.
        let gone = intervals_observed(
            &[followed(0.0, 100.0, 1.5)],
            gap,
            now,
            &Watched::unrecorded(),
        );
        assert!(!gone[0].open);
        // The measured end is the measurement, not the report: the interval is not stretched.
        assert_eq!(gone[0].time, TimeRange::new(t(0.0), t(100.0)));
        // A report can never claim silence past the reading's own edge: scrubbed back to 100.5 s,
        // a later report of 3 s of silence has only 0.5 s of it inside the view.
        let past = intervals_observed(
            &[followed(0.0, 100.0, 3.0)],
            gap,
            t(100.5),
            &Watched::unrecorded(),
        );
        assert!(past[0].open);
    }

    /// T-940: the freshest report decides. A stale report left on a superseded row cannot hold an
    /// interval open once a fresher one has observed the silence.
    #[test]
    fn the_freshest_report_decides() {
        let gap = IdleGap::continuous();
        let spans = [followed(0.0, 50.0, 0.0), followed(10.0, 99.0, 2.0)];
        let iv = intervals_observed(&spans, gap, t(130.0), &Watched::unrecorded());
        assert!(!iv[0].open, "{iv:?}");
        let spans = [followed(0.0, 50.0, 0.0), followed(10.0, 99.0, 0.3)];
        let iv = intervals_observed(&spans, gap, t(130.0), &Watched::unrecorded());
        assert!(iv[0].open, "{iv:?}");
    }

    /// T-940: a closed decode row beside a followed source does not end the interval, and a closed
    /// row that ends *later* than the tracker's measured end is not charged the tracker's silence
    /// twice.
    #[test]
    fn one_followed_source_keeps_its_interval_open() {
        let gap = IdleGap::continuous();
        let spans = [
            span(0.0, 50.0, 1),
            followed(10.0, 99.0, 0.0),
            span(98.0, 99.5, 1),
        ];
        let iv = intervals_observed(&spans, gap, t(130.0), &Watched::unrecorded());
        assert_eq!(iv.len(), 1);
        assert!(iv[0].open, "{iv:?}");
        assert_eq!(iv[0].time.end, t(99.5));
    }

    /// T-940: once every source is closed, the silence that ends an interval is the part of it the
    /// receiver was **watching the band**. A retune away is not the station going quiet.
    #[test]
    fn a_closed_source_is_ended_only_by_silence_the_receiver_watched() {
        let gap = IdleGap::continuous();
        let rows = [span(0.0, 100.0, 1)];
        // Watched up to 100.4 s, then tuned elsewhere until now: 0.4 s observed, still ongoing.
        let away = Watched::recorded(t(0.0), &[TimeRange::new(t(0.0), t(100.4))]);
        assert!(intervals_observed(&rows, gap, t(400.0), &away)[0].open);
        // Back on the band from 399 s: another second of watching past the 0.4 s ends it, because
        // the station would have been seen.
        let back = Watched::recorded(
            t(0.0),
            &[
                TimeRange::new(t(0.0), t(100.4)),
                TimeRange::new(t(399.0), t(400.0)),
            ],
        );
        assert!(!intervals_observed(&rows, gap, t(400.0), &back)[0].open);
        // A record that the receiver never looked at the band is not "unrecorded": no silence was
        // observed at all.
        let never = Watched::recorded(t(0.0), &[]);
        assert!(intervals_observed(&rows, gap, t(400.0), &never)[0].open);
        // But a record that begins long after the row ended cannot vouch for the time before it:
        // that stretch is elapsed time, and a station last measured at 100 s is not "still on the
        // air" at 400 s because the journal only remembers from 350 s.
        let forgot = Watched::recorded(t(350.0), &[]);
        assert!(!intervals_observed(&rows, gap, t(400.0), &forgot)[0].open);
        // And unrecorded keeps the elapsed-time reading, unchanged.
        assert!(!intervals_observed(&rows, gap, t(400.0), &Watched::unrecorded())[0].open);
    }

    /// T-940 review: under a **sweep** the gap is `2 × revisit` of *wall* time, and observed
    /// silence has already had the unwatched time taken out — judging one against the other
    /// discounts twice. Revisit 10 s, dwell 0.5 s: the gap is 20 s, and watched silence grows
    /// 0.5 s per visit, so the old reading held a stopped burst open for ~380 s. Observed
    /// silence is judged against the continuous floor instead: two visits of hearing nothing.
    #[test]
    fn a_swept_band_closes_after_two_silent_visits_not_2p_squared_over_d() {
        let gap = IdleGap::from_revisit_s(10.0);
        assert_eq!(gap.as_secs_f64(), 20.0);
        // Dwells of 0.5 s every 10 s from 0 s; the burst lived in the first one.
        let dwells: Vec<TimeRange> = (0..60)
            .map(|k| {
                let a = 10.0 * k as f64;
                TimeRange::new(t(a), t(a + 0.5))
            })
            .collect();
        let w = Watched::recorded(t(0.0), &dwells);
        let rows = [span(0.1, 0.3, 1)];
        // One further silent visit (0.2 + 0.5 s watched): not yet an end.
        assert!(intervals_observed(&rows, gap, t(10.6), &w)[0].open);
        // Two further silent visits: ended, ~20 s after the burst — ADR-0019 §3's 2·P.
        assert!(!intervals_observed(&rows, gap, t(20.6), &w)[0].open);
        // The defect: nowhere near 380 s.
        assert!(!intervals_observed(&rows, gap, t(60.0), &w)[0].open);
        // A still-followed track under the same sweep: its report is observed silence too.
        let f = [followed(0.1, 0.3, 1.2)];
        assert!(!intervals_observed(&f, gap, t(30.0), &w)[0].open);
        let f = [followed(0.1, 0.3, 0.7)];
        assert!(intervals_observed(&f, gap, t(30.0), &w)[0].open);
        // Unrecorded coverage is elapsed time and keeps the measured gap, unchanged.
        let none = Watched::unrecorded();
        assert!(intervals_observed(&rows, gap, t(15.0), &none)[0].open);
        assert!(!intervals_observed(&rows, gap, t(21.0), &none)[0].open);
    }

    /// T-940: observed time is the measure of the window's intersection with the coalesced spans,
    /// in any order and however they overlap.
    #[test]
    fn watched_time_is_the_intersection_with_coalesced_spans() {
        let r = |a: f64, b: f64| TimeRange::new(t(a), t(b));
        let w = Watched::recorded(
            t(0.0),
            &[r(5.0, 8.0), r(0.0, 2.0), r(1.0, 3.0), r(7.0, 7.5)],
        );
        let s = |a: f64, b: f64| w.observed_ns(t(a).as_unix_nanos(), t(b).as_unix_nanos());
        assert_eq!(s(0.0, 10.0), (6.0 * NS_PER_S) as i64);
        assert_eq!(s(2.5, 6.0), (1.5 * NS_PER_S) as i64);
        assert_eq!(s(3.0, 5.0), 0);
        assert_eq!(s(6.0, 4.0), 0, "a reversed range observed nothing");
        // Before the record begins, time is elapsed time: unknown, never unobserved.
        let late = Watched::recorded(t(5.0), &[r(5.0, 8.0)]);
        let l = |a: f64, b: f64| late.observed_ns(t(a).as_unix_nanos(), t(b).as_unix_nanos());
        assert_eq!(l(0.0, 10.0), (8.0 * NS_PER_S) as i64);
    }
}
