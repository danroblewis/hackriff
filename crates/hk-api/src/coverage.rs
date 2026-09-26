//! `GET /api/coverage` — the coverage map: which front end actually sampled which frequency, and
//! therefore which cells are grey because nothing ever looked (T-368).
//!
//! # The rule this serves
//!
//! The user's invariant (CLAUDE.md, "Time, the waterfall, and the live view"):
//!
//! > **The waterfall shows the data that exists for the selected (time, frequency); grey means
//! > genuinely unobserved.** … This requires the backend to keep a **coverage map derived from the
//! > SDR configuration/tune history** — for each interval, which centre/span/rate (and which
//! > device) was active — so observed-vs-unobserved is computed from what was actually sampled, and
//! > the frequency navigator's survey view is built from that same coverage.
//!
//! T-341 stopped the view *claiming* detail the front end never captured. This is the other half:
//! the view may *show* what the front end did capture, and must grey only what it did not. Between
//! them sits the failure this route exists to prevent — **painting never-observed spectrum as
//! quiet**, which invents an absence-of-signal finding out of an absence of measurement.
//!
//! # Three states on the wire, and the third cannot be spelled as the second
//!
//! Each cell is one of:
//!
//! ```jsonc
//! { "state": "unobserved" }                                  // 3: nothing ever looked. Grey.
//! { "state": "observed", "duty": 0.5, "observed_s": 30.0, … } // 1 or 2: the radio was here.
//! ```
//!
//! An unobserved cell carries **no measurement keys at all** — not `null` ones. That is stronger
//! than a nullable number, because there is no field a client can read as zero: the absence is
//! structural. It comes straight from [`hk_store::Coverage`], whose only observed-constructor refuses to
//! mint an observation out of nothing, so state 3 is unrepresentable as state 2 in the type as well
//! as on the wire.
//!
//! Whether an observed cell was *quiet* is a different question with a different answer: `shade`,
//! the level the spectrum history holds there. `shade: null` on an **observed** cell means the
//! history kept no level for it — sampled, level not retained — and a client must draw that
//! differently from grey. **Grey is `state == "unobserved"` and nothing else.**
//!
//! # The time axis (T-423, `docs/16` §7 step 2)
//!
//! T-368 served a **column**: one row over the whole window, answering *"was this band sampled
//! anywhere in it"*. A view drawing a waterfall needs a **cell** — *"was it sampled **then**"* —
//! and the difference is not cosmetic: a cell in a band the radio demonstrably watched, at an
//! instant it was tuned somewhere else, came back `observed` and read to the survey bar (T-405)
//! and the time navigator (T-411) as *"sampled, level not retained"* rather than grey.
//!
//! `rows` is that axis. T-421 already built the whole computation in
//! [`hk_store::coverage`] — [`hk_store::coverage::grid_over`] and its `by_device_over` /
//! `union_grid_over` wrappers lay `nt × nf` cells on the window and put each through the same
//! [`Coverage::of`], with the **row's own** extent as its window. This route only asks for it and
//! serves it; there is no second rasteriser here. `rows = 1` (the default) is T-368's answer
//! unchanged, bit for bit.
//!
//! # The fourth state: `"unknown"` — *we no longer know whether we looked*
//!
//! `docs/16` §5.4. Two horizons cross. The spectrum-history pyramid has **no age limit** (a rolling
//! byte budget); the IQ ring holds minutes and the observation log expires at 180 days (T-406 —
//! `docs/16` §5.4 raised it from 30 so the coverage record outlives the pyramid it explains). So a
//! cell can
//! hold a measurement whose coverage record is gone — and, more commonly, a *row* can lie before any
//! surviving record at all. `unobserved` claims *nothing looked*, which is a claim no record
//! supports there. Painting it grey spells "never looked" for spectrum whose records were merely
//! discarded: [`Coverage::of`]'s sin, one horizon out.
//!
//! So the wire has a fourth value, `"state": "unknown"`, carrying **no measurement keys** by the
//! same structural rule as `unobserved`, and `horizon` beside it saying where the boundary is and
//! how many rows fell before it — so a client can check the claim rather than take it. The
//! vocabulary is the house one: `bias_tee: "unknown"` ≠ `"off"`, [`Device::Unknown`] ≠ a wildcard —
//! **nothing said is never permissive.**
//!
//! It is a **wire** state and deliberately not a third [`Coverage`] variant. Three reasons, and
//! `docs/16` §5.4 asks for them to be stated:
//!
//! 1. **The fold has no input for it.** [`Coverage`] is computed from spans; a span does not carry
//!    the horizon that produced it, and [`Coverage::of`] — the one construction site, whose whole
//!    job is refusing to mint an observation out of nothing — would have to be handed a horizon it
//!    cannot check to decide a question it was never asked. Only the caller that *read* the
//!    records knows how far back they reach. That caller is this module.
//! 2. **It is a property of a row, not of a cell.** A discarded record takes every frequency with
//!    it, so the state is *"these rows are before our memory"*. A per-cell variant would let one
//!    grid say "unknown at 100 MHz, unobserved at 101 MHz" in the *same row*, which is
//!    unrepresentable in reality. [`hk_store::CoverageGrid::unknown_rows_before`] is the right
//!    shape, and it already exists (T-421).
//! 3. **A variant is a break for every consumer, to say something none of them could compute.**
//!    Derived here it is one line, checkable against `horizon`, and `Coverage`'s deliberate
//!    two-variant design — which is what makes state 3 unrepresentable as state 2 — stays intact.
//!
//! An **observed** cell is never re-labelled: a surviving measurement is itself proof we looked, so
//! it stays `observed` past the horizon (`docs/16` §5.4, explicitly). Only `unobserved` can become
//! `unknown`, and only on a row wholly before the oldest surviving record.
//!
//! **And only where something was recorded and lost (T-507).** A row wholly before this server
//! began recording at all is `unobserved`: nothing looked, and a server that has never recorded has
//! forgotten nothing. Serving every pre-record row as `unknown` painted a freshly started server's
//! whole past in the fourth state — the magenta wall. `Evidence::unknown_rows` states the three
//! cases; `horizon.recording_began_s` and `horizon.forgotten` make each checkable on the wire.
//!
//! # Where the map comes from: provenance already written
//!
//! Nothing new is journalled for this. Two records already say "for each interval, which
//! centre/span/rate was active", and one of them also says which device:
//!
//! | Source | Interval | Centre/span/rate | Device | Horizon |
//! |---|---|---|---|---|
//! | IQ ring journal (`/api/iqbuffer` segments, ADR-0014) | yes | yes | **yes** (`device_id`) | the ring's retention |
//! | observation log (`DwellRecord`/`SweepRecord`, ADR-0012 §1) | yes | yes (`ObservedWindow`) | **yes** (`device_id`, T-378) | 180 days / 2 GiB, whichever binds (T-406) |
//!
//! The ring journal opens a new segment on **every** provenance change, so retunes are segment
//! boundaries by construction — it is already a tune history. **T-378** put the same `device_id` on
//! the observation log's records, so the long horizon is device-local too and coverage over the
//! retention window answers "did *this* front end look here", not merely "did anything" — which,
//! with two SDRs, is the whole question. A record that names no device (every record written before
//! T-378, or a source that states no identity) stays [`Device::Unknown`]: evidence that *something*
//! looked, never evidence that a *particular* front end did, and never read as whichever radio is
//! running now. `sources` in the response reports, per record kind, how many spans actually named
//! one.
//!
//! | Method | Path | Query | Answers |
//! |---|---|---|---|
//! | GET | `/api/coverage` | `f_lo`&`f_hi` (Hz, required), `cells`? (1…4096, default 256), `rows`? (1…4096, default 1), `t0`&`t1`? (Unix s; default the capture window) | `{region, window, grid, devices, any, horizon, sources, resolution}` |

use hk_model::{
    EmitterId, FreqRange, IdleGap, Presence, PresenceInterval, RepoError, Repository, TimeRange,
    Timestamp, Watched,
};
use hk_store::coverage::{
    Coverage, CoverageGrid, CoverageSpan, Device, MAX_COVERAGE_CELLS, MAX_COVERAGE_ROWS,
};
use hk_store::observation::{MAX_RECORD_LIMIT, ObservationStore, RecordQuery};
use serde_json::{Value, json};

use crate::http::ApiState;
use crate::query::{ApiError, Params, count, parse_freq_only};

/// Frequency cells the survey strip is drawn in when the caller names none.
pub const DEFAULT_CELLS: usize = 256;

/// Time rows when the caller names none: one row over the whole window — T-368's column answer,
/// unchanged, so a caller that never heard of the time axis gets exactly what it got before.
pub const DEFAULT_ROWS: usize = 1;

/// Segments read from the IQ ring journal for one answer.
const RING_SEGMENTS: usize = 10_000;

/// The whole tunable spectrum, for a caller asking "which bands did the receiver watch at all"
/// rather than about one band. Wider than any front end, so it filters nothing out.
const ALL_FREQ: FreqRange = FreqRange::new(f64::NEG_INFINITY, f64::INFINITY);

/// What the receiver actually watched over one request's window: the IQ ring's tune history,
/// read **once per request** and then asked per band (T-410, ADR-0019 §3).
///
/// # Why presence needs this
///
/// An interval closes after [`hk_model::IdleGap`] of **observed** silence, and that gap is also the
/// end detector's latency — the time a box runs to the live edge before capping. hk-api used to
/// pass `IdleGap::conservative()` (60 s) everywhere, on the grounds that it "does not know the
/// scheduler's revisit period". But the receiver's revisit period is not a scheduler declaration;
/// it is a **measurement**, and the ring journal is where it is recorded: one segment per retune,
/// each naming its window, centre and rate. A dwell on one centre is one long segment over the
/// band — a revisit period of one STFT frame, not "unknown".
///
/// # Why it is asked per band and not per request
///
/// A sweep's segments are contiguous in *time* and disjoint in *frequency*. Folding them without
/// regard to frequency would read as "the receiver never looked away", which is true of the
/// receiver and false of every individual band. So the spans are kept whole and
/// [`Self::idle_gap`] selects the ones that overlap the band being asked about, exactly as
/// [`ring_spans`] does for the coverage grid: coverage of somewhere else is not coverage of here.
#[derive(Clone, Debug, Default)]
pub struct ObservedCoverage {
    spans: Vec<CoverageSpan>,
}

impl ObservedCoverage {
    /// Reads the ring's tune history over `window`. Empty when the server has no ring — which
    /// yields [`hk_model::IdleGap::conservative`] for every band, the unchanged behaviour for a
    /// replay store or a history-only server.
    pub fn of(state: &ApiState, window: TimeRange) -> Self {
        Self {
            spans: ring_spans(state, ALL_FREQ, window),
        }
    }

    /// The idle gap for `freq` over `window`: how often this receiver looked at **that band**.
    ///
    /// See [`hk_model::IdleGap::from_coverage`] for the rule and the three readings it
    /// distinguishes (continuously watched, combed, never recorded).
    pub fn idle_gap(&self, freq: FreqRange, window: TimeRange) -> hk_model::IdleGap {
        hk_model::IdleGap::from_coverage(&self.band_spans(freq), window)
    }

    /// The spans the receiver observed `freq` in: coverage of somewhere else is not coverage of here.
    fn band_spans(&self, freq: FreqRange) -> Vec<TimeRange> {
        self.spans
            .iter()
            .filter(|s| s.freq.overlaps(&freq))
            .map(|s| s.time)
            .collect()
    }

    /// When the receiver watched `freq` (T-940): the same tune history the gap is measured from,
    /// asked the second question — **which part of a silence was observed**. The record begins at
    /// the oldest segment the ring still holds, *on any band*: before it nobody can say whether the
    /// receiver looked, and the reading there stays elapsed time. No ring at all is
    /// [`Watched::unrecorded`], the unchanged behaviour of a replay store or a history-only server.
    pub fn watched(&self, freq: FreqRange) -> Watched {
        match self.spans.iter().map(|s| s.time.start).min() {
            None => Watched::unrecorded(),
            Some(from) => Watched::recorded(from, &self.band_spans(freq)),
        }
    }

    /// **The one derivation of an emitter's presence track** (T-591), and therefore of its
    /// liveness.
    ///
    /// Liveness is a property of *the emitter* — one interval `[start, end?]`, ongoing until an
    /// end is affirmatively detected and revocable afterwards (ADR-0017/0019) — never a property
    /// of the route the caller happened to ask. It broke exactly once, and instructively:
    /// `/api/inventory` measured the idle gap off this band's tune history (T-410) while
    /// `/api/events` and `/api/tiles/events` each hard-coded [`hk_model::IdleGap::conservative`]
    /// (60 s), so T-254's ISM scene read `open: true` on 3 of 3 events beside inventory rows for
    /// the same emitter in the same window reading `ended`. Two surfaces, one fact, two
    /// derivations.
    ///
    /// The fix is not two constants agreeing — they would drift apart again the moment one was
    /// touched — but **one derivation**, here, beside the measurement that feeds it. Every serving
    /// surface reaches the track through this method and projects it with
    /// [`PresenceTrack::project`], which reuses the very gap that closed the intervals, so a row's
    /// `live`/`ended` and the `open` flags on the same emitter's events are one derivation seen
    /// twice. `inventory_api::hk_api_derives_liveness_in_exactly_one_place` holds the line by
    /// refusing any other call to `presence_intervals` in this crate.
    pub fn track(
        &self,
        repo: &Repository,
        id: EmitterId,
        freq: FreqRange,
        span: TimeRange,
        now: Timestamp,
    ) -> Result<PresenceTrack, RepoError> {
        let gap = self.idle_gap(freq, span);
        // T-940: and the silence that gap is compared against is the silence the receiver
        // *observed* — watched time on this band, or what the tracker reported for a track it is
        // still following — never the wall clock. On staging every on-air FM station read `ended`
        // because the time since its row was last written counted as quiet.
        Ok(PresenceTrack {
            gap,
            intervals: repo.presence_intervals(id, gap, now, &self.watched(freq))?,
        })
    }
}

/// One emitter's presence track as [`ObservedCoverage::track`] derived it, carrying the measured
/// [`IdleGap`] that closed its intervals so every projection of it reads the same gap.
///
/// Keeping the gap *with* the intervals is the point: a caller cannot project this track under a
/// different gap from the one that produced it without saying so, which is the shape of the T-591
/// defect it exists to prevent.
#[derive(Clone, Debug)]
pub struct PresenceTrack {
    /// The gap measured off the band's tune history, which closed [`Self::intervals`].
    pub gap: IdleGap,
    /// The emitter's disjoint presence intervals, in start order (docs/07 §2.27).
    pub intervals: Vec<PresenceInterval>,
}

impl PresenceTrack {
    /// The track projected through one view window: intervals in it, time on air inside it, and
    /// the `live`/`ended`/`absent` an inventory row renders.
    pub fn project(&self, span: TimeRange) -> Presence {
        hk_model::presence_in_window(&self.intervals, span, self.gap)
    }
}

fn f64_of(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64).filter(|x| x.is_finite())
}

/// The tune history the IQ ring journal already holds: one segment per provenance, so one per
/// retune, each naming the device that produced it (ADR-0014).
///
/// A segment whose tuned window misses the region is dropped here rather than folded: coverage of
/// somewhere else is not coverage of here.
fn ring_spans(state: &ApiState, freq: FreqRange, window: TimeRange) -> Vec<CoverageSpan> {
    let Some(c) = state.iq_buffer.as_deref() else {
        return Vec::new();
    };
    let status = c.status(&crate::iqbuffer::IqBufferQuery {
        t0: Some(window.start.as_unix_nanos() as f64 * 1e-9),
        t1: Some(window.end.as_unix_nanos() as f64 * 1e-9),
        limit: RING_SEGMENTS,
    });
    let Some(segments) = status.get("segments").and_then(Value::as_array) else {
        return Vec::new();
    };
    segments
        .iter()
        .filter_map(|s| {
            let center_hz = f64_of(s, "center_hz")?;
            let rate = f64_of(s, "sample_rate_hz").filter(|r| *r > 0.0)?;
            let (t0, t1) = (
                s.get("t0_ns").and_then(Value::as_i64)?,
                s.get("t1_ns").and_then(Value::as_i64)?,
            );
            let half = rate / 2.0;
            let f = FreqRange::new(center_hz - half, center_hz + half);
            if !f.overlaps(&freq) {
                return None;
            }
            Some(CoverageSpan {
                // The journal names the radio, so this span is device-local. A segment without one
                // is `Unknown` rather than borrowing whichever device happens to be running.
                device: s
                    .get("device_id")
                    .and_then(Value::as_str)
                    .filter(|d| !d.is_empty())
                    .map_or(Device::Unknown, |d| Device::Id(d.to_string())),
                time: TimeRange::new(
                    Timestamp::from_unix_nanos(t0),
                    Timestamp::from_unix_nanos(t1),
                ),
                freq: f,
                // The ring holds the **samples**, DC included, so a segment's whole tuned window
                // is analysable: a client can re-run anything over it. The DC notch is a statement
                // the *observation log* makes about one dwell's analysis (T-595), and a segment
                // records no such exclusion — inventing one here would be a guess, and inventing
                // the reverse is what greyed the notch in the first place.
                analysis: hk_store::coverage::Analysis::Analysed,
                center_hz,
                sample_rate_hz: rate,
            })
        })
        .collect()
}

/// The tune history the observation log holds: every dwell's and every sweep hop's analysed extent
/// over the interval it was analysed in (ADR-0012 §1).
///
/// Since T-378 these records name the front end that observed, so a span from the log is
/// device-local over the log's retention horizon — far beyond the IQ ring's — and the coverage map
/// can answer "did *this* radio look here" over the whole of it. A record without a device stays
/// [`Device::Unknown`] (`hk_store::coverage::record_device`).
///
/// **The record → span mapping itself lives in `hk-store`**, next to the rasteriser that consumes
/// it ([`hk_store::spans_from_records`], T-406): it is the one place that decides what shape a
/// record takes on the grid, and `docs/16` §7 step 6 turns on that shape being the **step's** own
/// band and interval rather than one coarse claim over a pass. This function is the route's half:
/// page the log, then hand the records over.
///
/// Returns the spans and how many of them named a device, so the answer's `sources` row can say
/// whether this record actually knew.
#[cfg(test)]
fn observation_spans(
    store: &ObservationStore,
    freq: FreqRange,
    window: TimeRange,
) -> (Vec<CoverageSpan>, usize) {
    let (spans, named, _) = observation_spans_paged(store, freq, window);
    (spans, named)
}

