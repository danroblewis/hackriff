//! Queries: records in a box, visits, totals and gaps (ADR-0012 §1.4).

use std::collections::HashMap;

use hk_model::attention::observation::{
    ObservationRecord, ObservationTotals, ObservedWindow, SweepGeometry, Tier, TierSeconds,
};
use hk_model::{FreqRange, TimeRange, Timestamp};

use super::log::ObservationStore;
use super::segment::{HOUR_NS, decode_segment, hour_of};

/// Default page size of [`ObservationStore::query`].
pub const DEFAULT_RECORD_LIMIT: usize = 1000;
/// Largest page size of [`ObservationStore::query`].
pub const MAX_RECORD_LIMIT: usize = 10_000;

/// Longest a record may outlast the query span and still be found, ns (records are filed by end
/// time; a dwell or lease longer than this that started before `t1` is missed).
const LOOKAHEAD_NS: i64 = HOUR_NS;

/// A records query.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RecordQuery {
    /// Frequency box: records whose analysed extent overlaps it.
    pub freq: FreqRange,
    /// Time box: records whose interval overlaps it.
    pub span: TimeRange,
    /// Only records of this tier.
    pub tier: Option<Tier>,
    /// Records to skip (a previous page's `next_cursor`).
    pub cursor: usize,
    /// Page size (clamped to [`MAX_RECORD_LIMIT`]).
    pub limit: usize,
}

/// A page of records.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordPage {
    /// Dwell and sweep records in log order.
    pub records: Vec<ObservationRecord>,
    /// The geometries the page's sweep records reference.
    pub geometries: Vec<SweepGeometry>,
    /// Cursor of the next page, when there is one.
    pub next_cursor: Option<usize>,
}

/// One contiguous observation of a frequency range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Visit {
    /// Observed interval.
    pub observed: TimeRange,
    /// Tier it ran at.
    pub tier: Tier,
}

/// `freq` lies entirely inside one covered extent of `w`.
fn covers(w: &ObservedWindow, freq: &FreqRange) -> bool {
    w.covered()
        .iter()
        .any(|c| c.lo_hz <= freq.lo_hz && freq.hi_hz <= c.hi_hz)
}

fn at(start: Timestamp, ms: u32) -> Timestamp {
    start.saturating_add_nanos(i64::from(ms) * 1_000_000)
}

struct Loaded {
    records: Vec<ObservationRecord>,
    geometries: HashMap<u64, SweepGeometry>,
}

impl ObservationStore {
    fn load(&self, span: TimeRange) -> Loaded {
        let first = hour_of(span.start);
        let last = hour_of(span.end.saturating_add_nanos(LOOKAHEAD_NS));
        let snap = self.snapshot(first, last);
        let mut records = Vec::new();
        let mut geometries = HashMap::new();
        let mut take = |bytes: &[u8]| {
            for r in decode_segment(bytes) {
                match r {
                    ObservationRecord::Geometry(g) => {
                        geometries.insert(g.id, g);
                    }
                    other => records.push(other),
                }
            }
        };
        for f in &snap.files {
            // A segment retention deleted after the snapshot reads as empty.
            if let Ok(bytes) = std::fs::read(f) {
                take(&bytes);
            }
        }
        take(&snap.pending);
        Loaded {
            records,
            geometries,
        }
    }

    /// Records overlapping `q`'s box, paged.
    pub fn query(&self, q: &RecordQuery) -> RecordPage {
        let loaded = self.load(q.span);
        let limit = q.limit.clamp(1, MAX_RECORD_LIMIT);
        let mut matching = loaded.records.into_iter().filter(|r| match r {
            ObservationRecord::Dwell(d) => {
                q.tier.is_none_or(|t| t == d.tier)
                    && d.planned.overlaps(&q.span)
                    && d.window.usable.overlaps(&q.freq)
            }
            ObservationRecord::Sweep(s) => {
                q.tier.is_none_or(|t| t == Tier::BackgroundSweep)
                    && s.span.overlaps(&q.span)
                    && loaded.geometries.get(&s.geometry).is_some_and(|g| {
                        s.visits.iter().any(|v| {
                            g.hops
                                .get(v.hop as usize)
                                .is_some_and(|w| w.usable.overlaps(&q.freq))
                        })
                    })
            }
            ObservationRecord::Geometry(_) => false,
        });
        let records: Vec<ObservationRecord> =
            matching.by_ref().skip(q.cursor).take(limit).collect();
        let next_cursor = matching.next().map(|_| q.cursor + records.len());
        let mut ids: Vec<u64> = records
            .iter()
            .filter_map(|r| match r {
                ObservationRecord::Sweep(s) => Some(s.geometry),
                _ => None,
            })
            .collect();
        ids.sort_unstable();
        ids.dedup();
        let geometries = ids
            .iter()
            .filter_map(|id| loaded.geometries.get(id).cloned())
            .collect();
        RecordPage {
            records,
            geometries,
            next_cursor,
        }
    }

    /// Raw observations of `freq` (entirely inside a covered extent) overlapping `span`, in time
    /// order, one per hop visit or dwell (not merged).
    pub fn observations_of(&self, freq: FreqRange, span: TimeRange) -> Vec<Visit> {
        let loaded = self.load(span);
        observations_in(&loaded, &[freq], span)
            .pop()
            .unwrap_or_default()
    }

