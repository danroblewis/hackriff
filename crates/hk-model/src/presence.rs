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
//! **What this does to a burst source, stated plainly.** In the 902–928 MHz ISM playground
//! (T-254/T-255) a chatty sensor firing a 20 ms burst every 30 s is **fifty intervals, not one** —
//! 30 s of silence is far longer than any revisit-derived gap. That is the intended reading:
//! "50 events, 1.0 s on air" is honest, each burst's box is 20 ms tall, and bursts look like
//! bursts. Fifty intervals are cheap precisely because they are fifty intervals on **one** emitter,
//! not fifty rows. The fact that the sensor is *periodic* is said by `EmissionFeatures`
//! (period, duty cycle, burst length — ADR-0016 §5), which is where that belongs; it is not said by
//! smearing fifty transmissions into one span of mostly silence.
//!
//! **Why per-band is worse.** A per-band idle gap has no measurement behind it: it is a number
//! chosen until one screen looks right. It would make the same sensor read as one interval or
//! fifty depending only on which band it sits in, so moving a device from 433 MHz to 915 MHz would
//! change its event count with no change in the air. And a per-band gap wide enough to make an ISM
//! burst source "one interval" reintroduces exactly the pathology ADR-0017 exists to remove — a
//! span that is 99.97 % silence, presented as time on air — one level further down, where it is
//! harder to see.
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

    /// The gap in nanoseconds.
    pub const fn as_nanos(self) -> i64 {
        self.0
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
}

/// A maximal span during which one emitter was continuously on the air, within the detector's
/// ability to tell continuity from gaps (docs/07 §2.27).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PresenceInterval {
    /// The time extent. **This**, not the emitter's hull, is how long the signal was on air.
    pub time: TimeRange,
    /// Source rows normalised into this interval (≥ 1).
    pub sources: u32,
    /// Sightings summed over those rows: a **History total**. Never read by liveness or ranking,
    /// and — because overlapping sources describing the same minutes each keep their own count —
    /// not a count of distinct transmissions.
    pub count: u64,
    /// The latest centre measured within this interval, Hz, when a source recorded one.
    pub f_center_hz: Option<f64>,
    /// Derived, never stored: `now − t_end ≤ idle_gap`. Only the latest interval can be open.
    pub open: bool,
}

impl PresenceInterval {
    /// Time on air in this interval, s.
    pub fn duration_s(&self) -> f64 {
        self.time.duration_ns().max(0) as f64 / NS_PER_S
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
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Presence {
    /// Intervals intersecting the window.
    pub intervals: u64,
    /// Time on air **inside** the window, s: Σ of each interval's intersection with it. This is
    /// the honest replacement for ranking by lifetime `count`.
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

/// Normalises raw source rows into the emitter's ordered set of **disjoint** presence intervals
/// (module docs): overlapping rows and rows separated by at most `gap` fold together; a longer
/// silence closes the interval and the next row opens a new one. `now` is the live edge, and
/// decides only which interval reads as open.
///
/// Reads `time` only. `count` is carried through untouched.
pub fn intervals_from_spans(
    spans: &[ObservationSpan],
    gap: IdleGap,
    now: Timestamp,
) -> Vec<PresenceInterval> {
    let mut spans = spans.to_vec();
    spans.sort_by_key(|s| (s.time.start, s.time.end));
    let mut out: Vec<PresenceInterval> = Vec::with_capacity(spans.len());
    for s in spans {
        let joins = out.last().is_some_and(|cur| {
            s.time.start.as_unix_nanos()
                <= cur.time.end.as_unix_nanos().saturating_add(gap.as_nanos())
        });
        match out.last_mut() {
            Some(cur) if joins => {
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
            _ => out.push(PresenceInterval {
                time: s.time,
                sources: 1,
                count: s.count,
                f_center_hz: s.f_center_hz,
                open: false,
            }),
        }
    }
    for i in &mut out {
        i.open = now
            .as_unix_nanos()
            .saturating_sub(i.time.end.as_unix_nanos())
            <= gap.as_nanos();
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
        live |= i.open;
        if last.is_none_or(|l| i.time.end >= l.time.end) {
            last = Some(*i);
        }
    }
    let liveness = match (n, live) {
        (0, _) => Liveness::Absent,
        (_, true) => Liveness::Live,
        (_, false) => Liveness::Ended,
    };
    let silence_s =
        last.map(|l| (t1.saturating_sub(l.time.end.as_unix_nanos())).max(0) as f64 / NS_PER_S);
    // Gated on `liveness`, not on a recomputed silence, so "confident" and "live" are the same
    // statement: an open interval is one the receiver has observed no absence for at all.
    let confidence = match liveness {
        Liveness::Absent => 0.0,
        Liveness::Live => 1.0,
        Liveness::Ended => confidence_after_silence(silence_s.unwrap_or(0.0), gap),
    };
    Presence {
        intervals: n,
        on_air_s: on_air_ns as f64 / NS_PER_S,
        last_interval: last,
        liveness,
        ended_t: (liveness == Liveness::Ended)
            .then(|| last.expect("ended has an interval").time.end),
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

    /// The ISM consequence, stated as a test: a chatty burst source is one interval per burst.
    #[test]
    fn a_chatty_burst_source_reads_as_one_interval_per_burst() {
        let gap = IdleGap::from_revisit_s(0.75);
        let bursts: Vec<ObservationSpan> = (0..50)
            .map(|i| span(f64::from(i) * 30.0, f64::from(i) * 30.0 + 0.02, 1))
            .collect();
        let got = intervals_from_spans(&bursts, gap, t(1470.02));
        assert_eq!(got.len(), 50, "fifty events, deliberately - not one span");
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
}