/// Pages of [`MAX_RECORD_LIMIT`] records one coverage answer may walk (T-1055).
///
/// One page was the whole read until T-1055, and a page is 10 000 records: a full 1 MHz–6 GHz
/// `Scan everything` pass writes one record per step (~418 coarse, far more fine), so a window
/// holding a couple of dozen passes reached the limit and the answer silently lost the records past
/// it — **the newest ones**, because the log answers oldest-first. Four pages is 40 000 records,
/// two orders of magnitude above one pass, and the read is bounded rather than unbounded because a
/// coverage answer is on the interactive path: a client asks for one per viewport.
///
/// Past four pages the answer **says so** rather than pretending (`sources[].truncated`): a source
/// that answered with fewer records than it holds cannot support *"nothing looked here"*.
const MAX_RECORD_PAGES: usize = 4;

/// [`observation_spans`], and whether the read was **cut** (T-1034, paged by T-1055): the log
/// answers in log order, oldest first, so a cut read is missing the NEWEST records — the ones a
/// reader asking "when was this band last looked at" needs most.
fn observation_spans_paged(
    store: &ObservationStore,
    freq: FreqRange,
    window: TimeRange,
) -> (Vec<CoverageSpan>, usize, bool) {
    observation_spans_pages(store, freq, window, MAX_RECORD_LIMIT, MAX_RECORD_PAGES)
}

/// [`observation_spans_paged`] with the page size and page budget given, so the paging itself is
/// testable without writing 40 000 records (the bounds are constants, the walk is the behaviour).
fn observation_spans_pages(
    store: &ObservationStore,
    freq: FreqRange,
    window: TimeRange,
    limit: usize,
    pages: usize,
) -> (Vec<CoverageSpan>, usize, bool) {
    let mut spans: Vec<CoverageSpan> = Vec::new();
    let mut named = 0usize;
    let mut cursor = 0usize;
    let mut cut = false;
    for _ in 0..pages.max(1) {
        let page = store.query(&RecordQuery {
            freq,
            span: window,
            tier: None,
            cursor,
            limit,
        });
        let read = hk_store::spans_from_records(&page.records, &page.geometries, freq);
        spans.extend(read.spans);
        named += read.named;
        match page.next_cursor {
            // A further page exists: walk it, and if this was the last one we are allowed, the read
            // is cut and must say so.
            Some(next) => {
                cursor = next;
                cut = true;
            }
            None => {
                cut = false;
                break;
            }
        }
    }
    (spans, named, cut)
}

/// **The dwell each front end is inside right now** (T-596), as spans.
///
/// A record reaches the log when a dwell *closes* — on a retune, or after
/// `hk_pipeline::observe::INTERACTIVE_RECORD_MAX_NS` (60 s) of a steady tune. So for up to a whole
/// dwell after every retune the log holds nothing for the band the radio is sitting on and
/// measuring. With an IQ ring that gap is covered by the ring journal, which opens a segment on
/// every provenance change; **with the ring refused it is covered by nothing**, and T-588 measured
/// what that does: 18 rows (18 s) of `max_db` served `unobserved` — data that exists, drawn grey.
/// The ring was refused **because the disk was full**, which is the field failure mode of a
/// portable device, not an exotic configuration: it must be survived honestly rather than by the
/// canvas lying about where the radio looked.
///
/// The open dwell is not a weaker claim than a sealed one and does not get a mark of its own. The
/// samples are sampled, the analysis has run over the rows that exist, and only the bookkeeping is
/// outstanding — so it goes through the **same** [`hk_store::spans_from_records`], carries the
/// same `dc_excluded` notch T-595 marks `"excluded"`, and a cell's state does not change when the
/// seal catches up a minute later. A third coverage state here would stripe the live edge with a
/// mark that vanished for samples that never changed; what *is* new is the third **source**, so a
/// client can see which evidence carried the live edge.
///
/// # It is a tune record the server holds, so it counts toward the record horizon (T-680)
///
/// T-596 first served the open dwell **clipped to** [`Memory::oldest_record`] and computed that
/// horizon from sealed sources only. T-680 reversed that, on this evidence:
///
/// - **The clip only ever bound before a seal.** A dwell opens where the previous one closed (the
///   observers close, then start), so whenever the log holds any sealed record the open dwell
///   starts after it and the clip was a no-op. It bit in exactly one situation: *no* sealed record
///   yet — the first ≤ 60 s of a store (or of every dwell a failed write never sealed) — and the
///   IQ ring refused or already past the dwell's start. That is T-588's situation (the disk is
///   full), where the clip dropped the only evidence there was.
/// - **There it broke T-596's own rule.** "A cell's state does not change when the seal catches
///   up" — yet the clipped rows read `"unknown"` until the seal and `"observed"` after it, for
///   samples that never changed. The unknown band T-507's contract test asserted on a young
///   server with a 2 s ring was that artefact: nothing had been forgotten; the record of those
///   seconds was in memory, under sixty seconds from being written.
/// - **The feared failure mode cannot happen by construction.** An open dwell reaches back only to
///   its own start (settle, retune, or the previous seal), never behind it, and `oldest_record` is
///   a `min`: adding a source only admits rows *that source reaches*. A record that was genuinely
///   lost (a seal the log never wrote) lies *before* the next open dwell and stays `"unknown"`;
///   and the open dwell itself is bounded — replaced every tick, closed on retune and after
///   `INTERACTIVE_RECORD_MAX_NS` of a steady tune.
///
/// So it is folded into [`Memory::of`] like the ring and the log: one source among three, read
/// once per answer so the horizon and the spans come from the same snapshot. The two horizons are
/// now symmetric — the dwell in flight moves `oldest_record_s` back to its own start exactly as it
/// moves `as_of_s` forward to the live edge (T-532).
fn open_dwell_spans(
    open: &[hk_model::attention::observation::ObservationRecord],
    freq: FreqRange,
    window: TimeRange,
) -> (Vec<CoverageSpan>, usize) {
    let records: Vec<_> = open
        .iter()
        .filter(|r| match r {
            hk_model::attention::observation::ObservationRecord::Dwell(d) => {
                d.observed.start < window.end && d.observed.end > window.start
            }
            _ => false,
        })
        .cloned()
        .collect();
    let read = hk_store::spans_from_records(&records, &[], freq);
    (read.spans, read.named)
}

/// The earliest instant any dwell in flight has run from (T-680) — a tune record the server holds
/// in memory, and so a reach for [`Memory::oldest_record`] and [`Memory::recording_began`].
fn open_dwells_began(open: &[hk_model::attention::observation::ObservationRecord]) -> Option<i64> {
    open.iter()
        .filter_map(|r| match r {
            hk_model::attention::observation::ObservationRecord::Dwell(d)
                if d.observed.end > d.observed.start =>
            {
                Some(d.observed.start.as_unix_nanos())
            }
            _ => None,
        })
        .min()
}

/// The tune history behind one answer, and **how far back it reaches**.
///
/// Both halves come from records that already exist; neither is a new ledger. The second half is
/// the one T-423 added, and it is what lets a cell say `"unknown"` instead of grey: see this
/// module's header on the fourth state.
pub(crate) struct Evidence {
    /// Every span from every consulted source, device-local, unmerged.
    pub spans: Vec<CoverageSpan>,
    ring: usize,
    ring_named: usize,
    log: usize,
    log_named: usize,
    /// Spans from the dwells in flight (T-596): what the radio is on **now**, before the
    /// observation log has sealed a record for it.
    open: usize,
    open_named: usize,
    /// Whether the IQ ring **can contribute evidence**, not whether a handle is wired (T-640):
    /// [`ring_can_answer`], and the coverage state that turns on the difference.
    ring_available: bool,
    /// **Why the ring cannot contribute, when it cannot** (T-920): see [`RingSilence`]. `None`
    /// exactly when `ring_available` is true, or when no ring handle is wired at all.
    ring_silence: Option<RingSilence>,
    log_available: bool,
    /// The earliest instant **any** consulted source still holds a record for; `None` when no
    /// source holds one at all.
    ///
    /// `min`, not `max`: a row is knowable if *at least one* record reaches it. The IQ ring's
    /// journal opens a segment on every provenance change, so within what the ring still buffers
    /// an absence of segment really is "this front end was not tuned here"; the observation log
    /// drops whole hour segments, so its oldest surviving segment's hour start is exactly the
    /// instant past which its silence stops being evidence; a dwell in flight (T-680) reaches back
    /// to its own start and no further. Before the earliest of them, no record can speak, and
    /// `unobserved` would be a claim nothing supports.
    pub oldest_record: Option<Timestamp>,
    /// **When this server's memory of recording begins** (T-507): the earliest instant any source
    /// knows recording happened here — the spectrum history's own record of when it began, the IQ
    /// ring's oldest sample, the observation log's earliest record (not its oldest hour, which is
    /// a filing boundary up to an hour before any sample). `None` when nothing here has ever
    /// recorded anything.
    ///
    /// This is what keeps `"unknown"` narrow. Before it, nothing this installation knows of was
    /// recording, so an unsampled cell there is honestly `unobserved` — a fresh server's first
    /// minute is not a forgotten past. Between it and [`Evidence::oldest_record`] recording
    /// happened but the tune record of it did not survive (a restart lost the previous run's
    /// journal): that span, and only that span, is `"unknown"`.
    pub recording_began: Option<Timestamp>,
    /// A source has **discarded** records that could reach back before `recording_began`, so that
    /// boundary is not a floor and every row before `oldest_record` is `"unknown"`. `Some(why)`.
    pub forgotten: Option<&'static str>,
    /// **How far forward this answer's evidence reaches** (T-532): the newest instant any consulted
    /// tune record ends at, clamped into the asked-for window; `None` when no record reaches into
    /// the window at all.
    ///
    /// Over **any** band since T-881, not only this one: a record of the radio somewhere else at an
    /// instant is evidence that this band was unobserved then, so a departed band's answer reaches
    /// as far as the radio's record does, not only to the moment it was left. (The observation log
    /// is read for this band only — see [`Evidence::collect`].)
    ///
    /// # Why the young end needs its own horizon, and why its absence was a bug
    ///
    /// [`Evidence::oldest_record`] exists because *absence of a span is not evidence of absence*
    /// past the point the records reach — before it, "we did not look" is a claim nothing supports,
    /// so those rows are `"unknown"` rather than grey. **The same is true at the other end, and was
    /// not said.** A tune record is written as capture proceeds, so it stops at the newest sample;
    /// every row after that is served `unobserved` — a positive claim that nothing ever looked —
    /// about an instant the record simply has not reached yet.
    ///
    /// Served, that claim is momentarily harmless: nothing has happened there *yet*. **Held, it
    /// becomes false the instant capture continues**, and a tile is held — a client keeps a resident
    /// copy for as long as it can, and T-460/T-495 are two tickets about exactly how long that is.
    /// With a one-second coverage cell the error hid inside the cell the live edge was already in;
    /// at the fidelity floor's 40 ms cell (T-501) it is a visible band of grey across the newest
    /// second of every live pane, over rows the radio recorded and this server is serving.
    ///
    /// So the answer states where its own evidence stops, and a reader may not read `unobserved`
    /// past it as *"nothing looked"* — only as *"this answer does not reach here"*. It is the same
    /// sentence as `oldest_record_s`, pointing the other way.
    pub newest_record: Option<Timestamp>,
    /// The **IQ ring** answered with fewer segments than it holds over the window (T-1034): its
    /// segment list reached [`RING_SEGMENTS`]. The spans are then not every span, and "no span
    /// here" is not evidence of anything.
    pub ring_truncated: bool,
    /// The **observation log** answered with fewer records than it holds over the window (T-1034,
    /// paged by T-1055): the read walked [`MAX_RECORD_PAGES`] pages and a further page was still
    /// pending. The log answers oldest-first, so what is missing is the NEWEST records.
    pub log_truncated: bool,
}

impl Evidence {
    /// Whether any tune history on this server can contribute evidence at all (T-468). With none,
    /// every plane is uniformly `unobserved` and there is no forward horizon to wait for.
    pub(crate) fn has_source(&self) -> bool {
        self.ring_available || self.log_available
    }

    /// Whether **either** tune history answered with fewer records than it holds: then "no span
    /// here" proves nothing, and a caller that would otherwise read absence as evidence (the
    /// shadow search's quiet bound, `tiles::record_quiet_after`) must not.
    pub(crate) fn truncated(&self) -> bool {
        self.ring_truncated || self.log_truncated
    }

    /// Reads both tune histories over `freq × window`, and each one's reach.
    pub(crate) fn collect(state: &ApiState, freq: FreqRange, window: TimeRange) -> Self {
        // T-680: the dwells in flight, read ONCE, so the record horizon and the spans rasterised
        // beside it come from the same snapshot.
        let open_dwells = state
            .observations
            .as_ref()
            .map(|s| s.open_dwells())
            .unwrap_or_default();
        let memory = Memory::of(state, &open_dwells);
        // T-881: the ring's segments over EVERY band, read once. The band's own spans are the
        // subset `ring_spans` would have kept (the same `overlaps` test), and the rest are the
        // evidence that the radio was somewhere else — which is what `newest_record` reads below.
        let ring_all = ring_spans(state, ALL_FREQ, window);
        // Every segment the ring returned is in `ring_all` unless malformed, so a list at the
        // limit may be a cut one.
        let ring_truncated = ring_all.len() >= RING_SEGMENTS;
        let ring_reach = ring_all.iter().map(|s| s.time.end).max();
        let mut spans: Vec<CoverageSpan> = ring_all
            .into_iter()
            .filter(|s| s.freq.overlaps(&freq))
            .collect();
        let ring = spans.len();
        let ring_named = spans.iter().filter(|s| s.device.is_named()).count();
        let mut log_named = 0;
        let mut log_truncated = false;
        let mut open = 0;
        let mut open_named = 0;
        if let Some(store) = state.observations.as_ref() {
            let (log_spans, named, cut) = observation_spans_paged(store, freq, window);
            log_truncated = cut;
            log_named = named;
            spans.extend(log_spans);
        }
        let log = spans.len() - ring;
        // T-596: the live edge, between the last seal and now. Counted as its own source: it is
        // the one evidence a refused IQ ring leaves standing. Since T-680 it is ALSO one of the
        // reaches `memory.oldest_record` is the min over, so it needs no clip: it cannot start
        // before the horizon it helped compute.
        //
        // **Folded in BEFORE `newest_record` is taken, and the order is the semantics** (T-596 x
        // T-532). T-532's rule is that the forward horizon comes from the SAME `spans` the planes
        // are rasterised from, so the summary and the body cannot disagree; the open dwell's spans
        // reach the live edge and DO rasterise, so taking `newest_record` first would publish an
        // `as_of_s` at the last SEAL while the plane beside it paints `observed` for seconds after
        // it - the exact split T-532 exists to close. Both horizons now read the dwell in flight
        // (T-680): it moves `as_of_s` forward to the live edge and `oldest_record` back to its own
        // start, and neither further.
        if state.observations.is_some() {
            let (open_spans, named) = open_dwell_spans(&open_dwells, freq, window);
            open = open_spans.len();
            open_named = named;
            spans.extend(open_spans);
        }
        // The newest instant the consulted records reach over this band, never past the window they
        // were asked about: an answer cannot be evidence about time it did not look at. Taken from
        // the SAME `spans` the planes are rasterised from, so the horizon and the plane cannot
        // disagree - the failure mode of serving a summary beside a body.
        //
        // **Over ANY band, not only this one** (T-881). A record that reaches an instant somewhere
        // else is evidence about this band at that instant too: the radio was elsewhere, so an
        // `unobserved` cell here is the true answer, not "the record has not got here yet". Taking
        // this band's spans alone stopped a DEPARTED band's horizon at the moment it was left —
        // so a tile straddling the departure left every row after it as the pane's pending ground
        // for good, and a tile wholly after it named no horizon at all and was drawn as served:
        // THE grey, from a copy that could not speak about the rows recorded since it was built
        // (the fog-of-war defect: the newest rows of a departed band, grey). The other bands'
        // reach comes from the SAME snapshot the band's spans do — the ring status read once
        // above, the dwells in flight read once at the top — so it can never run ahead of the
        // plane beside it. The observation log is still read for this band only: a whole-band
        // page would compete with this band's records for the page limit, and the dwells in
        // flight already carry every band's live edge.
        let open_reach = open_dwells
            .iter()
            .filter_map(|r| match r {
                hk_model::attention::observation::ObservationRecord::Dwell(d)
                    if d.observed.start < window.end && d.observed.end > window.start =>
                {
                    Some(d.observed.end)
                }
                _ => None,
            })
            .max();
        let newest_record = spans
            .iter()
            .map(|s| s.time.end)
            .chain(ring_reach)
            .chain(open_reach)
            .max()
            .map(|t| t.min(window.end));
        Evidence {
            spans,
            ring,
            ring_named,
            log,
            log_named,
            open,
            open_named,
            // T-640: whether this source can actually contribute evidence, NOT whether a handle is
            // wired — a refused ring answers `/api/iqbuffer` and holds no journal.
            ring_available: memory.ring_can_answer,
            // T-920: `available: false` never travels alone.
            ring_silence: memory.ring_silence,
            log_available: state.observations.is_some(),
            oldest_record: memory.oldest_record,
            recording_began: memory.recording_began,
            forgotten: memory.forgotten,
            newest_record,
            ring_truncated,
            log_truncated,
        }
    }

    /// Which tune histories answered, how many of each one's spans actually named the radio, and
    /// whether every span it contributed did. `device_known` is **measured, not declared**
    /// (T-378): a log still holding records written before devices were logged reports them as the
    /// unattributed spans they are instead of claiming a device-local horizon it has not got. A
    /// source with no spans still appears, so a client can tell "this record had nothing here"
    /// from "this record was not consulted".
    ///
    /// **T-920: `available: false` states which negative it is.** Every row carries `state` and
    /// `reason` beside `available`, so a client never has to guess whether a source that
    /// contributed nothing is still being laid down (`state: "allocating"`, and it will answer in
    /// a moment), was refused for want of disk (`"refused"`), is locked by another process
    /// (`"locked"`), or is simply not configured (`state: null`). Both come from the status
    /// `/api/iqbuffer` already serves; nothing new is measured here. A silent `false` cost a
    /// remote Linux worker four red runs of
    /// `coverage_greys_only_what_was_never_observed_and_names_the_device_that_looked`, whose wait
    /// was satisfied by the open dwell while the ring was still allocating.
    fn sources_json(&self) -> Value {
        // T-920: what a source that cannot answer says about itself. `available: false` is never
        // served bare — see [`RingSilence`] for why a silent negative is the defect.
        let (ring_state, ring_reason) = match (&self.ring_silence, self.ring_available) {
            (Some(s), _) => (
                s.state.clone().map_or(Value::Null, Value::String),
                s.reason.clone().map_or(Value::Null, Value::String),
            ),
            // No silence recorded and no ring handle wired: the run has no ring to ask.
            (None, false) => (
                Value::Null,
                json!("this run has no IQ capture ring: no ring is wired to this server"),
            ),
            (None, true) => (json!("open"), Value::Null),
        };
        let log_reason = if self.log_available {
            Value::Null
        } else {
            json!(
                "this run has no observation log, so no sealed dwell can say where the radio looked"
            )
        };
        json!([
            { "kind": "iq-ring", "spans": self.ring, "named_spans": self.ring_named,
              "device_known": self.ring_named == self.ring,
              "available": self.ring_available,
              // T-1055: this source answered with fewer records than it holds. Served per source,
              // always, so a reader never has to infer it from a suspiciously round count.
              "truncated": self.ring_truncated,
              "state": ring_state, "reason": ring_reason },
            { "kind": "observation-log", "spans": self.log, "named_spans": self.log_named,
              "device_known": self.log_named == self.log,
              "available": self.log_available,
              "truncated": self.log_truncated,
              "state": if self.log_available { json!("open") } else { Value::Null },
              "reason": log_reason.clone() },
            // T-596: the dwells in flight. Same claim as a sealed record and rasterised the same
            // way; named separately so a client can see that the live edge was carried by a dwell
            // the log has not sealed yet - which, with the IQ ring refused (a full disk on a
            // portable device), is the only evidence there is.
            { "kind": "open-dwell", "spans": self.open, "named_spans": self.open_named,
              "device_known": self.open_named == self.open,
              "available": self.log_available,
              // The dwells in flight are read whole (there is one per front end), so this source
              // cannot be cut. Served anyway: a field that appears only sometimes is a field a
              // client reads as false when it is absent.
              "truncated": false,
              "state": if self.log_available { json!("open") } else { Value::Null },
              "reason": log_reason },
        ])
    }