    /// [`Self::observations_of`] for each range in `freqs` (one pass over the log's segments).
    pub fn observations_of_each(&self, freqs: &[FreqRange], span: TimeRange) -> Vec<Vec<Visit>> {
        let loaded = self.load(span);
        observations_in(&loaded, freqs, span)
    }

    /// [`ObservationTotals`] of each range in `freqs` over `span` (one pass over the log; the
    /// C12 occupancy weighting input).
    pub fn totals(&self, freqs: &[FreqRange], span: TimeRange) -> Vec<ObservationTotals> {
        let loaded = self.load(span);
        observations_in(&loaded, freqs, span)
            .iter()
            .zip(freqs)
            .map(|(v, f)| totals_from_visits(*f, span, v))
            .collect()
    }
}

fn observations_in(loaded: &Loaded, freqs: &[FreqRange], span: TimeRange) -> Vec<Vec<Visit>> {
    let mut out = vec![Vec::new(); freqs.len()];
    let mut push = |w: &ObservedWindow, observed: TimeRange, tier: Tier| {
        if observed.duration_ns() <= 0 || !observed.overlaps(&span) {
            return;
        }
        for (i, f) in freqs.iter().enumerate() {
            if covers(w, f) {
                out[i].push(Visit { observed, tier });
            }
        }
    };
    for r in &loaded.records {
        match r {
            ObservationRecord::Dwell(d) => push(&d.window, d.observed, d.tier),
            ObservationRecord::Sweep(s) => {
                let Some(g) = loaded.geometries.get(&s.geometry) else {
                    continue;
                };
                for v in &s.visits {
                    if let Some(w) = g.hops.get(v.hop as usize) {
                        let start = at(s.span.start, v.start_ms);
                        let end = at(start, v.observed_ms).min(s.span.end);
                        push(
                            w,
                            TimeRange::new(start, end.max(start)),
                            Tier::BackgroundSweep,
                        );
                    }
                }
            }
            ObservationRecord::Geometry(_) => {}
        }
    }
    for v in &mut out {
        v.sort_by_key(|x| x.observed.start);
    }
    out
}

/// Clips `r` to `span`, ns.
fn clipped_ns(r: TimeRange, span: TimeRange) -> i64 {
    (r.end.min(span.end).as_unix_nanos() - r.start.max(span.start).as_unix_nanos()).max(0)
}

/// Merges time-ordered observations into visits: observations that touch or overlap form one
/// visit (consecutive hops covering the same range are one look, not two). Returns
/// `(interval, activity_independent)`.
fn merge(visits: &[Visit]) -> Vec<(TimeRange, bool)> {
    let mut out: Vec<(TimeRange, bool)> = Vec::new();
    for v in visits {
        let ai = v.tier.activity_independent();
        match out.last_mut() {
            Some((last, a)) if v.observed.start <= last.end => {
                last.end = last.end.max(v.observed.end);
                *a |= ai;
            }
            _ => out.push((v.observed, ai)),
        }
    }
    out
}

/// Totals of `freq` over `span` from its time-ordered observations ([`ObservationStore::observations_of`]).
pub fn totals_from_visits(freq: FreqRange, span: TimeRange, visits: &[Visit]) -> ObservationTotals {
    let mut observed_s = TierSeconds::default();
    for v in visits {
        observed_s.add(v.tier, clipped_ns(v.observed, span) as f64 * 1e-9);
    }
    let merged: Vec<(TimeRange, bool)> = merge(visits)
        .into_iter()
        .filter(|(r, _)| clipped_ns(*r, span) > 0)
        .collect();
    let span_s = span.duration_ns() as f64 * 1e-9;
    let max_gap_s = coverage_gaps(span, visits, 0)
        .iter()
        .map(|g| g.duration_ns() as f64 * 1e-9)
        .fold(0.0, f64::max)
        .min(span_s);
    let starts: Vec<i64> = merged
        .iter()
        .map(|(r, _)| r.start.as_unix_nanos())
        .collect();
    let mean_revisit_s = (starts.len() >= 2).then(|| {
        ((starts[starts.len() - 1] - starts[0]) as f64 * 1e-9 / (starts.len() - 1) as f64)
            .min(span_s)
    });
    ObservationTotals {
        freq,
        span,
        n_visits: merged.len() as u64,
        n_visits_activity_independent: merged.iter().filter(|(_, a)| *a).count() as u64,
        observed_s,
        max_gap_s,
        mean_revisit_s,
    }
}

/// Unobserved intervals of `span` at least `min_gap_ns` long, from time-ordered observations
/// (span edges count).
pub fn coverage_gaps(span: TimeRange, visits: &[Visit], min_gap_ns: i64) -> Vec<TimeRange> {
    let mut gaps = Vec::new();
    let mut cursor = span.start;
    for (r, _) in merge(visits) {
        if r.end <= span.start || r.start >= span.end {
            continue;
        }
        if r.start > cursor {
            gaps.push(TimeRange::new(cursor, r.start));
        }
        cursor = cursor.max(r.end);
    }
    if cursor < span.end {
        gaps.push(TimeRange::new(cursor, span.end));
    }
    gaps.retain(|g| g.duration_ns() >= min_gap_ns.max(1));
    gaps
}
