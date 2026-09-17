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

use hk_model::{FreqRange, TimeRange, Timestamp};
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
        let spans: Vec<TimeRange> = self
            .spans
            .iter()
            .filter(|s| s.freq.overlaps(&freq))
            .map(|s| s.time)
            .collect();
        hk_model::IdleGap::from_coverage(&spans, window)
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
fn observation_spans(
    store: &ObservationStore,
    freq: FreqRange,
    window: TimeRange,
) -> (Vec<CoverageSpan>, usize) {
    let page = store.query(&RecordQuery {
        freq,
        span: window,
        tier: None,
        cursor: 0,
        limit: MAX_RECORD_LIMIT,
    });
    let read = hk_store::spans_from_records(&page.records, &page.geometries, freq);
    (read.spans, read.named)
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
    ring_available: bool,
    log_available: bool,
    /// The earliest instant **any** consulted source still holds a record for; `None` when no
    /// source holds one at all.
    ///
    /// `min`, not `max`: a row is knowable if *at least one* record reaches it. The IQ ring's
    /// journal opens a segment on every provenance change, so within what the ring still buffers
    /// an absence of segment really is "this front end was not tuned here"; the observation log
    /// drops whole hour segments, so its oldest surviving segment's hour start is exactly the
    /// instant past which its silence stops being evidence. Before the earlier of the two, neither
    /// record can speak, and `unobserved` would be a claim nothing supports.
    pub oldest_record: Option<Timestamp>,
}

impl Evidence {
    /// Reads both tune histories over `freq × window`, and each one's reach.
    pub(crate) fn collect(state: &ApiState, freq: FreqRange, window: TimeRange) -> Self {
        let mut spans = ring_spans(state, freq, window);
        let ring = spans.len();
        let ring_named = spans.iter().filter(|s| s.device.is_named()).count();
        let mut log_named = 0;
        if let Some(store) = state.observations.as_ref() {
            let (log_spans, named) = observation_spans(store, freq, window);
            log_named = named;
            spans.extend(log_spans);
        }
        let log = spans.len() - ring;
        Evidence {
            spans,
            ring,
            ring_named,
            log,
            log_named,
            ring_available: state.iq_buffer.is_some(),
            log_available: state.observations.is_some(),
            oldest_record: oldest_record(state),
        }
    }

    /// Which tune histories answered, how many of each one's spans actually named the radio, and
    /// whether every span it contributed did. `device_known` is **measured, not declared**
    /// (T-378): a log still holding records written before devices were logged reports them as the
    /// unattributed spans they are instead of claiming a device-local horizon it has not got. A
    /// source with no spans still appears, so a client can tell "this record had nothing here"
    /// from "this record was not consulted".
    fn sources_json(&self) -> Value {
        json!([
            { "kind": "iq-ring", "spans": self.ring, "named_spans": self.ring_named,
              "device_known": self.ring_named == self.ring,
              "available": self.ring_available },
            { "kind": "observation-log", "spans": self.log, "named_spans": self.log_named,
              "device_known": self.log_named == self.log,
              "available": self.log_available },
        ])
    }

    /// How many leading rows of `g` lie wholly before the record horizon — the rows whose
    /// `unobserved` cells must be served as `"unknown"` instead.
    ///
    /// With **no** surviving record anywhere, every row is beyond the horizon: a server that has
    /// forgotten (or never had) its tune history cannot say the radio was not there, and greying
    /// the window would be precisely the claim this route exists to refuse.
    fn unknown_rows(&self, g: &CoverageGrid) -> usize {
        match self.oldest_record {
            Some(t) => g.unknown_rows_before(t),
            None => g.nt,
        }
    }

    /// The horizon block: the boundary, where it came from, and what lies before it.
    fn horizon_json(&self, g: &CoverageGrid) -> Value {
        let unknown = self.unknown_rows(g);
        json!({
            // Unix s, or null when nothing on this server holds a tune record at all.
            "oldest_record_s": self.oldest_record.map(|t| t.as_unix_nanos() as f64 * 1e-9),
            // Leading rows of the grid that lie wholly before it — the rows whose unobserved cells
            // are served as `"unknown"`. Served so a client can check the states it was sent.
            "unknown_rows": unknown,
            "rows": g.nt,
            "rule": "a row wholly before `oldest_record_s` has no surviving record either way, so \
                its unsampled cells are \"unknown\" (we no longer know whether we looked), never \
                \"unobserved\" (nothing looked). An observed cell is never relabelled: a surviving \
                measurement is itself proof we looked.",
            "state_rule": "\"unknown\" carries no measurement keys, exactly like \"unobserved\", \
                and must be drawn as neither grey nor a level — forgetting is not a measurement of \
                nothing.",
        })
    }
}

/// The earliest instant either tune history still holds a record for — the record horizon.
///
/// The IQ ring reports what it actually buffers; the observation log retains whole hour segments
/// and drops whole hour segments, so its oldest hour's start is the exact boundary. A source that
/// is absent, or holds nothing, contributes no reach — it is not evidence of anything.
fn oldest_record(state: &ApiState) -> Option<Timestamp> {
    let ring = crate::timeline::capture_window(state)
        .0
        .buffered
        .map(|(t0, _)| (t0 * 1e9).round() as i64);
    let log = state.observations.as_ref().and_then(|s| {
        s.hours()
            .into_iter()
            .min()
            .map(|h| h.saturating_mul(hk_store::observation::segment::HOUR_NS))
    });
    match (ring, log) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (x, y) => x.or(y),
    }
    .map(Timestamp::from_unix_nanos)
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
                "state": "observed",
                "spans": s.spans,
                "observed_s": s.observed_ns as f64 * 1e-9,
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

/// One grid's JSON, with `unknown_rows` leading rows served as the fourth state.
///
/// `unknown_rows` is a **row** count because the horizon is a time: a discarded record takes every
/// frequency with it (this module's header, reason 2).
fn grid_json(g: &CoverageGrid, shades: Option<&[Option<f32>]>, unknown_rows: usize) -> Value {
    let mut unknown_cells = 0usize;
    let cells: Vec<Value> = g
        .cells
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let beyond = g.nf > 0 && i / g.nf < unknown_rows;
            unknown_cells += usize::from(beyond && !c.is_observed());
            cell_json(c, shades.map(|s| s.get(i).copied().flatten()), beyond)
        })
        .collect();
    json!({
        "device": g.device.as_str(),
        // Whether `device` is a real front-end identity. `"unknown"` and `"any"` are labels, not
        // radios, and a client must not attribute their coverage to a device.
        "named": g.device.is_named(),
        "observed_cells": g.observed_cells(),
        // Cells that are genuinely grey: nothing looked, and a surviving record says so. The
        // `"unknown"` cells are **not** counted here — they are the fourth state, and adding them
        // in would be the collapse this route exists to refuse.
        "unobserved_cells": g.unobserved_cells() - unknown_cells,
        "unknown_cells": unknown_cells,
        "observed_fraction": g.observed_fraction(),
        "cells": cells,
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
        Ok(crate::query::overview_read(p, &region, rows, cells)?.grid)
    })
    .map(|o| StripShades {
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
    })
    .unwrap_or_else(|_| StripShades {
        values: vec![None; rows * cells],
        range_db: None,
        scale: None,
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
                not grey and not a level — draw it as a fourth thing (hatching, per T-413)",
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
            grey a cell if and only if its state is \"unobserved\".",
    })
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
        let c =
            hk_store::coverage::Coverage::of(2, 30_000_000_000, 60_000_000_000, t(1030), 1e8, 2e6);
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
}