    /// The rows of `g` whose unsampled cells must be served as `"unknown"`: those wholly before the
    /// record horizon **and not wholly before this server's memory of recording begins** (T-507).
    ///
    /// Three cases, and the first is the one T-423 left wide open:
    ///
    /// - **Nothing has ever recorded here** (`recording_began` is `None`, nothing forgotten): no
    ///   row is unknown. A server that never recorded has forgotten nothing; its answer about
    ///   every row is the true one, `unobserved`. This is what a freshly started or reset store
    ///   says about the time before it started — **T-507's purple wall** was every such row
    ///   served as `"unknown"` while live frames arrived.
    /// - **Recording began, and every record since survives** — the rows before `recording_began`
    ///   are `unobserved` for the same reason; the rows between it and `oldest_record` (a span
    ///   whose tune record did not survive, e.g. the previous run's IQ journal across a restart)
    ///   are `"unknown"`.
    /// - **A source discarded records that could predate both** (`forgotten`), or **there is no
    ///   tune history on this server at all** (no ring, no log): every row before `oldest_record`
    ///   — every row, if there is none — is `"unknown"`, T-423's rule unchanged. Those are the
    ///   cases where the server genuinely cannot say whether it looked.
    fn unknown_rows(&self, g: &CoverageGrid) -> std::ops::Range<usize> {
        let no_tune_history = !self.ring_available && !self.log_available;
        let end = match self.oldest_record {
            Some(t) => g.unknown_rows_before(t),
            None => g.nt,
        };
        if no_tune_history || self.forgotten.is_some() {
            return 0..end;
        }
        match self.recording_began {
            // Rows wholly before recording began are unobserved; a row straddling it stays
            // unknown (it may hold some of the forgotten span).
            Some(t) => g.unknown_rows_before(t).min(end)..end,
            None => 0..0,
        }
    }

    /// The horizon block: the boundary, where it came from, and what lies before it.
    fn horizon_json(&self, g: &CoverageGrid) -> Value {
        let unknown = self.unknown_rows(g);
        let secs = |t: Option<Timestamp>| t.map(|t| t.as_unix_nanos() as f64 * 1e-9);
        json!({
            // Unix s, or null when nothing on this server holds a tune record at all.
            "oldest_record_s": secs(self.oldest_record),
            // Unix s: when this server's memory of recording begins (T-507), or null when nothing
            // here has ever recorded. Rows wholly before it are `"unobserved"`, not `"unknown"`,
            // unless `forgotten` says a discarded record could reach back past it.
            "recording_began_s": secs(self.recording_began),
            // Why this server cannot bound what it forgot, or null when it can.
            "forgotten": self.forgotten,
            // Unix s: how far FORWARD this answer's evidence reaches (T-532) — over any band since
            // T-881 — or null when no record reaches into the window at all. `oldest_record_s`
            // pointing the other way — see [`Evidence::newest_record`] for why a held answer needs it.
            "as_of_s": secs(self.newest_record),
            // The rows served as `"unknown"` are exactly `[unknown_from_row, unknown_from_row +
            // unknown_rows)` — a contiguous band, so a client can check the states it was sent.
            "unknown_rows": unknown.len(),
            "unknown_from_row": unknown.start,
            "rows": g.nt,
            "rule": "a row wholly before `oldest_record_s` has no surviving tune record, so its \
                unsampled cells are \"unknown\" (we no longer know whether we looked) - UNLESS the \
                row is also wholly before `recording_began_s` and nothing is `forgotten`: before \
                this installation recorded anything, nothing looked, and the cell is \
                \"unobserved\". A server that has never recorded has forgotten nothing. An observed \
                cell is never relabelled: a surviving measurement is itself proof we looked.",
            "state_rule": "\"unknown\" carries no measurement keys, exactly like \"unobserved\", \
                and must be drawn as neither grey nor a level - forgetting is not a measurement of \
                nothing.",
            "as_of_rule": "this answer's records reach forward only as far as `as_of_s`. An \
                `\"unobserved\"` cell AFTER it means \"this answer does not reach here\", NOT \
                \"nothing looked\" - a tune record is written as capture proceeds, so it always \
                stops at the newest sample. A reader that KEEPS this answer (every tile cache does) \
                must not draw grey past `as_of_s`: the rows there are being recorded while the copy \
                ages, and grey is the one mark that may only mean the radio never looked. It is \
                read over ANY band (T-881): the radio recorded somewhere else up to `as_of_s`, so \
                an `\"unobserved\"` cell before it is the true answer even for a band the radio \
                left. `null` means no record reaches into this window at all, and then the whole \
                answer stands.",
        })
    }
}

/// What this server remembers about its own recording, read once per answer (T-423, T-507).
struct Memory {
    oldest_record: Option<Timestamp>,
    recording_began: Option<Timestamp>,
    forgotten: Option<&'static str>,
    /// **Whether the IQ ring can contribute evidence at all** (T-640) — not whether a handle is
    /// wired. See [`ring_can_answer`].
    ring_can_answer: bool,
    /// **Why it cannot**, in the ring's own words, when it cannot (T-920): the `allocation` state
    /// and the `reason` `/api/iqbuffer` is already serving. `None` when the ring can answer, or
    /// when there is no ring handle at all to ask.
    ring_silence: Option<RingSilence>,
}

/// Why the IQ ring is contributing nothing, read off its own status (T-920).
///
/// `available: false` on a source row used to be **silent**: a client could not tell a ring that
/// is *still being laid down* (and will answer in a moment) from one *refused* for want of disk,
/// from one another process has *locked*, from one *disabled by configuration*. Those are four
/// different facts about the device, and only the first resolves itself. The distinction is the
/// same one this module already insists on for a cell (`unobserved` is not `unknown`) and the same
/// one `BiasTee::Unknown` is not `Off`: a negative must say **which** negative it is.
///
/// Both fields come from the status `/api/iqbuffer` already serves — this invents no new state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RingSilence {
    /// The ring's `allocation` state (`"allocating"`, `"refused"`, `"locked"`, `"incompatible"`),
    /// or `None` where the status carries none (disabled by configuration).
    state: Option<String>,
    /// The ring's own `reason`, as served on `/api/iqbuffer`.
    reason: Option<String>,
}

impl RingSilence {
    /// The silence of a ring that cannot answer, or `None` for one that can (or no ring at all).
    fn of(status: Option<&Value>) -> Option<Self> {
        let s = status?;
        if ring_can_answer(Some(s)) {
            return None;
        }
        Some(Self {
            state: s
                .get("allocation")
                .and_then(Value::as_str)
                .map(str::to_owned),
            reason: s.get("reason").and_then(Value::as_str).map(str::to_owned),
        })
    }
}

/// **Can the IQ ring journal actually answer "did we look here"?** (T-640)
///
/// `state.iq_buffer.is_some()` asks whether a *handle* is wired, which is not the same question. A
/// ring whose allocation was **refused** for lack of free space — the field failure mode of a
/// portable device, and exactly how T-588 hit T-596 — still presents a handle and still answers
/// `/api/iqbuffer`, while holding no journal and contributing zero spans.
///
/// That distinction decides a coverage state. [`Evidence::unknown_rows`] reads `no_tune_history`
/// from this flag and the log's: with **no** tune history a row's unsampled cells are `"unknown"`,
/// because "we did not look" is a claim nothing supports. Reporting a refused ring as *available*
/// made the server answer `"unobserved"` — a positive claim that the radio did not look — over
/// rows it has no tune record of — a positive claim no record supports. (T-680's open dwell is not
/// the mirror of this: it *is* a record, reaching exactly its own interval.) *We cannot say* is
/// neither `observed` nor `unobserved`.
///
/// So the predicate is the status's own `enabled` — "the run buffers IQ", which every no-buffer
/// case (`refused`, `locked`, `incompatible`, disabled by configuration) reports as `false` with a
/// `reason` — minus the one enabled state that is not yet holding anything: `allocation ==
/// "allocating"`, where the ring is still being laid down and "nothing is buffered until it
/// completes". A status that does not say, or says something that is not a boolean, is **not**
/// read as available: this fails closed on to `"unknown"`, the answer that claims least.
fn ring_can_answer(status: Option<&Value>) -> bool {
    let Some(s) = status else { return false };
    s.get("enabled").and_then(Value::as_bool).unwrap_or(false)
        && s.get("allocation").and_then(Value::as_str) != Some("allocating")
}

impl Memory {
    /// The record horizon, the start of recording, and whether anything that could reach past the
    /// latter has been discarded.
    ///
    /// **The record horizon** (`oldest_record`): the earliest instant any tune record this server
    /// holds reaches. The IQ ring reports what it actually buffers; the observation log retains
    /// whole hour segments and drops whole hour segments, so its oldest hour's start is the exact
    /// boundary; the dwells in flight (T-680, see [`open_dwell_spans`]) reach their own start. A
    /// source that is absent, or holds nothing, contributes no reach.
    ///
    /// **The start of recording** (`recording_began`): the earliest of the IQ ring's reach, the
    /// observation log's earliest RECORD ([`hk_store::observation::ObservationStore::earliest_start`]
    /// — not its oldest hour, which `oldest_record` uses) and the spectrum history's own record of
    /// when it began ([`hk_store::Pyramid::recording_began`]
    /// — a fact it keeps from open and ingest, so it outlives the tiles that proved it).
    ///
    /// **Forgetting** is a discard that could predate that start: the observation log deleting a
    /// segment (its retention outlives the history's, so its oldest records can be the oldest
    /// anywhere), or the IQ ring evicting or discarding data on a server with no spectrum history
    /// to remember when recording began. The ring's routine eviction on a server *with* history is
    /// not forgetting in this sense: the history was recording over the same span and still knows
    /// when it began.
    fn of(
        state: &ApiState,
        open_dwells: &[hk_model::attention::observation::ObservationRecord],
    ) -> Self {
        let ring = state.iq_buffer.as_deref().map(|c| {
            c.status(&crate::iqbuffer::IqBufferQuery {
                t0: None,
                t1: None,
                limit: 1,
            })
        });
        let ring_t0 = ring
            .as_ref()
            .and_then(|s| match (f64_of(s, "t0"), f64_of(s, "t1")) {
                (Some(a), Some(b)) if b > a => Some((a * 1e9).round() as i64),
                _ => None,
            });
        let ring_discarded = ring.as_ref().is_some_and(|s| {
            s.pointer("/evicted/samples")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                > 0
                || s.get("discarded_slots")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    > 0
        });
        let log_t0 = state.observations.as_ref().and_then(|s| {
            s.hours()
                .into_iter()
                .min()
                .map(|h| h.saturating_mul(hk_store::observation::segment::HOUR_NS))
        });
        let log_deleted = state.observations.as_ref().is_some_and(|s| {
            s.stats()
                .segments_deleted
                .load(std::sync::atomic::Ordering::Relaxed)
                > 0
        });
        // The log's reach for `recording_began` is its earliest RECORD, not its oldest hour: the
        // hour is what retention keeps or drops, so it bounds `oldest_record`, but a segment is
        // filed under the hour it falls in, which begins up to an hour before anything was sampled
        // (deflake-0922: the first sealed dwell moved a 15 s old server's start 923 s back).
        let log_began = state
            .observations
            .as_ref()
            .and_then(|s| s.earliest_start())
            .map(Timestamp::as_unix_nanos);
        let (history_present, history_began) = history_began(state);
        let min = |a: Option<i64>, b: Option<i64>| match (a, b) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (x, y) => x.or(y),
        };
        // T-680: the dwell in flight is a tune record this server holds (in memory, not yet
        // sealed) - a third reach, bounded to its own start. See [`open_dwell_spans`].
        let open_t0 = open_dwells_began(open_dwells);
        let oldest_record = min(min(ring_t0, log_t0), open_t0);
        let forgotten = if log_deleted {
            Some("the observation log has deleted segments by retention")
        } else if ring_discarded && !history_present {
            Some(
                "the IQ ring has discarded data and no spectrum history remembers when recording began",
            )
        } else {
            None
        };
        Memory {
            oldest_record: oldest_record.map(Timestamp::from_unix_nanos),
            // `open_t0` too, so `recording_began <= oldest_record` holds whatever reached back.
            recording_began: min(min(min(ring_t0, log_began), history_began), open_t0)
                .map(Timestamp::from_unix_nanos),
            forgotten,
            // T-640: measured off the status this function already read, not off the handle.
            ring_can_answer: ring_can_answer(ring.as_ref()),
            // T-920: and when it cannot answer, why — off the same status.
            ring_silence: RingSilence::of(ring.as_ref()),
        }
    }
}

/// Whether any spectrum history is present, and the earliest instant any of them began recording.
///
/// Both lattices are read: the pyramid behind `/api/history` (or the floor's, when that is what
/// this server keeps) and the view lattice behind `/api/tiles`. Each lock is held for two field
/// reads.
fn history_began(state: &ApiState) -> (bool, Option<i64>) {
    let mut present = false;
    let mut began: Option<i64> = None;
    let mut note = |t: Option<Timestamp>| {
        present = true;
        if let Some(t) = t.map(|t| t.as_unix_nanos()) {
            began = Some(began.map_or(t, |b| b.min(t)));
        }
    };
    if state.history.is_some() || state.floor.is_some() {
        if let Ok(t) = crate::http::with_history(state, |p| Ok(p.recording_began())) {
            note(t);
        }
    }
    if let Some(v) = state.view_history.as_ref() {
        if let Ok(p) = v.lock() {
            note(p.recording_began());
        }
    }
    (present, began)
}

/// The wire vocabulary of a coverage cell's state, **indexed by its code** (T-467).
///
/// This is the alphabet [`TileOverlay`]'s compact planes are written in, and it is served beside
/// every one of them so a code can never be read against an alphabet the answer did not state. The
/// order is fixed by [`state_code`], and [`cell_json`] — the per-cell form `/api/coverage` and
/// `/api/timeline` still serve — is asserted against it cell-state for cell-state, so the two
/// encodings cannot drift into disagreeing about what a cell is.
pub(crate) const COVERAGE_STATES: [&str; 4] = ["unobserved", "observed", "unknown", "excluded"];
/// Nothing ever looked. Grey, and **only** this is grey.
const UNOBSERVED: u8 = 0;
/// The radio was here.
const OBSERVED: u8 = 1;
/// We no longer know whether we looked (T-423). Not grey, not a level, not `unobserved`.
const UNKNOWN: u8 = 2;
/// Sampled, and **deliberately excluded from analysis** (T-595): the receiver's own DC/LO notch.
/// Observed — the measurement is there and must be drawn — with no detection claim over it.
///
/// Appended after `unknown` rather than inserted beside `observed` so every existing code keeps its
/// value: the alphabet is served with the planes, but a client caching the old one must not read an
/// old code as a new state.
const EXCLUDED: u8 = 3;

/// One cell's state as a code into [`COVERAGE_STATES`] — the same three-way decision
/// [`cell_json`] makes, and written next to it so it stays the same decision.
fn state_code(c: &Coverage, beyond_horizon: bool) -> u8 {
    match c.sampled() {
        // Only `unobserved` can become `unknown`. An observed cell stays observed past the
        // horizon: the measurement is the proof.
        None if beyond_horizon => UNKNOWN,
        None => UNOBSERVED,
        // Sampled, nothing of it analysed: the DC notch. Its own code (T-595), never grey.
        Some(s) if s.excluded() => EXCLUDED,
        Some(_) => OBSERVED,
    }
}

/// One cell's JSON.
///
/// An unobserved cell carries **no measurement keys**, so there is nothing a client can read as a
/// zero level or a zero occupancy — and an `"unknown"` cell (this module's fourth state) carries
/// none either, for the same reason and one horizon out.
///
/// `shade` is `None` when the caller has no shading to attach, in which case the key is **absent**
/// rather than `null`: on this route `shade: null` already means *sampled, level not retained*, so
/// a route that carries its own levels (`/api/timeline`) must not appear to be making that claim.
fn cell_json(c: &Coverage, shade: Option<Option<f32>>, beyond_horizon: bool) -> Value {
    match c.sampled() {
        // Only `unobserved` can become `unknown`. An observed cell stays observed past the
        // horizon: the measurement is the proof.
        None if beyond_horizon => json!({ "state": "unknown" }),
        None => json!({ "state": "unobserved" }),
        Some(s) => {
            let mut v = json!({
                // `"excluded"` (T-595) is an **observed** cell: the radio sampled it, the history
                // holds rows over it, and the only thing that did not happen is the analysis. It
                // keeps every measurement key an `"observed"` cell has — and `analysed_s: 0.0`
                // says, checkably, why it is not simply `"observed"`.
                "state": if s.excluded() { "excluded" } else { "observed" },
                "spans": s.spans,
                "observed_s": s.observed_ns as f64 * 1e-9,
                "analysed_s": s.analysed_ns as f64 * 1e-9,
                "duty": s.duty,
                "last_s": s.last.as_unix_nanos() as f64 * 1e-9,
                "center_hz": s.center_hz,
                "sample_rate_hz": s.sample_rate_hz,
            });
            if let (Some(shade), Some(obj)) = (shade, v.as_object_mut()) {
                // The level the spectrum history holds here, normalised to this answer's own
                // observed range so the client never picks a colour scale from the numbers it
                // happens to hold. `null` = sampled, level not retained — drawn differently from
                // grey, never as grey and never as the bottom of the ramp.
                obj.insert(
                    "shade".into(),
                    shade.map(|x| json!(x)).unwrap_or(Value::Null),
                );
            }
            v
        }
    }
}

/// One grid's JSON, with the `unknown_rows` band of rows served as the fourth state.
///
/// `unknown_rows` is a band of **rows** because the horizon is a time: a discarded record takes
/// every frequency with it (this module's header, reason 2).
fn grid_json(
    g: &CoverageGrid,
    shades: Option<&[Option<f32>]>,
    unknown_rows: std::ops::Range<usize>,
) -> Value {
    let mut unknown_cells = 0usize;
    // T-964: the survey census, collapsed over time — see [`band_census`]. Accumulated in the same
    // pass as the cells, from the same `beyond`, so the two censuses cannot disagree about a cell.
    let mut bands = vec![BandState::Unobserved; g.nf];
    let cells: Vec<Value> = g
        .cells
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let beyond = g.nf > 0 && unknown_rows.contains(&(i / g.nf));
            unknown_cells += usize::from(beyond && !c.is_observed());
            if let Some(b) = bands.get_mut(i % g.nf.max(1)) {
                *b = (*b).max(BandState::of(c, beyond));
            }
            cell_json(c, shades.map(|s| s.get(i).copied().flatten()), beyond)
        })
        .collect();
    json!({
        "device": g.device.as_str(),
        // Whether `device` is a real front-end identity. `"unknown"` and `"any"` are labels, not
        // radios, and a client must not attribute their coverage to a device.
        "named": g.device.is_named(),
        // Cells whose state is `"observed"`, strictly — the `"excluded"` ones are counted beside
        // them, exactly as the compact plane form counts them, so the two encodings cannot drift
        // into disagreeing about one cell (T-595).
        "observed_cells": g.observed_cells() - g.excluded_cells(),
        // Sampled, and wholly excluded from analysis (T-595) — the DC notch. `observed_cells +
        // excluded_cells` is the sampled total.
        "excluded_cells": g.excluded_cells(),
        // Cells that are genuinely grey: nothing looked, and a surviving record says so. The
        // `"unknown"` cells are **not** counted here — they are the fourth state, and adding them
        // in would be the collapse this route exists to refuse.
        "unobserved_cells": g.unobserved_cells() - unknown_cells,
        "unknown_cells": unknown_cells,
        "observed_fraction": g.observed_fraction(),
        // T-964: the same census with the **time axis collapsed** — the survey question. See
        // [`band_census`] for why a grid census cannot answer it.
        "bands": band_census(&bands),
        "cells": cells,
    })
}

/// One frequency cell's state once its rows are collapsed, ordered so that `max` keeps the
/// strongest claim: a band the radio sampled at any instant *was* sampled, whatever the rest of the
/// column says, and forgetting (`Unknown`) outranks grey because grey is the positive claim.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum BandState {
    /// Nothing looked here at any instant in the window, and a surviving record says so.
    Unobserved,
    /// No row was sampled and at least one row is past the record horizon: we no longer know.
    Unknown,
    /// Sampled at some instant, and every sampled row was excluded from analysis (T-595).
    Excluded,
    /// Sampled, and analysed, at some instant in the window.
    Observed,
}

impl BandState {
    fn of(c: &Coverage, beyond_horizon: bool) -> Self {
        match c.sampled() {
            Some(s) if s.excluded() => Self::Excluded,
            Some(_) => Self::Observed,
            None if beyond_horizon => Self::Unknown,
            None => Self::Unobserved,
        }
    }
}

/// **The survey census: the same coverage with the time axis collapsed** (T-964).
///
/// A grid census counts (time × frequency) cells, and that is the wrong denominator for the one
/// question the fog-of-war view asks — *where has this radio ever looked?* A front end sees one
/// window at a time, so a **complete** 1 MHz–6 GHz survey pass can never occupy more than a thin
/// diagonal of a (time × frequency) grid: T-964 measured a finished full-range pass reported as
/// *"4.3 % of this surface was ever sampled (176 of 4096 coverage cells)"* — arithmetically true of
/// the 128 × 32 grid it counted, and read by the user as *the survey did not light the map*.
///
/// The collapse is the honest answer to the question actually being asked: a frequency cell counts
/// as sampled if **any** row in the window sampled it. It is deliberately a *different* number from
/// `observed_cells`, not a replacement for it — the grid census is what the canvas draws, because a
/// cell is a claim about an instant, and this one is what a sentence about the survey may cite.
/// Both are served, so a client never has to fold the grid itself to get either.
fn band_census(bands: &[BandState]) -> Value {
    let count = |want: BandState| bands.iter().filter(|b| **b == want).count();
    let (observed, excluded) = (count(BandState::Observed), count(BandState::Excluded));
    json!({
        "cells": bands.len(),
        // Strictly observed, with `excluded_cells` beside it — the same split as the grid census,
        // so `observed + excluded` is the sampled total there and here.
        "observed_cells": observed,
        "excluded_cells": excluded,
        "unobserved_cells": count(BandState::Unobserved),
        "unknown_cells": count(BandState::Unknown),
        "observed_fraction": if bands.is_empty() {
            0.0
        } else {
            (observed + excluded) as f64 / bands.len() as f64
        },
        "rule": "collapsed over time: a frequency cell counts as sampled if ANY row in this window \
            sampled it. This is the survey question - `did this radio ever look here in this \
            window` - and never a claim about one instant: a front end sees one window at a time, \
            so a COMPLETE full-range pass covers only a thin diagonal of the (time x frequency) \
            grid. Cite this for a sentence about the survey; draw the per-cell grid.",
    })
}

/// The strip's shading, and **the scale it was normalised against** (T-342).
///
/// The normalisation is legitimate here — it is made where the levels are known — but until T-342
/// its denominator never reached the wire, so `0.87` was a number relative to a range the client
/// could not name. `range_db` and `unit` are that range, served, which is also what lets a strip
/// share the main waterfall's scaling instead of inventing a second one.
struct StripShades {
    /// Per cell, 0–1 over `range_db`; `None` where the history keeps no level.
    values: Vec<Option<f32>>,
    /// The observed range the values are relative to; `None` when nothing was observed.
    range_db: Option<(f32, f32)>,
    /// Scale of `range_db` (`"dbfs-per-hz"` / `"dbm-per-hz"`); `None` when no history answered at
    /// all — an unnamed scale beats a guessed one.
    scale: Option<&'static str>,
    /// The pyramid tier these shades were actually read from, and its own cells (T-426).
    ///
    /// Served because the tier is no longer a function of the window alone: when the preferred
    /// (coarsest adequate) tier holds nothing and a finer one does, the finer one answers, and a
    /// fallback nobody can see is as dishonest as the empty strip it replaces. `None` only when
    /// this server has no spectrum history to read at all.
    level: Option<u8>,
    /// That tier's time cell, s; `None` with `level`.
    src_t_cell_s: Option<f64>,
    /// That tier's frequency cell, Hz; `None` with `level`.
    src_f_cell_hz: Option<f64>,
}

/// The level the spectrum history holds for each cell of the strip, normalised to the strip's own
/// observed range. `None` per cell where the history has nothing.
///
/// The fold is a **max-hold** — [`hk_store::RegionHistory::overview`] over `rows × cells`, the
/// frequency-axis twin of the timeline's band-collapsed series (T-342) — so a brief emission still
/// lights its cell instead of being averaged away. At `rows = 1` it is a max-hold over the whole
/// window, T-368's strip unchanged; at `rows > 1` it is per (time, frequency) cell, laid out
/// row-major exactly as the coverage grid is, so the two line up cell for cell.
///
/// This is only the *shading*: it never decides observed-versus-unobserved. A cell the radio
/// demonstrably sampled but whose level the pyramid no longer keeps is observed with no shade, and
/// a cell the pyramid happens to hold a value for is still grey if no tune ever covered it.
fn shades(
    state: &ApiState,
    freq: FreqRange,
    window: TimeRange,
    rows: usize,
    cells: usize,
) -> StripShades {
    let region = crate::query::Region {
        freq,
        t0_ns: window.start.as_unix_nanos(),
        t1_ns: window.end.as_unix_nanos(),
    };
    crate::http::with_history(state, |p| {
        crate::query::overview_read(p, &region, rows, cells)
    })
    .map(|r| {
        let o = r.grid;
        StripShades {
            values: o
                .cells
                .iter()
                .map(|c| match (o.range_db, c.observed()) {
                    (Some((lo, hi)), true) if c.max_db.is_finite() => {
                        let span = if hi > lo { hi - lo } else { 1.0 };
                        Some(((c.max_db - lo) / span).clamp(0.0, 1.0))
                    }
                    _ => None,
                })
                .collect(),
            range_db: o.range_db,
            scale: Some(crate::query::scale_str(o.unit)),
            level: Some(r.level),
            src_t_cell_s: Some(r.src_t_cell_s),
            src_f_cell_hz: Some(r.src_f_cell_hz),
        }
    })
    .unwrap_or_else(|_| StripShades {
        values: vec![None; rows * cells],
        range_db: None,
        scale: None,
        level: None,
        src_t_cell_s: None,
        src_f_cell_hz: None,
    })
}

/// `GET /api/coverage?f_lo&f_hi[&cells][&rows][&t0&t1]`.
pub fn coverage_json(state: &ApiState, q: &Params) -> Result<Value, ApiError> {
    const ALLOWED: [&str; 7] = ["f_lo", "f_hi", "cells", "rows", "t0", "t1", "token"];
    if let Some((k, _)) = q.iter().find(|(k, _)| !ALLOWED.contains(&k.as_str())) {
        return Err(ApiError::new(
            400,
            format!("unknown parameter {k:?} (allowed: f_lo, f_hi, cells, rows, t0, t1)"),
        ));
    }
    let cells = count(q, "cells", DEFAULT_CELLS, MAX_COVERAGE_CELLS)?;
    let rows = count(q, "rows", DEFAULT_ROWS, MAX_COVERAGE_ROWS)?;
    let freq = parse_freq_only(q)?
        .ok_or_else(|| ApiError::new(400, "f_lo and f_hi are required, in Hz"))?;
    let (window, window_source) = window_of(state, q)?;

    let evidence = Evidence::collect(state, freq, window);
    let spans = &evidence.spans;

    // The grid T-421 built, asked for with the time axis kept. `rows = 1` is T-368's column.
    let any = hk_store::coverage::union_grid_over(spans, freq, window, rows, cells);
    // The realised row count: `grid_over` reduces the time axis rather than the frequency one when
    // the product exceeds its cap, and says which `nt` it built. Everything below is laid out on
    // that, not on what was asked for.
    let nt = any.nt;
    let shades = shades(state, freq, window, nt, cells);
    let unknown_rows = evidence.unknown_rows(&any);
    let devices: Vec<Value> = hk_store::coverage::by_device_over(spans, freq, window, nt, cells)
        .iter()
        .map(|g| grid_json(g, Some(&shades.values), evidence.unknown_rows(g)))
        .collect();

    let source = crate::navigation::live_window_verdict(
        freq.width_hz(),
        crate::http::max_live_span_hz(state),
    );
    Ok(json!({
        "region": { "lo_hz": freq.lo_hz, "hi_hz": freq.hi_hz },
        "window": {
            "t0_s": window.start.as_unix_nanos() as f64 * 1e-9,
            "t1_s": window.end.as_unix_nanos() as f64 * 1e-9,
            "span_s": window.duration_ns() as f64 * 1e-9,
            // Where the window came from: the caller, or the capture window this server holds.
            "source": window_source,
        },
        // The grid every `cells` array is laid out on: **row-major, earliest row first, low
        // frequency first**. `rows` is the realised time axis (T-423) — `rows = 1` is T-368's
        // single column over the whole window, and is what a caller that names no `rows` gets.
        "grid": {
            "cells": cells,
            "rows": nt,
            "requested_rows": rows,
            "f_lo_hz": any.f_lo_hz,
            "f_cell_hz": any.f_cell_hz,
            "t0_s": any.window.start.as_unix_nanos() as f64 * 1e-9,
            "t_cell_s": any.t_cell_ns as f64 * 1e-9,
            "order": "row-major: cells[t * cells + f], earliest row first, low frequency first",
        },
        // One entry per front end that actually sampled here, never merged. Empty means nothing on
        // this server can say what was sampled — which is **not** the same as "nothing was".
        "devices": devices,
        // The deliberate union, labelled `"any"` so it can never be mistaken for one radio's
        // coverage (T-259/T-305: device-local physics reads the device).
        "any": grid_json(&any, Some(&shades.values), unknown_rows),
        // Where this server's memory of *whether it looked* runs out (`docs/16` §5.4, T-423).
        "horizon": evidence.horizon_json(&any),
        // What a `shade` **is** (T-342): the fold that produced it, the scale it is relative to,
        // and the range that scale spans. Before this block a shade was a 0–1 number normalised
        // against a range the response never named, so the same energy could read as two
        // different strengths beside a waterfall drawn on a different scale.
        "shade": {
            "fold": "max-hold",
            "rule": crate::query::MAX_HOLD_RULE,
            "statistic": if nt > 1 {
                "max-hold over each time row, per (time, frequency) cell"
            } else {
                "max-hold over the whole window, per frequency cell"
            },
            "scale": shades.scale,
            "range_db": shades.range_db.map(|(lo, hi)| json!({"lo": lo, "hi": hi})),
            "normalisation": "0 at `range_db.lo`, 1 at `range_db.hi`, linear in dB and clamped",
            // Which pyramid tier drew the shading, and that tier's own cells (T-426). The tier is
            // not a function of the window alone: the coarsest tier whose cells fit one drawn
            // column is *preferred*, but when it holds nothing a populated finer tier answers
            // instead — more resolution than was asked for, never less. Serving the level is what
            // keeps that fallback honest; the same three fields appear in `/api/timeline`'s
            // `resolution`, from the same read.
            "level": shades.level,
            "src_t_cell_s": shades.src_t_cell_s,
            "src_f_cell_hz": shades.src_f_cell_hz,
            "level_rule": "the tier that ACTUALLY answered: the coarsest tier whose time cells are \
                no larger than one drawn cell is preferred, and a populated finer tier answers \
                when it is empty — so `src_t_cell_s` smaller than the drawn cell is more \
                resolution than asked for, folded down, and never invented detail",
            // The constraint the survey strip shares with the timeline's series: folding many
            // time cells into one column must never turn a never-observed cell into an observed
            // one. An unobserved cell carries no `shade` key at all, which is stronger than a
            // null — there is no field to misread as the bottom of the ramp.
            "unobserved": "an unobserved cell carries no `shade` key: the max of nothing is \
                unknown, not zero, and not the bottom of the scale",
        },
        // Which tune histories answered, how many of each one's spans actually named the radio,
        // and whether every span it contributed did (see `Evidence::sources_json`).
        "sources": evidence.sources_json(),
        "resolution": {
            "source": source.as_str(),
            "live": source.is_live(),
            "statement": source.statement(),
            "served_span_hz": freq.width_hz(),
            "max_live_span_hz": crate::http::max_live_span_hz(state),
            // Grey is decided here and nowhere else — and `"unknown"` is not grey. Forgetting
            // whether we looked is a different claim from having looked and found nothing, and
            // collapsing the two is the dishonesty this route exists to refuse (`docs/16` §5.4).
            "grey_rule": "grey a cell if and only if its state is \"unobserved\"; \"unknown\" is \
                not grey and not a level — draw it as a fourth thing (hatching, per T-413); \
                \"excluded\" (T-595) is spectrum the radio DID sample and the analysis skipped — \
                draw the measurement, mark it distinctly, never grey",
            // And the shade is measured here and nowhere else: what it means, and against what,
            // is the `shade` block above (T-342).
            "shade_rule": "shade is the max-hold over the window, normalised over `shade.range_db`; \
                it never decides observed-versus-unobserved",
        },
    }))
}

/// The record-derived coverage plane for a grid someone else drew — `/api/timeline`'s overlay
/// (T-423, `docs/16` §7 step 2).
///
/// # Why the timeline needs this when its grid already has a `coverage` array
///
/// They answer different questions, and only one of them decides grey.
///
/// - `grid.coverage` is **frame** coverage: the fraction of the drawn cell for which the tiered
///   spectrum-history pyramid still holds frames. It is `0` both where the radio never looked and
///   where the pyramid's byte budget evicted what it saw — and those are not the same thing.
/// - `coverage.*` here is **record**-derived: the fraction of the cell the front end was actually
///   **tuned to**, read from the IQ ring journal and the observation log. A cell with no frames but
///   a covering tune record was *sampled, level not retained*; a cell with neither was never looked
///   at; a cell with neither, before the record horizon, is `"unknown"`.
///
/// The grid is laid out on exactly the same axes as `grid` — `nt = columns` time rows, `nf = rows`
/// frequency cells, row-major — so a client indexes one with the other's index. When the cap in
/// [`hk_store::coverage::grid_over`] reduces the time axis, `grid.nt`/`grid.nf` here say what was
/// realised and `aligned` says whether it still matches; a realised grid is never implied.
pub(crate) fn overlay_json(
    state: &ApiState,
    freq: FreqRange,
    window: TimeRange,
    columns: usize,
    rows: usize,
) -> Value {
    let evidence = Evidence::collect(state, freq, window);
    let any = hk_store::coverage::union_grid_over(&evidence.spans, freq, window, columns, rows);
    let devices: Vec<Value> =
        hk_store::coverage::by_device_over(&evidence.spans, freq, window, any.nt, any.nf)
            .iter()
            // No shades: the timeline carries its own levels in `grid.max_db`, and a `shade: null`
            // here would read as "sampled, level not retained" — a claim this block is not making.
            .map(|g| grid_json(g, None, evidence.unknown_rows(g)))
            .collect();
    json!({
        "grid": {
            "nt": any.nt,
            "nf": any.nf,
            "t0_s": any.window.start.as_unix_nanos() as f64 * 1e-9,
            "t_cell_s": any.t_cell_ns as f64 * 1e-9,
            "f_lo_hz": any.f_lo_hz,
            "f_cell_hz": any.f_cell_hz,
            // Whether this plane lines up cell-for-cell with `grid` above.
            "aligned": any.nt == columns && any.nf == rows,
            "order": "row-major: cells[t * nf + f], earliest row first, low frequency first — the \
                same layout as `grid`",
        },
        "devices": devices,
        "any": grid_json(&any, None, evidence.unknown_rows(&any)),
        "horizon": evidence.horizon_json(&any),
        "sources": evidence.sources_json(),
        "rule": "record-derived: whether the front end was TUNED to this cell, from the IQ ring \
            journal and the observation log. `grid.coverage` is a different measurement — the \
            fraction for which the spectrum-history pyramid still holds frames — and is 0 both \
            where nothing looked and where the budget evicted what it saw. Grey is decided here: \
            grey a cell if and only if its state is \"unobserved\" — \"excluded\" is sampled \
            spectrum the analysis skipped (T-595), never grey.",
    })
}

// ---- the compact form `/api/tiles` serves (T-467) -------------------------------------------

/// Run-length encodes a cell-state plane as a flat `[code, count, code, count, …]`.
///
/// A coverage plane is the output of rasterising **spans**, so along a row it changes state only
/// where a tuned band begins or ends: a handful of runs a row, not one entry a cell. That is why
/// the per-cell form was 146 B/cell for information that is nearly constant.
fn rle(codes: &[u8]) -> Vec<u64> {
    let mut runs: Vec<u64> = Vec::with_capacity(8);
    for &c in codes {
        let n = runs.len();
        if n >= 2 && runs[n - 2] == u64::from(c) {
            runs[n - 1] += 1;
        } else {
            runs.push(u64::from(c));
            runs.push(1);
        }
    }
    runs
}

/// One distinct coverage plane's JSON: the runs, the counts, and the uniform fast path.
///
/// **Every number here is derived from the same `codes` slice the runs are**, so the counts cannot
/// disagree with the plane they describe — the failure mode of serving a summary beside a body.
fn plane_json(codes: &[u8]) -> Value {
    let mut counts = [0usize; COVERAGE_STATES.len()];
    for &c in codes {
        counts[usize::from(c)] += 1;
    }
    let observed = counts[usize::from(OBSERVED)];
    let uniform = codes
        .first()
        .filter(|&&c| counts[usize::from(c)] == codes.len())
        .map(|&c| COVERAGE_STATES[usize::from(c)]);
    json!({
        "runs": rle(codes),
        "cells": codes.len(),
        // The whole plane in one word when it has one, so a caller need not expand the runs to
        // learn it — and `null` when it has not. Derived from the runs, never asserted beside them.
        "uniform": uniform,
        "observed_cells": observed,
        "unobserved_cells": counts[usize::from(UNOBSERVED)],
        "unknown_cells": counts[usize::from(UNKNOWN)],
        // T-595. Not in `observed_cells`: on this compact form each cell carries exactly one code,
        // and `excluded` is its own code. `observed_cells + excluded_cells` is the sampled total.
        "excluded_cells": counts[usize::from(EXCLUDED)],
        // The fraction the radio SAMPLED, so an exclusion inside a tuned band never reads as a
        // hole in the survey: `"excluded"` cells count here exactly as `"observed"` ones do, and
        // `CoverageGrid::observed_fraction` — the per-cell form's source — does the same (T-595).
        "observed_fraction": if codes.is_empty() { 0.0 } else { (observed + counts[usize::from(EXCLUDED)]) as f64 / codes.len() as f64 },
    })
}

/// Which plane answers for the selected device, by the route's own selection rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Selected {
    /// This plane index in [`TileOverlay::planes`] decides the tile's grey.
    Plane(usize),
    /// A **named** front end with no plane in this answer: it recorded nothing over this tile.
    /// That is a coverage answer — unobserved for that device — not a missing one.
    AbsentDevice,
}

/// `/api/tiles`'s coverage overlay: computed **once**, asked once for the short-circuit, and
/// serialised once — compactly (T-467, T-461).
///
/// # Why this is not [`overlay_json`]
///
/// [`overlay_json`] serves one JSON object per cell — `{"state":"observed","duty":…,"last_s":…,
/// "center_hz":…,"sample_rate_hz":…,"spans":…,"observed_s":…}`, about 146 B. On a 256 × 256 tile
/// that is 9.5 MB **per plane**, and the answer carried two of them: `any` and, on a one-device
/// server, a `devices[0]` holding the identical bytes. Measured on the demo backend, that was 99 %
/// of a 19.34 MB tile body, of which the client reads one field — `state`.
///
/// So this form carries the **state and nothing else**, run-length encoded, and carries each
/// *distinct* plane exactly once with `any` and each device naming the plane that is theirs. The
/// per-cell sampling metadata is still served, per cell, by `/api/coverage` — which is the right
/// place for it: it is a hover question about one cell, not a property of every cell of every tile.
///
/// # The honesty boundary, under compression
///
/// Three codes, never two. `unobserved` is a code of its own, `unknown` is a code of its own, and
/// neither carries any measurement key — there is no field on this wire a client could read as a
/// zero level, which is *stronger* than the per-cell form's absence of keys, not weaker. The
/// alphabet is served with the planes ([`COVERAGE_STATES`]) so a code is never read against an
/// alphabet the answer did not state, and `hk-api`'s own tests assert code-for-code that this form
/// and [`cell_json`] classify every cell identically.
pub(crate) struct TileOverlay {
    evidence: Evidence,
    /// `any`'s grid **shape** (T-579): window, rows, cells and device, with `cells` left empty.
    /// Everything this overlay serves is the plane codes; the per-cell `Coverage` values are ~4 MB
    /// a 256 × 256 grid and are dropped as soon as the codes exist, which is what lets the raster
    /// be memoised at all ([`CoverageRasterMemo`]).
    any: CoverageGrid,
    /// Each device's grid shape, `cells` empty, as for `any`.
    devices: Vec<CoverageGrid>,
    /// The **distinct** planes, in first-seen order. An identical plane is never repeated: that is
    /// the duplicate `any`/`devices[0]` this ticket was filed about, removed by construction rather
    /// than by a special case for one-device servers.
    planes: Vec<Vec<u8>>,
    /// `any`'s plane.
    any_plane: usize,
    /// Each device's plane, parallel to `devices`.
    device_planes: Vec<usize>,
    nt_asked: usize,
    nf_asked: usize,
}

impl TileOverlay {
    /// Reads both tune histories over `freq × window` and rasterises the union and every device's
    /// plane onto an `nt × nf` grid.
    ///
    /// **The rasterisation is memoised** (T-579) in `state.coverage_raster`, keyed on the tune
    /// history itself — every span the read returned, clipped to this window — so it is redone
    /// exactly when what the histories say about this tile changes, and never on a timer. The
    /// horizon (`"unknown"` rows) is applied per call on top of the cached codes, because it moves
    /// with the ring's eviction independently of the spans. See [`CoverageRasterMemo`].
    ///
    /// This is the path for a **tile address** — `GET /api/tiles` and every member of
    /// `/api/tiles/batch` (which answers each address through the single-tile route), where the
    /// same `freq × window` recurs on every poll. A window that never recurs goes through
    /// [`Self::collect_once`].
    pub(crate) fn collect(
        state: &ApiState,
        freq: FreqRange,
        window: TimeRange,
        nt: usize,
        nf: usize,
    ) -> Self {
        Self::collect_with(state, freq, window, nt, nf, true)
    }

    /// [`Self::collect`] without the memo: the same evidence, the same rasterisation, the same
    /// horizon rule — only the cached raster is neither consulted nor filled.
    ///
    /// For windows that are asked about **once**: the `/ws/tiles/rows` feed (T-468) walks a
    /// cursor forward row block by row block, so each window it rasterises is new and is never
    /// asked again. Memoising those would be pure cost on the live path — an insertion and, once
    /// the memo is full, an LRU scan per block — and each would evict a tile raster the next poll
    /// does reuse (T-579 rebuild).
    pub(crate) fn collect_once(
        state: &ApiState,
        freq: FreqRange,
        window: TimeRange,
        nt: usize,
        nf: usize,
    ) -> Self {
        Self::collect_with(state, freq, window, nt, nf, false)
    }

    fn collect_with(
        state: &ApiState,
        freq: FreqRange,
        window: TimeRange,
        nt: usize,
        nf: usize,
        memo: bool,
    ) -> Self {
        let evidence = Evidence::collect(state, freq, window);
        let raster = if memo {
            state
                .coverage_raster
                .raster(&evidence.spans, freq, window, nt, nf)
        } else {
            std::sync::Arc::new(rasterise(&evidence.spans, freq, window, nt, nf))
        };
        let any = raster.any.clone();
        let devices = raster.devices.clone();
        let codes_of = |g: &CoverageGrid, runs: &[(u8, u32)]| -> Vec<u8> {
            let mut codes = expand_runs(runs);
            // `state_code`'s horizon rule, applied to the cached codes: only an `unobserved` cell
            // becomes `unknown` past the horizon; an observed or excluded one is its own proof.
            for r in evidence.unknown_rows(g) {
                let row = &mut codes[r * g.nf..((r + 1) * g.nf).min(g.nt * g.nf)];
                for c in row.iter_mut().filter(|c| **c == UNOBSERVED) {
                    *c = UNKNOWN;
                }
            }
            codes
        };
        let mut planes: Vec<Vec<u8>> = Vec::new();
        let mut intern = |codes: Vec<u8>| -> usize {
            match planes.iter().position(|p| *p == codes) {
                Some(i) => i,
                None => {
                    planes.push(codes);
                    planes.len() - 1
                }
            }
        };
        let any_plane = intern(codes_of(&any, &raster.any_runs));
        let device_planes: Vec<usize> = devices
            .iter()
            .zip(&raster.device_runs)
            .map(|(g, runs)| intern(codes_of(g, runs)))
            .collect();
        Self {
            evidence,
            any,
            devices,
            planes,
            any_plane,
            device_planes,
            nt_asked: nt,
            nf_asked: nf,
        }
    }

    /// The plane that decides this tile's grey, by the same rule the response states and the
    /// renderer follows: a **named** device is that front end's own plane and never the union
    /// (T-259/T-305), `any` is the union.
    pub(crate) fn selected(&self, device: &str) -> Selected {
        if device == "any" {
            return Selected::Plane(self.any_plane);
        }
        match self
            .devices
            .iter()
            .position(|g| g.device.as_str() == device)
        {
            Some(i) => Selected::Plane(self.device_planes[i]),
            None => Selected::AbsentDevice,
        }
    }

    /// How far forward this overlay's evidence reaches, Unix ns — the `as_of_s` it serves
    /// ([`Evidence::newest_record`]).
    pub(crate) fn as_of_ns(&self) -> Option<i64> {
        self.evidence.newest_record.map(Timestamp::as_unix_nanos)
    }

    /// Per cell of the asked-for grid (row-major, `[t * nf + f]`), whether the **selected** plane
    /// says `unobserved` — the cells a last-known value may be drawn over (T-881). `None` when the
    /// plane is not laid cell-for-cell on the asked-for grid, so a caller never reads one grid's
    /// cells through another's addressing. A named device with no plane here is unobserved
    /// everywhere, as [`Self::uniform_state`] says.
    pub(crate) fn unobserved_mask(&self, device: &str) -> Option<Vec<bool>> {
        let (nt, nf) = (self.nt_asked, self.nf_asked);
        if self.any.nt != nt || self.any.nf != nf {
            return None;
        }
        match self.selected(device) {
            Selected::AbsentDevice => Some(vec![true; nt * nf]),
            Selected::Plane(i) => {
                let codes = &self.planes[i];
                (codes.len() == nt * nf).then(|| codes.iter().map(|&c| c == UNOBSERVED).collect())
            }
        }
    }

    /// The one state every cell of the selected plane is in, or `None` when the plane is mixed.
    ///
    /// **This is the same `Vec<u8>` [`Self::to_json`] serialises**, not a second computation over
    /// the same spans — which is the point: a short-circuit that consulted its own opinion of what
    /// was observed would be exactly the drift this milestone spent weeks closing.
    ///
    /// A named device with no plane here is uniformly `unobserved` *for that device*, which is what
    /// the answer already tells the client (`selected.present = false`) and what the client already
    /// draws.
    pub(crate) fn uniform_state(&self, device: &str) -> Option<&'static str> {
        let codes = match self.selected(device) {
            Selected::AbsentDevice => return Some(COVERAGE_STATES[usize::from(UNOBSERVED)]),
            Selected::Plane(i) => &self.planes[i],
        };
        let first = *codes.first()?;
        codes
            .iter()
            .all(|&c| c == first)
            .then(|| COVERAGE_STATES[usize::from(first)])
    }

    /// The **selected** plane alone, in the same `plane_json` form `planes[i]` takes, with the
    /// alphabet and grid it is laid on — what `/ws/tiles/rows` (T-468) sends beside each block of
    /// rows. A named device with no plane here is uniformly `unobserved` *for that device*, the
    /// same answer [`Self::uniform_state`] gives, spelled as a plane rather than omitted.
    pub(crate) fn selected_plane_json(&self, device: &str) -> Value {
        let absent;
        let codes: &[u8] = match self.selected(device) {
            Selected::Plane(i) => &self.planes[i],
            Selected::AbsentDevice => {
                absent = vec![UNOBSERVED; self.any.nt * self.any.nf];
                &absent
            }
        };
        json!({
            "encoding": "plane-rle",
            "states": COVERAGE_STATES,
            "nt": self.any.nt,
            "nf": self.any.nf,
            "aligned": self.any.nt == self.nt_asked && self.any.nf == self.nf_asked,
            "present": self.selected(device) != Selected::AbsentDevice,
            "plane": plane_json(codes),
        })
    }

    /// The compact `coverage` block.
    pub(crate) fn to_json(&self, device: &str, named: bool) -> Value {
        let g = &self.any;
        let selected = self.selected(device);
        let plane_of = |s: Selected| match s {
            Selected::Plane(i) => json!(i),
            Selected::AbsentDevice => Value::Null,
        };
        json!({
            // Named so a client can refuse an encoding it does not know rather than guess at one.
            "encoding": "plane-table-rle",
            "grid": {
                "nt": g.nt,
                "nf": g.nf,
                "t0_s": g.window.start.as_unix_nanos() as f64 * 1e-9,
                "t_cell_s": g.t_cell_ns as f64 * 1e-9,
                "f_lo_hz": g.f_lo_hz,
                "f_cell_hz": g.f_cell_hz,
                // Whether this plane lines up cell-for-cell with `grid` above.
                "aligned": g.nt == self.nt_asked && g.nf == self.nf_asked,
                "order": "row-major: cells[t * nf + f], earliest row first, low frequency first — \
                    the same layout as `grid`",
            },
            // Code -> state name. Served WITH the planes, so a code is never read against an
            // alphabet the answer did not state.
            "states": COVERAGE_STATES,
            // Every DISTINCT plane, once. `any` and each device name the one that is theirs, so a
            // device whose coverage happens to equal the union costs an index rather than a copy.
            "planes": self.planes.iter().map(|c| plane_json(c)).collect::<Vec<_>>(),
            "any": { "device": "any", "named": false, "plane": self.any_plane },
            "devices": self
                .devices
                .iter()
                .zip(&self.device_planes)
                .map(|(g, &p)| json!({
                    "device": g.device.as_str(),
                    "named": g.device.is_named(),
                    "plane": p,
                }))
                .collect::<Vec<_>>(),
            "selected": {
                "device": device,
                "named": named,
                // A named device with no plane here is a front end that recorded nothing over this
                // tile — a coverage answer, not a missing one, and saying which it is keeps "we
                // have no record" from being read as "it never looked".
                "present": selected != Selected::AbsentDevice,
                "plane": plane_of(selected),
                "rule": "grey a cell of this tile if and only if the selected plane's state is \
                    \"unobserved\" - NOT \"excluded\", which is sampled spectrum the analysis \
                    skipped (T-595) and whose level must still be drawn. `any` is the union; a \
                    named device is that front end alone, and never the union.",
            },
            "horizon": self.evidence.horizon_json(&self.any),
            "sources": self.evidence.sources_json(),
            "rule": "record-derived: whether the front end was TUNED to this cell, from the IQ ring \
                journal and the observation log. `grid.coverage` is a different measurement — the \
                fraction for which the spectrum-history pyramid still holds frames — and is 0 both \
                where nothing looked and where the budget evicted what it saw. Grey is decided \
                here: grey a cell if and only if its state is \"unobserved\" — \"excluded\" is \
                sampled spectrum the analysis skipped (T-595), never grey.",
            "encoding_rule": "`planes[i].runs` is a flat [code, count, code, count, …] run-length \
                encoding of that plane's cells in `grid.order`; the counts sum to `planes[i].cells` \
                and each code indexes `states`. FOUR states, never two: \"unobserved\" (nothing \
                looked), \"unknown\" (we no longer know whether we looked, T-423) and \
                \"excluded\" (sampled, deliberately left out of analysis - the DC notch, T-595) \
                are separate codes and none of them is \"observed\". No cell on this plane carries a measurement key \
                of any kind, so there is nothing here a client can read as a level of zero — the \
                measurement plane is `grid`, and it is separate on purpose.",
            "per_cell_metadata": "the per-cell sampling detail (`duty`, `observed_s`, `last_s`, \
                `spans`, `center_hz`, `sample_rate_hz`) is NOT carried here (T-467): at ~146 B a \
                cell it was 99 % of a tile's body to serve a field no renderer reads. It is a \
                question about ONE cell, and `GET /api/coverage?f_lo&f_hi&t0&t1&cells&rows` answers \
                it per cell, in the same three-state vocabulary.",
        })
    }
}

/// One rasterised tile's coverage, reduced to what [`TileOverlay`] serves (T-579).
struct Raster {
    /// The union grid's shape; `cells` is empty.
    any: CoverageGrid,
    /// Each device's grid shape, `cells` empty, in [`hk_store::coverage::by_device_over`]'s order.
    devices: Vec<CoverageGrid>,
    /// `any`'s state codes with **no** horizon applied, run-length encoded.
    any_runs: Vec<(u8, u32)>,
    /// Each device's codes, parallel to `devices`, likewise.
    device_runs: Vec<Vec<(u8, u32)>>,
}

impl Raster {
    fn runs(&self) -> usize {
        self.any_runs.len() + self.device_runs.iter().map(Vec::len).sum::<usize>()
    }
}

/// Everything a tile's coverage raster is a function of — and nothing else (T-579).
///
/// The raster ([`hk_store::coverage::union_grid_over`] / [`by_device_over`]) reads a span only
/// through its part inside `window`, its frequency extent, its analysis flag, its centre/rate and
/// its device; a span that does not reach the window still names a device, and so still decides
/// whether that device gets a plane. So the key carries each span **clipped to the window**, which
/// is what makes the key stable for a tile the live edge has passed even while the ring's newest
/// segment keeps growing — and makes it change, for a tile the live edge is inside, on every row.
///
/// Equality is on the full value, never a hash of it: a collision here would serve one tile's
/// grey for another's.
///
/// [`by_device_over`]: hk_store::coverage::by_device_over
#[derive(Clone, PartialEq, Eq, Hash)]
struct RasterKey {
    freq: (u64, u64),
    window: (i64, i64),
    nt: usize,
    nf: usize,
    spans: Vec<SpanKey>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct SpanKey {
    device: Device,
    valid: bool,
    /// The span's interval clipped to the window; `None` when it does not reach it.
    time: Option<(i64, i64)>,
    freq: (u64, u64),
    analysed: bool,
    center_hz: u64,
    sample_rate_hz: u64,
}

impl RasterKey {
    fn of(
        spans: &[CoverageSpan],
        freq: FreqRange,
        window: TimeRange,
        nt: usize,
        nf: usize,
    ) -> Self {
        let (w0, w1) = (window.start.as_unix_nanos(), window.end.as_unix_nanos());
        Self {
            freq: (freq.lo_hz.to_bits(), freq.hi_hz.to_bits()),
            window: (w0, w1),
            nt,
            nf,
            spans: spans
                .iter()
                .map(|s| {
                    let (t0, t1) = (
                        s.time.start.as_unix_nanos().max(w0),
                        s.time.end.as_unix_nanos().min(w1),
                    );
                    SpanKey {
                        device: s.device.clone(),
                        valid: s.is_valid(),
                        time: (t1 > t0).then_some((t0, t1)),
                        freq: (s.freq.lo_hz.to_bits(), s.freq.hi_hz.to_bits()),
                        analysed: s.analysis.is_analysed(),
                        center_hz: s.center_hz.to_bits(),
                        sample_rate_hz: s.sample_rate_hz.to_bits(),
                    }
                })
                .collect(),
        }
    }
}

/// Rasters held at most. A screen is 135–290 tile requests (the T-579 review), a few panes more.
const RASTER_MEMO_MAX_ENTRIES: usize = 2048;
/// Code runs held at most across every entry — 8 B a run, so 8 MiB. A plane is usually one run (a
/// grey tile) or a handful (a tuned band's edges); this bounds the pathological striped one.
const RASTER_MEMO_MAX_RUNS: usize = 1 << 20;

/// **The coverage raster, memoised against the tune history** (T-579).
///
/// Every `GET /api/tiles` decides grey from [`TileOverlay`], which rasterised a 256 × 256 grid
/// per plane from scratch on every request — at 135–290 requests a screen, the same pure
/// computation hundreds of times, for tiles whose tune history had not changed since the last
/// poll.
///
/// # What invalidates an entry, and why it is not a version counter or a timer
///
/// The key ([`RasterKey`]) **is** the tune-history evidence the raster is computed from: every
/// span both histories (and the open dwell) returned for this tile, clipped to its window. So an
/// entry is reused only when the history says exactly the same thing about this tile, and a
/// history change that reaches the tile — a new dwell sealed, a segment evicted, the live edge
/// growing into it — is a different key and is rasterised exactly once. A change that does not
/// reach the tile (the ring's newest segment growing past the tile's end) leaves its key alone,
/// which a store-wide version counter could not do: it would invalidate every past tile on every
/// arriving row. The reads that produce the spans still run per request; that is the price of the
/// key being the evidence itself rather than a guess about it, and it is what makes a stale grey
/// impossible by construction rather than improbable by timing.
///
/// The `"unknown"` horizon is **not** in the key: it moves with the ring's eviction independently
/// of the spans, so [`TileOverlay::collect`] applies it to the cached codes on every call.
#[derive(Default)]
pub struct CoverageRasterMemo {
    inner: std::sync::Mutex<RasterMemoInner>,
}

#[derive(Default)]
struct RasterMemoInner {
    map: std::collections::HashMap<RasterKey, (std::sync::Arc<Raster>, u64)>,
    clock: u64,
    runs: usize,
    rasterisations: u64,
    hits: u64,
}

impl CoverageRasterMemo {
    /// Rasterisations actually performed — misses — since this state was built.
    pub fn rasterisations(&self) -> u64 {
        self.inner.lock().map_or(0, |g| g.rasterisations)
    }

    /// Requests answered from a held raster.
    pub fn hits(&self) -> u64 {
        self.inner.lock().map_or(0, |g| g.hits)
    }

    fn raster(
        &self,
        spans: &[CoverageSpan],
        freq: FreqRange,
        window: TimeRange,
        nt: usize,
        nf: usize,
    ) -> std::sync::Arc<Raster> {
        let key = RasterKey::of(spans, freq, window, nt, nf);
        if let Ok(mut g) = self.inner.lock() {
            g.clock += 1;
            let clock = g.clock;
            if let Some((r, used)) = g.map.get_mut(&key) {
                *used = clock;
                let r = r.clone();
                g.hits += 1;
                return r;
            }
        }
        // Computed outside the lock: two concurrent misses on one key both rasterise, and both
        // produce the same answer, which is cheaper than serialising every tile behind one.
        let r = std::sync::Arc::new(rasterise(spans, freq, window, nt, nf));
        if let Ok(mut g) = self.inner.lock() {
            g.rasterisations += 1;
            let runs = r.runs();
            if runs <= RASTER_MEMO_MAX_RUNS {
                g.clock += 1;
                let clock = g.clock;
                if let Some((old, _)) = g.map.insert(key, (r.clone(), clock)) {
                    g.runs -= old.runs();
                }
                g.runs += runs;
                while g.map.len() > RASTER_MEMO_MAX_ENTRIES || g.runs > RASTER_MEMO_MAX_RUNS {
                    let Some(lru) = g
                        .map
                        .iter()
                        .min_by_key(|(_, (_, used))| *used)
                        .map(|(k, _)| k.clone())
                    else {
                        break;
                    };
                    if let Some((old, _)) = g.map.remove(&lru) {
                        g.runs -= old.runs();
                    }
                }
            }
        }
        r
    }
}

/// The rasterisation itself: the union and every device's grid, reduced to shape + codes.
fn rasterise(
    spans: &[CoverageSpan],
    freq: FreqRange,
    window: TimeRange,
    nt: usize,
    nf: usize,
) -> Raster {
    let mut any = hk_store::coverage::union_grid_over(spans, freq, window, nt, nf);
    let mut devices = hk_store::coverage::by_device_over(spans, freq, window, any.nt, any.nf);
    let any_runs = run_length(&state_codes(&any, 0..0));
    let device_runs = devices
        .iter()
        .map(|g| run_length(&state_codes(g, 0..0)))
        .collect();
    any.cells = Vec::new();
    for g in &mut devices {
        g.cells = Vec::new();
    }
    Raster {
        any,
        devices,
        any_runs,
        device_runs,
    }
}

fn run_length(codes: &[u8]) -> Vec<(u8, u32)> {
    let mut out: Vec<(u8, u32)> = Vec::new();
    for &c in codes {
        match out.last_mut() {
            Some((k, n)) if *k == c => *n += 1,
            _ => out.push((c, 1)),
        }
    }
    out
}

fn expand_runs(runs: &[(u8, u32)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(runs.iter().map(|&(_, n)| n as usize).sum());
    for &(c, n) in runs {
        out.extend(std::iter::repeat_n(c, n as usize));
    }
    out
}

/// One grid's per-cell state codes, row-major, in exactly [`grid_json`]'s cell order.
fn state_codes(g: &CoverageGrid, unknown_rows: std::ops::Range<usize>) -> Vec<u8> {
    g.cells
        .iter()
        .enumerate()
        .map(|(i, c)| state_code(c, g.nf > 0 && unknown_rows.contains(&(i / g.nf))))
        .collect()
}

/// The window: the caller's `t0`/`t1` when both are given, else the capture window this server
/// holds. An answer with no window at all is refused rather than defaulted to a plausible span.
fn window_of(state: &ApiState, q: &Params) -> Result<(TimeRange, &'static str), ApiError> {
    let num = |k: &str| -> Result<Option<f64>, ApiError> {
        match q.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str()) {
            None => Ok(None),
            Some(raw) => raw
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
                .map(Some)
                .ok_or_else(|| ApiError::new(400, format!("{k} must be a finite number"))),
        }
    };
    match (num("t0")?, num("t1")?) {
        (Some(a), Some(b)) if b > a => Ok((
            TimeRange::new(
                Timestamp::from_unix_nanos((a * 1e9).round() as i64),
                Timestamp::from_unix_nanos((b * 1e9).round() as i64),
            ),
            "requested",
        )),
        (None, None) => {
            let (w, _) = crate::timeline::capture_window(state);
            let (t0, t1) = w.band().ok_or_else(|| {
                ApiError::new(
                    404,
                    "no capture window on this server; give t0 and t1 to ask for one",
                )
            })?;
            Ok((
                TimeRange::new(
                    Timestamp::from_unix_nanos((t0 * 1e9).round() as i64),
                    Timestamp::from_unix_nanos((t1 * 1e9).round() as i64),
                ),
                "capture-window",
            ))
        }
        _ => Err(ApiError::new(
            400,
            "t0 and t1 must be given together, t1 > t0",
        )),
    }
}

#[cfg(test)]
mod tests {
    use hk_model::attention::observation::{ObservationRecord, ObservedWindow};

    use super::*;

    fn t(s: i64) -> Timestamp {
        Timestamp::from_unix_nanos(s * 1_000_000_000)
    }

    /// The front end this run is using, so the migration control has a device available to
    /// wrongly acquire.
    const RUNNING: &str = "hackrf:0000000000000000a06063c8234e925f";
    /// A second front end, covering somewhere else entirely.
    const OTHER: &str = "rtl-sdr:00000001";

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "hk-coverage-{tag}-{}-{:?}",
                std::process::id(),
                std::time::Instant::now()
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn band() -> FreqRange {
        // 100 MHz wide, so a 10-cell grid has 10 MHz cells.
        FreqRange::new(100e6, 200e6)
    }

    fn observation_window() -> TimeRange {
        TimeRange::new(t(1000), t(1060))
    }

    /// One dwell over `lo..hi` for the whole window, observed by `device` (or by nobody).
    fn dwell_over(device: Option<&str>, lo: f64, hi: f64) -> ObservationRecord {
        use hk_model::attention::baseline::SiteKey;
        use hk_model::attention::observation::{DwellRecord, Reason, Tier};
        let reason = Reason::RegionDwell { hop: 0 };
        ObservationRecord::Dwell(DwellRecord {
            schema: hk_model::attention::ATTENTION_SCHEMA_VERSION,
            survey_id: None,
            seq: 1,
            plan_version: 1,
            site: SiteKey::Unassigned,
            device_id: device.map(str::to_string),
            reason,
            tier: Tier::ScheduledPlan,
            window: ObservedWindow {
                center_hz: (lo + hi) / 2.0,
                sample_rate_hz: hi - lo,
                usable: FreqRange::new(lo, hi),
                dc_excluded: None,
                rbw_hz: 1e3,
            },
            rf_path: 0,
            planned: observation_window(),
            observed: observation_window(),
            preempted: false,
            dropped_samples: 0,
            overload: false,
            provenance_ref: None,
        })
    }

    fn store_of(dir: &TempDir, records: &[ObservationRecord]) -> ObservationStore {
        let s = ObservationStore::open(hk_store::observation::ObservationLogConfig::new(
            dir.0.join("observations"),
        ))
        .unwrap();
        for r in records {
            s.append(r);
        }
        s.flush();
        s
    }

    /// **T-1055: the coverage read PAGES the observation log, and says so when it still cannot
    /// reach the end.**
    ///
    /// Until T-1055 the read was one page of [`MAX_RECORD_LIMIT`] (10 000) records with no paging.
    /// A full `Scan everything` pass writes one record per step — ~418 for a coarse 1 MHz–6 GHz
    /// pass — so a window holding a couple of dozen passes hit the limit, and the log answers
    /// **oldest-first**: what was dropped was the newest records, the ones that say where the radio
    /// looked most recently. `/api/coverage` then served `unobserved` over bands it had a record
    /// for, and `bands` (T-964) under-counted with it.
    ///
    /// The page size and budget are constants, so this drives [`observation_spans_pages`] with a
    /// two-record page: the walk is the behaviour under test, not the size of the constants.
    ///
    /// RED before the fix: `pages = 2` returned the first 2 spans of 5 and reported `cut` from one
    /// page only.
    #[test]
    fn a_coverage_read_pages_the_observation_log_and_says_when_it_is_still_cut() {
        let dir = TempDir::new("paged-log");
        // Five dwells, each over its own 2 MHz of the band, so a lost record is a lost band.
        let records: Vec<ObservationRecord> = (0..5)
            .map(|i| {
                let lo = 100e6 + f64::from(i) * 2e6;
                dwell_over(Some(RUNNING), lo, lo + 2e6)
            })
            .collect();
        let store = store_of(&dir, &records);
        let (freq, window) = (FreqRange::new(100e6, 110e6), observation_window());

        // One page of two: two spans, and the answer knows it is cut.
        let (spans, _, cut) = observation_spans_pages(&store, freq, window, 2, 1);
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert!(cut, "a read that left a page unread must say so");

        // Two pages of two: FOUR spans — the paging walked — and still cut.
        let (spans, _, cut) = observation_spans_pages(&store, freq, window, 2, 2);
        assert_eq!(spans.len(), 4, "the second page was never read: {spans:?}");
        assert!(cut, "one record is still unread");

        // Enough pages: every record, and NOT cut — so `unobserved` here is evidence again.
        let (spans, named, cut) = observation_spans_pages(&store, freq, window, 2, 3);
        assert_eq!(spans.len(), 5, "{spans:?}");
        assert_eq!(named, 5, "every record named its radio");
        assert!(!cut, "the read reached the end of the log");

        // The shipped read (4 pages of 10 000) reaches the end of this log without a cut.
        let (spans, _, cut) = observation_spans_paged(&store, freq, window);
        assert_eq!(spans.len(), 5, "{spans:?}");
        assert!(!cut);
    }

    /// **The property.** Coverage over the observation log's horizon — the long one, far beyond the
    /// IQ ring's retention — answers *"did **this** front end look here"*, not merely "did
    /// anything". Two radios on disjoint ranges are two grids, each unobserved exactly where the
    /// other looked, and neither one's coverage is ever the union.
    ///
    /// (T-368's `two_devices_on_disjoint_ranges_do_not_union` shape, now driven through real
    /// observation-log records rather than hand-built spans — which is the whole of T-378: before
    /// it, both of these records produced `Device::Unknown` and this test could not be written.)
    #[test]
    fn long_horizon_coverage_names_the_front_end_that_looked_and_two_devices_do_not_union() {
        let dir = TempDir::new("two-devices");
        let store = store_of(
            &dir,
            &[
                dwell_over(Some(RUNNING), 100e6, 110e6),
                dwell_over(Some(OTHER), 190e6, 200e6),
            ],
        );
        let (spans, named) = observation_spans(&store, band(), observation_window());
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert_eq!(named, 2, "both records named their radio");

        let a = hk_store::coverage::grid(
            &spans,
            &Device::Id(RUNNING.into()),
            band(),
            observation_window(),
            10,
        );
        let b = hk_store::coverage::grid(
            &spans,
            &Device::Id(OTHER.into()),
            band(),
            observation_window(),
            10,
        );
        assert!(a.at(105e6).unwrap().is_observed(), "{a:?}");
        assert_eq!(*a.at(195e6).unwrap(), Coverage::Unobserved, "{a:?}");
        assert!(b.at(195e6).unwrap().is_observed(), "{b:?}");
        assert_eq!(*b.at(105e6).unwrap(), Coverage::Unobserved, "{b:?}");
        assert_eq!((a.observed_cells(), b.observed_cells()), (1, 1));

        // `by_device` keeps them apart and names both; the union is only what someone asked for.
        let per = hk_store::coverage::by_device(&spans, band(), observation_window(), 10);
        assert_eq!(per.len(), 2);
        assert!(per.iter().all(|g| g.device.is_named()));
        let u = hk_store::coverage::union_grid(&spans, band(), observation_window(), 10);
        assert_eq!(u.device, Device::Any);
        assert!(!u.device.is_named());
        assert_eq!(u.observed_cells(), 2);
    }

    /// **The migration control.** An observation log written *before* T-378 — the literal old
    /// bytes, hand-written below with no `device_id` key anywhere in them — still reads, and its
    /// coverage is `Device::Unknown`: evidence that *something* looked, never evidence that the
    /// radio running now did.
    ///
    /// **The mutation.** Default the missing field to the running device (`mutant` below) and the
    /// control breaks: the named grid then claims a band that front end demonstrably never tuned.
    /// That is what makes the honest assertion load-bearing rather than vacuous.
    #[test]
    fn a_pre_t378_log_reads_back_unknown_and_never_the_device_that_happens_to_be_running() {
        let dir = TempDir::new("pre-t378");
        let root = dir.0.join("observations");
        // The literal bytes of a pre-T-378 dwell over 140–160 MHz, CRC and all, written into the
        // log before this run opens it.
        let json = concat!(
            r#"{"record":"dwell","schema":1,"seq":11,"plan_version":1,"#,
            r#""site":{"kind":"unassigned"},"reason":{"code":"region-dwell","hop":0},"#,
            r#""tier":"scheduled-plan","window":{"center_hz":150000000.0,"#,
            r#""sample_rate_hz":20000000.0,"usable":{"lo_hz":140000000.0,"hi_hz":160000000.0},"#,
            r#""rbw_hz":1000.0},"rf_path":0,"#,
            r#""planned":{"start_ns":1000000000000,"end_ns":1060000000000},"#,
            r#""observed":{"start_ns":1000000000000,"end_ns":1060000000000},"#,
            r#""preempted":false,"dropped_samples":0,"overload":false}"#,
        );
        assert!(!json.contains("device_id"), "the old bytes name no device");
        let path = hk_store::observation::segment::segment_path(
            &root,
            hk_store::observation::segment::hour_of(t(1030)),
        );
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                "{:08x} {json}
",
                hk_store::observation::segment::crc32(json.as_bytes())
            ),
        )
        .unwrap();

        // The running device is in the same log, looking somewhere else — so there is a device
        // available for the old record to wrongly acquire.
        let store = store_of(&dir, &[dwell_over(Some(RUNNING), 100e6, 110e6)]);
        let (spans, named) = observation_spans(&store, band(), observation_window());
        assert_eq!(spans.len(), 2, "both records read: {spans:?}");
        assert_eq!(named, 1, "only one of them named a radio: {spans:?}");
        assert!(
            spans.iter().any(|s| s.device == Device::Unknown),
            "the pre-T-378 record must read back unknown: {spans:?}"
        );
        assert!(
            !spans
                .iter()
                .any(|s| s.device == Device::Id(RUNNING.into()) && s.freq.lo_hz == 140e6),
            "the old record must never acquire the running device: {spans:?}"
        );

        // The consequence: the running front end's own coverage says it never looked at 150 MHz.
        let honest = hk_store::coverage::grid(
            &spans,
            &Device::Id(RUNNING.into()),
            band(),
            observation_window(),
            10,
        );
        assert!(honest.at(105e6).unwrap().is_observed(), "{honest:?}");
        assert_eq!(
            *honest.at(150e6).unwrap(),
            Coverage::Unobserved,
            "an unattributed span is not this radio's coverage: {honest:?}"
        );
        // Unknown is its own device, and it did look there.
        let unknown =
            hk_store::coverage::grid(&spans, &Device::Unknown, band(), observation_window(), 10);
        assert!(unknown.at(150e6).unwrap().is_observed(), "{unknown:?}");
        assert!(!unknown.device.is_named());

        // The mutation: read a missing device as the one that happens to be running.
        let mutant: Vec<CoverageSpan> = spans
            .iter()
            .map(|s| CoverageSpan {
                device: Device::Id(RUNNING.into()),
                ..s.clone()
            })
            .collect();
        let m = hk_store::coverage::grid(
            &mutant,
            &Device::Id(RUNNING.into()),
            band(),
            observation_window(),
            10,
        );
        assert!(
            m.at(150e6).unwrap().is_observed(),
            "the mutant must claim the band this radio never tuned: {m:?}"
        );
        assert_ne!(
            honest.cells, m.cells,
            "so the honest assertion is doing work"
        );
    }

    // ---- T-467: the compact plane table `/api/tiles` serves --------------------------------

    /// Expands a served plane back to state names, the way `ui/src/surface/tile.ts` does — so the
    /// assertions below are about what a client actually reads, not about the runs.
    fn expand(v: &Value) -> Vec<String> {
        let states: Vec<String> = v["states"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_string())
            .collect();
        let runs: Vec<u64> = v["runs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_u64().unwrap())
            .collect();
        assert_eq!(runs.len() % 2, 0, "runs must be [code, count] pairs: {v}");
        let mut out = Vec::new();
        for pair in runs.chunks(2) {
            for _ in 0..pair[1] {
                out.push(states[pair[0] as usize].clone());
            }
        }
        assert_eq!(
            out.len(),
            v["cells"].as_u64().unwrap() as usize,
            "runs must cover exactly `cells`: {v}"
        );
        out
    }

    /// Pulls one plane out of a whole coverage block, with the alphabet attached.
    fn plane(cov: &Value, i: usize) -> Vec<String> {
        let mut p = cov["planes"][i].clone();
        p["states"] = cov["states"].clone();
        expand(&p)
    }

    /// **The drift guard between the two encodings.** `/api/coverage` and `/api/timeline` still
    /// serve [`cell_json`]'s per-cell form; `/api/tiles` serves [`state_code`]'s compact one. They
    /// are two spellings of one decision, and the moment they disagree about which of the three
    /// states a cell is, the compression has cost exactly what it was forbidden to cost.
    #[test]
    fn the_compact_code_and_the_per_cell_form_classify_every_cell_identically() {
        let observed = hk_store::coverage::Coverage::of(
            2,
            30_000_000_000,
            30_000_000_000,
            60_000_000_000,
            t(1030),
            1e8,
            2e6,
        );
        // Sampled, and no part of it analysed: the DC notch (T-595).
        let excluded = hk_store::coverage::Coverage::of(
            2,
            30_000_000_000,
            0,
            60_000_000_000,
            t(1030),
            1e8,
            2e6,
        );
        assert!(
            excluded.is_excluded() && excluded.is_observed(),
            "{excluded:?}"
        );
        for (c, beyond) in [
            (&Coverage::Unobserved, false),
            (&Coverage::Unobserved, true),
            (&observed, false),
            // An observed cell is NEVER relabelled past the horizon: the measurement is the proof.
            (&observed, true),
            // Nor is an excluded one: it is an observation with an exclusion on it, not an absence.
            (&excluded, false),
            (&excluded, true),
        ] {
            let per_cell = cell_json(c, None, beyond);
            let code = state_code(c, beyond);
            assert_eq!(
                per_cell["state"],
                json!(COVERAGE_STATES[usize::from(code)]),
                "the two encodings disagree for beyond_horizon={beyond}: {per_cell}"
            );
        }
        // And the alphabet really does have four entries, all distinct: a two-state alphabet is
        // the collapse this whole surface exists to refuse.
        assert_eq!(COVERAGE_STATES.len(), 4);
        assert_eq!(COVERAGE_STATES[usize::from(UNOBSERVED)], "unobserved");
        assert_eq!(COVERAGE_STATES[usize::from(OBSERVED)], "observed");
        assert_eq!(COVERAGE_STATES[usize::from(UNKNOWN)], "unknown");
        // T-595, appended: every code an older client cached keeps its meaning.
        assert_eq!(COVERAGE_STATES[usize::from(EXCLUDED)], "excluded");
        // An excluded cell keeps its measurement keys — it IS an observation — and says, in a
        // number rather than a word, why it is not simply "observed".
        let v = cell_json(&excluded, None, false);
        assert_eq!(v["state"], json!("excluded"), "{v}");
        assert_eq!(v["analysed_s"], json!(0.0), "{v}");
        assert!(
            (v["observed_s"].as_f64().unwrap() - 30.0).abs() < 1e-6,
            "an excluded cell keeps every measurement key an observed one has: {v}"
        );
        let o = cell_json(&observed, None, false);
        assert!(
            (o["analysed_s"].as_f64().unwrap() - 30.0).abs() < 1e-6,
            "{o}"
        );
    }

    /// The runs are lossless, and a run boundary is exactly a state change — never a merge.
    #[test]
    fn the_run_length_encoding_is_lossless_and_never_merges_two_states() {
        for codes in [
            vec![],
            vec![UNOBSERVED; 5],
            vec![OBSERVED, OBSERVED, UNOBSERVED, UNKNOWN, UNKNOWN, OBSERVED],
            vec![OBSERVED, EXCLUDED, EXCLUDED, OBSERVED, UNOBSERVED],
            // The pathological shape: every cell a different state from its neighbour.
            (0..40).map(|i| (i % 4) as u8).collect(),
        ] {
            let v = {
                let mut p = plane_json(&codes);
                p["states"] = json!(COVERAGE_STATES);
                p
            };
            let back: Vec<u8> = expand(&v)
                .iter()
                .map(|s| COVERAGE_STATES.iter().position(|x| x == s).unwrap() as u8)
                .collect();
            assert_eq!(back, codes, "{v}");
            // The counts are derived from the same slice the runs are, so they cannot disagree.
            let n = |c: u8| codes.iter().filter(|&&x| x == c).count();
            assert_eq!(v["observed_cells"], json!(n(OBSERVED)), "{v}");
            assert_eq!(v["unobserved_cells"], json!(n(UNOBSERVED)), "{v}");
            assert_eq!(v["unknown_cells"], json!(n(UNKNOWN)), "{v}");
            assert_eq!(v["excluded_cells"], json!(n(EXCLUDED)), "{v}");
            // `uniform` is a statement about the runs, not a claim beside them.
            let uniform = codes.first().filter(|&&c| n(c) == codes.len());
            assert_eq!(
                v["uniform"],
                uniform.map_or(Value::Null, |&c| json!(COVERAGE_STATES[usize::from(c)])),
                "{v}"
            );
        }
    }

    /// **The multi-SDR case the plane table exists for**, and the duplication it removes.
    ///
    /// Two front ends on disjoint bands are two genuinely different planes and the union is a
    /// third: three entries, each device reading its own, and a named device never reading the
    /// union. One front end whose coverage *is* the union costs **one** entry — which is the
    /// `coverage.any` / `coverage.devices[0]` duplication T-467 was filed about, gone by
    /// construction rather than by a special case.
    #[test]
    fn devices_that_differ_get_their_own_plane_and_an_identical_plane_is_never_repeated() {
        let dir = TempDir::new("planes-two");
        let store = store_of(
            &dir,
            &[
                dwell_over(Some(RUNNING), 100e6, 110e6),
                dwell_over(Some(OTHER), 190e6, 200e6),
            ],
        );
        let state = ApiState {
            observations: Some(store),
            ..ApiState::default()
        };
        let o = TileOverlay::collect(&state, band(), observation_window(), 2, 10);
        let v = o.to_json(RUNNING, true);
        assert_eq!(
            v["planes"].as_array().unwrap().len(),
            3,
            "union + two disjoint devices are three distinct planes: {v}"
        );
        let idx = |d: &str| -> usize {
            v["devices"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["device"] == json!(d))
                .unwrap_or_else(|| panic!("no plane for {d}: {v}"))["plane"]
                .as_u64()
                .unwrap() as usize
        };
        let (a, b) = (plane(&v, idx(RUNNING)), plane(&v, idx(OTHER)));
        let union = plane(&v, v["any"]["plane"].as_u64().unwrap() as usize);
        // The property, read off the decoded planes: each radio is observed where it looked and
        // unobserved where the *other* one did.
        assert_eq!(
            (&a[0], &a[9]),
            (&"observed".to_string(), &"unobserved".to_string()),
            "{a:?}"
        );
        assert_eq!(
            (&b[0], &b[9]),
            (&"unobserved".to_string(), &"observed".to_string()),
            "{b:?}"
        );
        assert_ne!(a, b, "two radios on disjoint bands must not share a plane");
        assert_ne!(
            a, union,
            "a named device must never be served the union's plane"
        );
        assert_ne!(b, union);
        assert_eq!(
            (&union[0], &union[9]),
            (&"observed".to_string(), &"observed".to_string())
        );
        // The selection the response states is the one a client applies.
        assert_eq!(v["selected"]["plane"], json!(idx(RUNNING)), "{v}");
        assert_eq!(v["selected"]["present"], json!(true), "{v}");
        // A named device this answer holds no plane for is a coverage answer, not a missing one.
        let absent = o.to_json("mock:never-ran", true);
        assert_eq!(absent["selected"]["present"], json!(false), "{absent}");
        assert_eq!(absent["selected"]["plane"], Value::Null, "{absent}");

        // ---- and now the one-device server, which is what the duplication was measured on ----
        let dir1 = TempDir::new("planes-one");
        let store1 = store_of(&dir1, &[dwell_over(Some(RUNNING), 100e6, 110e6)]);
        let state1 = ApiState {
            observations: Some(store1),
            ..ApiState::default()
        };
        let o1 = TileOverlay::collect(&state1, band(), observation_window(), 2, 10);
        let v1 = o1.to_json("any", false);
        assert_eq!(
            v1["planes"].as_array().unwrap().len(),
            1,
            "the union and the only device are the same plane, so it is carried ONCE: {v1}"
        );
        assert_eq!(v1["any"]["plane"], v1["devices"][0]["plane"], "{v1}");
        assert_eq!(
            plane(&v1, 0),
            plane(&v1, v1["devices"][0]["plane"].as_u64().unwrap() as usize),
            "sharing an index must not change what either reads"
        );
    }

    /// **The size, measured — both encodings, on the same grid, in the same process.**
    ///
    /// The demo backend measured a 256 × 256 tile's coverage at 17.3 MB across two identical
    /// planes. Here the same grid is serialised both ways and the ratio is asserted, so a
    /// regression that quietly restores the per-cell form fails rather than merely costing.
    #[test]
    fn the_compact_plane_is_orders_of_magnitude_smaller_than_the_per_cell_form_it_replaces() {
        let dir = TempDir::new("size");
        let store = store_of(&dir, &[dwell_over(Some(RUNNING), 100e6, 200e6)]);
        let state = ApiState {
            observations: Some(store),
            ..ApiState::default()
        };
        const CELLS: usize = 256;
        let o = TileOverlay::collect(&state, band(), observation_window(), CELLS, CELLS);
        let compact = serde_json::to_string(&o.to_json("any", false))
            .unwrap()
            .len();
        // What the same information cost before: `grid_json`'s per-cell objects, twice, because
        // `any` and the single device's plane were byte-identical.
        // The overlay keeps only its grid's shape (T-579), so the per-cell form is rasterised here
        // from the same evidence.
        let full = hk_store::coverage::union_grid_over(
            &o.evidence.spans,
            band(),
            observation_window(),
            CELLS,
            CELLS,
        );
        let per_cell = serde_json::to_string(&grid_json(&full, None, 0..0))
            .unwrap()
            .len();
        let before = per_cell * 2;
        eprintln!(
            "T-467 coverage plane, {CELLS}x{CELLS} cells: per-cell x2 = {before} B, compact = {compact} B ({:.0}x)",
            before as f64 / compact as f64
        );
        assert!(
            full.observed_cells() == CELLS * CELLS,
            "the fixture must fill the plane, or the comparison is about a cheaper grid"
        );
        assert!(
            compact < 8_192,
            "the compact plane must stay a few KB, not scale with cells: {compact} B"
        );
        assert!(
            before / compact > 100,
            "per-cell {before} B vs compact {compact} B is not the order this ticket is about"
        );
    }

    #[test]
    fn an_unobserved_cell_carries_no_measurement_keys_at_all() {
        // The wire form of state 3: nothing a client can read as a zero level, a zero duty or a
        // zero occupancy. Not a null measurement — no measurement.
        let v = cell_json(&Coverage::Unobserved, Some(Some(0.9)), false);
        assert_eq!(v, json!({ "state": "unobserved" }));
        assert!(v.get("duty").is_none());
        assert!(v.get("observed_s").is_none());
        assert!(v.get("shade").is_none());
    }

    #[test]
    fn an_observed_cell_states_the_sampling_that_makes_it_observed() {
        let c = hk_store::coverage::Coverage::of(
            2,
            30_000_000_000,
            30_000_000_000,
            60_000_000_000,
            t(1030),
            1e8,
            2e6,
        );
        let v = cell_json(&c, Some(None), false);
        assert_eq!(v["state"], json!("observed"));
        assert_eq!(v["spans"], json!(2));
        // Floating seconds, so compare as a number rather than by JSON equality.
        assert!(
            (v["observed_s"].as_f64().unwrap() - 30.0).abs() < 1e-9,
            "{v}"
        );
        assert_eq!(v["duty"], json!(0.5));
        assert_eq!(v["last_s"], json!(1030.0));
        // Sampled, level not retained: an explicit null on an *observed* cell, which is a
        // different thing from grey and must be drawn differently.
        assert_eq!(v["shade"], Value::Null);
        assert_ne!(
            v["state"],
            cell_json(&Coverage::Unobserved, Some(None), false)["state"]
        );
    }

    // ---- T-507: "unknown" is what was recorded and lost, never the default for a young store ----

    /// One dwell over the whole of [`band`] for `[t0, t1)` (Unix s).
    fn dwell_at(t0: i64, t1: i64) -> ObservationRecord {
        let mut r = dwell_over(Some(RUNNING), 100e6, 200e6);
        if let ObservationRecord::Dwell(d) = &mut r {
            d.planned = TimeRange::new(t(t0), t(t1));
            d.observed = d.planned;
        }
        r
    }

    /// A spectrum history that began recording at `began` (Unix s): one frame folded there.
    fn history_began_at(
        dir: &TempDir,
        began: i64,
    ) -> std::sync::Arc<std::sync::Mutex<hk_store::Pyramid>> {
        let mut p = hk_store::Pyramid::open(
            dir.0.join("history-root"),
            hk_store::history::PyramidConfig::default(),
        )
        .unwrap();
        let psd = [1e-12f32; 16];
        p.ingest(&hk_store::history::FrameInput::new(
            t(began),
            1_000_000_000,
            100e6,
            1e6,
            hk_model::PowerUnit::Dbfs,
            &psd,
        ))
        .unwrap();
        assert_eq!(p.recording_began(), Some(t(began)));
        std::sync::Arc::new(std::sync::Mutex::new(p))
    }

    /// Per-row states of `overlay_json`'s union over 10 rows × 1 cell of `window`.
    fn row_states(state: &ApiState, window: TimeRange) -> (Vec<String>, Value) {
        let v = overlay_json(state, band(), window, 10, 1);
        let rows = v["any"]["cells"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["state"].as_str().unwrap().to_string())
            .collect();
        (rows, v["horizon"].clone())
    }

    fn states(spec: &[(&str, usize)]) -> Vec<String> {
        spec.iter()
            .flat_map(|&(s, n)| std::iter::repeat_n(s.to_string(), n))
            .collect()
    }

    /// **T-507, the ticket.** The plane's answer in each state a server's memory can be in:
    ///
    /// 1. **nothing has ever recorded here** (an empty log, before the first frame): every row
    ///    `unobserved`, `recording_began_s` null. Before this ticket, every row was `"unknown"` —
    ///    the purple wall;
    /// 2. **recording began and every record since survives**: rows before recording began are
    ///    `unobserved`; the rows between it and the oldest surviving tune record — recorded, record
    ///    since lost — are `"unknown"`; the dwell's own rows `observed`; after it, `unobserved`;
    /// 3. **a source discarded records that could predate that** (the log deleted a segment): the
    ///    boundary is no floor, so every row before the oldest record is `"unknown"` — T-423's rule;
    /// 4. **no tune history on this server at all** (no ring, no log): every row `"unknown"`. The
    ///    narrow case T-441 named: this server cannot say whether it looked.
    ///
    /// Window: `[7080, 7280)` s in ten 20-s rows. The dwell is `[7200, 7260)` — whole rows 6..9,
    /// and on an hour boundary so the log's hour-granular reach is exactly its start; the history
    /// began at 7100, the start of row 1.
    #[test]
    fn unknown_is_what_was_recorded_and_lost_and_a_young_store_has_lost_nothing() {
        let window = TimeRange::new(t(7080), t(7280));

        // 1. Nothing ever recorded.
        let dir = TempDir::new("t507-never");
        let state = ApiState {
            observations: Some(store_of(&dir, &[])),
            ..ApiState::default()
        };
        let (rows, h) = row_states(&state, window);
        assert_eq!(rows, states(&[("unobserved", 10)]), "{h}");
        assert_eq!(h["recording_began_s"], Value::Null, "{h}");
        assert_eq!(h["oldest_record_s"], Value::Null, "{h}");
        assert_eq!(h["forgotten"], Value::Null, "{h}");
        assert_eq!(h["unknown_rows"], json!(0), "{h}");

        // 2. Recorded since 7100; the only surviving tune record starts at 7200.
        let dir = TempDir::new("t507-lost");
        let state = ApiState {
            observations: Some(store_of(&dir, &[dwell_at(7200, 7260)])),
            history: Some(history_began_at(&dir, 7100)),
            ..ApiState::default()
        };
        let (rows, h) = row_states(&state, window);
        assert_eq!(
            rows,
            states(&[
                ("unobserved", 1),
                ("unknown", 5),
                ("observed", 3),
                ("unobserved", 1)
            ]),
            "{h}"
        );
        assert_eq!(h["recording_began_s"], json!(7100.0), "{h}");
        assert_eq!(h["oldest_record_s"], json!(7200.0), "{h}");
        assert_eq!(h["unknown_from_row"], json!(1), "{h}");
        assert_eq!(h["unknown_rows"], json!(5), "{h}");
        assert_eq!(h["forgotten"], Value::Null, "{h}");

        // 3. The same, after the log has deleted a segment by retention.
        let dir = TempDir::new("t507-forgot");
        let log = store_of(&dir, &[dwell_at(7200, 7260)]);
        log.stats()
            .segments_deleted
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let state = ApiState {
            observations: Some(log),
            history: Some(history_began_at(&dir, 7100)),
            ..ApiState::default()
        };
        let (rows, h) = row_states(&state, window);
        assert_eq!(
            rows,
            states(&[("unknown", 6), ("observed", 3), ("unobserved", 1)]),
            "{h}"
        );
        assert!(h["forgotten"].is_string(), "{h}");
        assert_eq!(h["unknown_from_row"], json!(0), "{h}");

        // 4. No tune history at all.
        let (rows, h) = row_states(&ApiState::default(), window);
        assert_eq!(rows, states(&[("unknown", 10)]), "{h}");
    }

    /// **T-680: the dwell in flight is a tune record, so a cell does not change when it seals.**
    ///
    /// T-596 served the open dwell clipped to a record horizon computed from sealed sources only.
    /// Before the first seal that horizon is `None`, the clip dropped the open dwell entirely, and
    /// the rows it covers read `"unknown"` — *we no longer know whether we looked* — until the seal
    /// landed and turned them `"observed"`, for samples that never changed. T-680 counts the open
    /// dwell as the tune record it is: one reach among the ring's and the log's, bounded to its own
    /// start.
    ///
    /// Window `[7080, 7280)` in ten 20-s rows; the spectrum history began at 7100 (row 1). The
    /// radio has been on this band since **7200** (row 6), an hour boundary so a sealed record's
    /// hour-granular reach is exactly its start. What happened over `[7100, 7200)` has no surviving
    /// record — a seal the log never wrote. Two servers, identical but for the seal:
    ///
    /// - **A**: the tune `[7200, 7280)` is the dwell in flight, nothing sealed;
    /// - **B**: the same tune, sealed.
    ///
    /// Both read, row by row: row 0 `unobserved` (before recording began), rows 1–5 `"unknown"`
    /// (recorded, record lost — the open dwell reaches back to its own start and **no further**,
    /// so it cannot paper over forgetting before it), rows 6–9 `observed`; `oldest_record_s` 7200,
    /// `unknown_rows` 5. And both horizons agree with the plane: A's `as_of_s` is 7280, the live
    /// edge the open dwell reaches (T-532 x T-596), and `oldest_record_s` is 7200, its start.
    ///
    /// RED without T-680 (the clip, sealed-only horizon): A reads 1 `unobserved` + 9 `"unknown"`,
    /// `oldest_record_s` null — the four live-edge rows the radio is measuring right now, served as
    /// forgotten, and flipping to `observed` the instant B's seal lands.
    #[test]
    fn the_dwell_in_flight_is_a_tune_record_and_a_cell_does_not_change_when_it_seals() {
        let window = TimeRange::new(t(7080), t(7280));
        let want = states(&[("unobserved", 1), ("unknown", 5), ("observed", 4)]);

        // A: in flight.
        let dir = TempDir::new("t680-open");
        let store = store_of(&dir, &[]);
        let ObservationRecord::Dwell(open) = dwell_at(7200, 7280) else {
            unreachable!("dwell_at builds a dwell")
        };
        store.note_open_dwell(open);
        let open_state = ApiState {
            observations: Some(store),
            history: Some(history_began_at(&dir, 7100)),
            ..ApiState::default()
        };
        let (rows, h) = row_states(&open_state, window);
        assert_eq!(
            rows, want,
            "the dwell in flight is a tune record the server holds: its rows are `observed` before \
             the seal exactly as after it, and it reaches back to its own start and no further \
             (rows 1-5 stay `unknown`). {h}"
        );
        assert_eq!(h["oldest_record_s"], json!(7200.0), "{h}");
        assert_eq!(h["recording_began_s"], json!(7100.0), "{h}");
        assert_eq!(h["unknown_from_row"], json!(1), "{h}");
        assert_eq!(h["unknown_rows"], json!(5), "{h}");
        assert_eq!(h["forgotten"], Value::Null, "{h}");
        assert_eq!(
            h["as_of_s"],
            json!(7280.0),
            "the forward horizon reaches the live edge the plane claims (T-532): {h}"
        );

        // B: the seal has caught up.
        let dir2 = TempDir::new("t680-sealed");
        let sealed = ApiState {
            observations: Some(store_of(&dir2, &[dwell_at(7200, 7280)])),
            history: Some(history_began_at(&dir2, 7100)),
            ..ApiState::default()
        };
        let (sealed_rows, hs) = row_states(&sealed, window);
        assert_eq!(
            sealed_rows, rows,
            "a cell's state must not change when the seal catches up: {hs} vs {h}"
        );
        for k in [
            "oldest_record_s",
            "recording_began_s",
            "unknown_from_row",
            "unknown_rows",
            "as_of_s",
        ] {
            assert_eq!(
                hs[k], h[k],
                "`{k}` changed when the seal landed: {hs} vs {h}"
            );
        }
    }

    /// **And with no history, no ring and nothing sealed** — a fresh store whose ring was refused,
    /// the first minute of T-588's full disk — the dwell in flight is the whole of this server's
    /// memory of recording: `recording_began_s` and `oldest_record_s` are both its start, nothing
    /// is `"unknown"`, the rows before it are `unobserved` (before this server recorded anything,
    /// nothing looked) and its own rows `observed`. RED without T-680: the clip drops it (horizon
    /// `None`) and all ten rows read `unobserved` — the live edge greyed, which is T-588's defect.
    #[test]
    fn a_young_store_with_nothing_sealed_is_carried_by_the_dwell_in_flight() {
        let window = TimeRange::new(t(7080), t(7280));
        let dir = TempDir::new("t680-young");
        let store = store_of(&dir, &[]);
        let ObservationRecord::Dwell(open) = dwell_at(7200, 7280) else {
            unreachable!("dwell_at builds a dwell")
        };
        store.note_open_dwell(open);
        let state = ApiState {
            observations: Some(store),
            ..ApiState::default()
        };
        let (rows, h) = row_states(&state, window);
        assert_eq!(rows, states(&[("unobserved", 6), ("observed", 4)]), "{h}");
        assert_eq!(h["oldest_record_s"], json!(7200.0), "{h}");
        assert_eq!(h["recording_began_s"], json!(7200.0), "{h}");
        assert_eq!(h["unknown_rows"], json!(0), "{h}");
    }

    /// A ring handle whose status is whatever the case under test needs (T-640).
    struct FakeRing(Value);

    impl crate::iqbuffer::IqBufferControl for FakeRing {
        fn status(&self, _q: &crate::iqbuffer::IqBufferQuery) -> Value {
            self.0.clone()
        }
        fn clip(
            &self,
            _r: &crate::iqbuffer::ClipStart,
        ) -> Result<Value, crate::iqbuffer::IqBufferFailure> {
            unreachable!("coverage never exports a clip")
        }
    }

    /// **T-640: a REFUSED IQ ring is not an available one, and the difference is a coverage state.**
    ///
    /// `ring_available` used to be `state.iq_buffer.is_some()` — whether a *handle* is wired. A ring
    /// whose allocation was refused for lack of free space (T-588's, and the field failure mode of
    /// a portable device) still presents that handle and still answers `/api/iqbuffer`, while
    /// holding no journal and contributing zero spans. [`Evidence::unknown_rows`] reads
    /// `no_tune_history` from that flag, so such a server answered `"unobserved"` — *the radio did
    /// not look* — over rows it has **no tune record of at all**.
    ///
    /// A refused ring may not paint `unobserved` over rows no record reaches: *we cannot say* is
    /// neither `observed` nor `unobserved`, and it is `"unknown"`.
    ///
    /// Counted over ten rows × one cell, no observation log in any case, so the ring is the only
    /// source there could be:
    ///
    /// - **refused** (`enabled: false`): 10 `"unknown"`, 0 `"unobserved"`, and `sources[iq-ring]`
    ///   reports `available: false` beside its zero spans;
    /// - **allocating** (`enabled: true`, nothing buffered yet): the same — an enabled ring that is
    ///   still being laid down holds no journal;
    /// - **enabled and holding a segment**: `observed` where the segment is, `unobserved` after it,
    ///   and `available: true` — so the strictness above cannot pass by calling every ring dead.
    ///
    /// RED with the old `is_some()` predicate: the first two cases serve 10 `"unobserved"` rows and
    /// `available: true`, i.e. ten rows of "nothing looked" asserted by a ring that never opened.
    #[test]
    fn a_refused_ring_is_not_available_and_its_rows_are_unknown_rather_than_grey() {
        let window = TimeRange::new(t(7080), t(7280));
        let rows_of = |status: Value| {
            let state = ApiState {
                iq_buffer: Some(std::sync::Arc::new(FakeRing(status))),
                ..ApiState::default()
            };
            let v = overlay_json(&state, band(), window, 10, 1);
            let rows: Vec<String> = v["any"]["cells"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c["state"].as_str().unwrap().to_string())
                .collect();
            let ring = v["sources"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["kind"] == json!("iq-ring"))
                .cloned()
                .expect("the iq-ring source is always reported");
            (rows, ring)
        };

        // 1. Refused: a handle, an answer, and no journal behind either.
        let (rows, ring) = rows_of(json!({
            "enabled": false,
            "reason": "needs 134217728 bytes above the 8589934592-byte free-space floor and \
                       4789297152 bytes are free",
            "allocation": "refused",
            "segments": [],
        }));
        assert_eq!(
            rows,
            states(&[("unknown", 10)]),
            "a refused ring is no tune history, so no row may claim the radio did not look: {ring}"
        );
        assert_eq!(ring["available"], json!(false), "{ring}");
        assert_eq!(ring["spans"], json!(0), "{ring}");

        // 2. Allocating: enabled, but nothing is buffered until it completes.
        let (rows, ring) = rows_of(json!({
            "enabled": true, "reason": Value::Null, "allocation": "allocating",
            "allocation_progress": 0.4, "segments": [],
        }));
        assert_eq!(rows, states(&[("unknown", 10)]), "{ring}");
        assert_eq!(ring["available"], json!(false), "{ring}");

        // 3. A ring that really is holding a journal: available, and its segment is observed. This
        //    is the non-vacuity half - the predicate must not simply call every ring dead.
        let (rows, ring) = rows_of(json!({
            "enabled": true, "reason": Value::Null, "allocation": "full",
            "t0": 7100.0, "t1": 7200.0,
            "segments": [{
                "device_id": RUNNING, "center_hz": 150e6, "sample_rate_hz": 20e6,
                "t0_ns": 7_100i64 * 1_000_000_000, "t1_ns": 7_200i64 * 1_000_000_000,
            }],
        }));
        assert_eq!(ring["available"], json!(true), "{ring}");
        assert_eq!(ring["spans"], json!(1), "{ring}");
        assert_eq!(
            rows,
            states(&[("unobserved", 1), ("observed", 5), ("unobserved", 4)]),
            "the ring's own segment is observed, and after its reach nothing looked: {ring}"
        );
    }

    /// **T-1055: every source row says whether it answered SHORT, and the flag is never absent.**
    ///
    /// `Evidence` has carried this fact since T-1034 and no answer stated it, so a client reading
    /// `unobserved` had no way to tell *nothing looked here* from *this answer does not hold every
    /// record*. It is the same rule as T-920's `available`: a negative that cannot say which
    /// negative it is cannot be read at all.
    ///
    /// RED before the fix: no `sources[]` row has a `truncated` key.
    #[test]
    fn every_source_row_says_whether_its_tune_history_answered_short() {
        let window = TimeRange::new(t(7080), t(7280));
        let sources_of = |segments: Value| {
            let state = ApiState {
                iq_buffer: Some(std::sync::Arc::new(FakeRing(json!({
                    "enabled": true, "reason": Value::Null, "allocation": "full",
                    "t0": 7100.0, "t1": 7200.0, "segments": segments,
                })))
                    as std::sync::Arc<dyn crate::IqBufferControl>),
                ..ApiState::default()
            };
            overlay_json(&state, band(), window, 10, 1)["sources"].clone()
        };
        let segment = |i: i64| {
            json!({ "device_id": RUNNING, "center_hz": 150e6, "sample_rate_hz": 20e6,
                    "t0_ns": (7_100 + i) * 1_000_000_000, "t1_ns": (7_101 + i) * 1_000_000_000 })
        };

        // A short list: every source answered everything it holds, and every row says so.
        let v = sources_of(json!([segment(0)]));
        for row in v.as_array().unwrap() {
            assert_eq!(
                row["truncated"],
                json!(false),
                "the {} row does not state whether it answered short: {row}",
                row["kind"]
            );
        }

        // A list AT the limit is one that may have been cut, and the ring row says so — while the
        // other sources, read whole, keep saying false. A flag that is true for everything whenever
        // one source is cut would be the same silence one level up.
        let full: Vec<Value> = (0..RING_SEGMENTS as i64).map(segment).collect();
        let v = sources_of(Value::Array(full));
        let row = |kind: &str| {
            v.as_array()
                .unwrap()
                .iter()
                .find(|s| s["kind"] == json!(kind))
                .cloned()
                .unwrap_or_else(|| panic!("the {kind} source is always reported: {v}"))
        };
        assert_eq!(
            row("iq-ring")["truncated"],
            json!(true),
            "a segment list at the limit may be a cut one and must say so: {}",
            row("iq-ring")
        );
        assert_eq!(row("observation-log")["truncated"], json!(false));
        assert_eq!(row("open-dwell")["truncated"], json!(false));
    }

    /// **T-920: an unavailable source says WHICH negative it is.**
    ///
    /// T-640 made `available` mean *this source can contribute evidence*, which is right — and
    /// left it **silent**. A client reading `available: false` could not tell a ring that is still
    /// being laid down, and will answer within a second or two, from one **refused** for want of
    /// disk, from one another process has **locked**, from one **disabled by configuration**.
    /// Only the first of those resolves itself, so a reader that cannot tell them apart cannot
    /// tell *wait a moment* from *this device has no ring today* — the same distinction this
    /// module already insists on one level down, where `"unobserved"` may not be spelled as
    /// `"unknown"`.
    ///
    /// That silence is what made
    /// `hk-cli::api_contract::coverage_greys_only_what_was_never_observed_and_names_the_device_that_looked`
    /// red on a loaded Linux host and green on a quiet Mac: its wait was satisfied by the **open
    /// dwell** (T-596) while the ring was still allocating, and the `available: true` it then
    /// asserted was a state the server was seconds away from reaching and could not say so.
    ///
    /// So every row carries `state` and `reason`, read off the status `/api/iqbuffer` already
    /// serves. RED before the fix: no row has either key at all.
    #[test]
    fn an_unavailable_source_states_which_negative_it_is() {
        let window = TimeRange::new(t(7080), t(7280));
        let sources_of = |ring: Option<Value>| {
            let state = ApiState {
                iq_buffer: ring.map(|s| {
                    std::sync::Arc::new(FakeRing(s)) as std::sync::Arc<dyn crate::IqBufferControl>
                }),
                ..ApiState::default()
            };
            overlay_json(&state, band(), window, 10, 1)["sources"].clone()
        };
        let row = |v: &Value, kind: &str| {
            v.as_array()
                .unwrap()
                .iter()
                .find(|s| s["kind"] == json!(kind))
                .cloned()
                .unwrap_or_else(|| panic!("the {kind} source is always reported: {v}"))
        };

        // 1. Still being laid down: the one negative that resolves itself, and it says so.
        let v = sources_of(Some(json!({
            "enabled": true, "allocation": "allocating",
            "reason": "allocating the IQ capture ring in the background: capture is not buffered \
                       until it is allocated",
            "allocation_progress": 0.4, "segments": [],
        })));
        let ring = row(&v, "iq-ring");
        assert_eq!(ring["available"], json!(false), "{ring}");
        assert_eq!(ring["state"], json!("allocating"), "{ring}");
        assert!(
            ring["reason"]
                .as_str()
                .is_some_and(|r| r.contains("allocating")),
            "a ring that is still opening says so in its own words: {ring}"
        );

        // 2. Refused for want of disk: the same `available: false`, a different fact, and the
        //    reason carries the numbers the operator needs.
        let v = sources_of(Some(json!({
            "enabled": false, "allocation": "refused",
            "reason": "needs 134217728 bytes above the 8589934592-byte free-space floor and \
                       4789297152 bytes are free",
            "segments": [],
        })));
        let ring = row(&v, "iq-ring");
        assert_eq!(ring["state"], json!("refused"), "{ring}");
        assert!(
            ring["reason"]
                .as_str()
                .is_some_and(|r| r.contains("free-space floor")),
            "{ring}"
        );

        // 3. Locked by another process: a third fact, distinguishable from both.
        let v = sources_of(Some(json!({
            "enabled": false, "allocation": "locked",
            "reason": "another process holds the ring's lock", "segments": [],
        })));
        assert_eq!(row(&v, "iq-ring")["state"], json!("locked"), "{v}");

        // 4. Disabled by configuration: no allocation state at all, and the reason still speaks.
        let v = sources_of(Some(json!({
            "enabled": false, "allocation": Value::Null,
            "reason": "disabled by configuration (--iq-retention off)", "segments": [],
        })));
        let ring = row(&v, "iq-ring");
        assert_eq!(ring["state"], Value::Null, "{ring}");
        assert!(
            ring["reason"]
                .as_str()
                .is_some_and(|r| r.contains("disabled by configuration")),
            "{ring}"
        );

        // 5. No ring wired at all: still not silent — the row says there is none to ask.
        let v = sources_of(None);
        let ring = row(&v, "iq-ring");
        assert_eq!(ring["available"], json!(false), "{ring}");
        assert_eq!(ring["state"], Value::Null, "{ring}");
        assert!(ring["reason"].is_string(), "{ring}");
        // The observation log is absent on this server too, and says which negative IT is - so
        // the rule is the row's, not one source's special case.
        for kind in ["observation-log", "open-dwell"] {
            let r = row(&v, kind);
            assert_eq!(r["available"], json!(false), "{r}");
            assert!(
                r["reason"]
                    .as_str()
                    .is_some_and(|s| s.contains("observation log")),
                "{r}"
            );
        }

        // 6. Non-vacuity: a source that IS contributing reports `state: "open"` and no reason, so
        //    none of the above can pass by stamping every row with a complaint.
        let v = sources_of(Some(json!({
            "enabled": true, "reason": Value::Null, "allocation": "full",
            "t0": 7100.0, "t1": 7200.0,
            "segments": [{
                "device_id": RUNNING, "center_hz": 150e6, "sample_rate_hz": 20e6,
                "t0_ns": 7_100i64 * 1_000_000_000, "t1_ns": 7_200i64 * 1_000_000_000,
            }],
        })));
        let ring = row(&v, "iq-ring");
        assert_eq!(ring["available"], json!(true), "{ring}");
        assert_eq!(ring["state"], json!("open"), "{ring}");
        assert_eq!(ring["reason"], Value::Null, "{ring}");
    }
}
