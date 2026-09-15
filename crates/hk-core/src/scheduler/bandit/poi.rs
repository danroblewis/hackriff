//! POI accounting and coverage-gap disclosure (ADR-0012 §5.5): exact probability of intercept per
//! region from observed intervals — the observation log's records (T-115) or the scheduler's
//! in-memory ring of recent windows — never from the nominal formula.
//!
//! A frequency cell counts as observed at an instant only when a covered extent (usable span
//! minus the DC notch) contains the whole cell (§1.4). A region's POI rows are the mean over its
//! cells (plus the worst cell); coverage gaps are the unobserved stretches longer than twice the
//! nominal revisit, coalesced across adjacent cells with the same gap.

use hk_model::attention::observation::{ObservationRecord, SweepGeometry};
use hk_model::attention::report::CoverageGap;
use hk_model::attention::schedule::{PoiEntry, p_at_least_one, poi_fraction};
use hk_model::{FreqRange, TimeRange, Timestamp};

/// Burst durations every POI disclosure covers, s (§5.5).
pub const DEFAULT_POI_TAUS_S: [f64; 4] = [0.005, 0.1, 1.0, 10.0];

/// Most coverage gaps listed per region.
pub const MAX_GAPS: usize = 64;

/// Most cells a region is split into.
pub const MAX_CELLS: usize = 512;

/// One observed extent over one interval.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CoverageVisit {
    /// Covered frequencies (usable span minus any DC notch), Hz.
    pub covered: FreqRange,
    /// Observed interval.
    pub time: TimeRange,
}

/// POI and coverage of one region over one span.
#[derive(Clone, Debug, PartialEq)]
pub struct RegionPoi {
    /// Region.
    pub region: FreqRange,
    /// Span.
    pub span: TimeRange,
    /// Cell width, Hz.
    pub cell_hz: f64,
    /// Cells.
    pub cells: usize,
    /// Cells observed at least once.
    pub observed_cells: usize,
    /// Mean over cells of the observed fraction of the span.
    pub observed_fraction: f64,
    /// Mean revisit interval over cells with at least two visits, s.
    pub mean_revisit_s: Option<f64>,
    /// Mean-over-cells POI per τ (with P≥1 when a burst rate was given).
    pub poi: Vec<PoiEntry>,
    /// Worst cell's POI per τ, in `poi` order.
    pub poi_min: Vec<f64>,
    /// Gaps listed are longer than this, s (2 × the nominal revisit).
    pub gap_threshold_s: f64,
    /// Coverage gaps.
    pub gaps: Vec<CoverageGap>,
    /// More gaps than [`MAX_GAPS`].
    pub gaps_truncated: bool,
}

/// Covered extents over observed intervals from observation-log records: dwell windows over
/// their `observed` interval, sweep hop visits through their geometry.
pub fn visits_from_records(records: &[ObservationRecord]) -> Vec<CoverageVisit> {
    let geoms: Vec<&SweepGeometry> = records
        .iter()
        .filter_map(|r| match r {
            ObservationRecord::Geometry(g) => Some(g),
            _ => None,
        })
        .collect();
    let mut out = Vec::new();
    for r in records {
        match r {
            ObservationRecord::Dwell(d) => {
                for covered in d.window.covered() {
                    out.push(CoverageVisit {
                        covered,
                        time: d.observed,
                    });
                }
            }
            ObservationRecord::Sweep(s) => {
                let Some(g) = geoms.iter().find(|g| g.id == s.geometry) else {
                    continue;
                };
                let t0 = s.span.start.as_unix_nanos();
                for v in &s.visits {
                    let Some(hop) = g.hops.get(v.hop as usize) else {
                        continue;
                    };
                    let a = t0 + i64::from(v.start_ms) * 1_000_000;
                    let b = a + i64::from(v.observed_ms) * 1_000_000;
                    for covered in hop.covered() {
                        out.push(CoverageVisit {
                            covered,
                            time: TimeRange::new(
                                Timestamp::from_unix_nanos(a),
                                Timestamp::from_unix_nanos(b),
                            ),
                        });
                    }
                }
            }
            ObservationRecord::Geometry(_) => {}
        }
    }
    out
}

/// POI and coverage of `region` over `span` from `visits`. `cell_hz` is raised so the region has
/// at most [`MAX_CELLS`]; `rate_hz` adds P≥1 rows; gaps longer than 2 × `nominal_revisit_s`
/// (else 2 × the measured mean revisit) are listed.
pub fn region_poi(
    visits: &[CoverageVisit],
    region: FreqRange,
    span: TimeRange,
    cell_hz: f64,
    taus_s: &[f64],
    rate_hz: Option<f64>,
    nominal_revisit_s: Option<f64>,
) -> RegionPoi {
    let width = region.width_hz().max(0.0);
    let cell_hz = cell_hz.max(width / MAX_CELLS as f64).max(1.0);
    let cells = ((width / cell_hz).ceil() as usize).max(1);
    let span_s = span.duration_ns().max(0) as f64 / 1e9;
    let mut relevant: Vec<CoverageVisit> = visits
        .iter()
        .copied()
        .filter(|v| v.covered.overlaps(&region) && v.time.overlaps(&span))
        .collect();
    relevant.sort_by(|a, b| a.time.start.cmp(&b.time.start));
    let mut poi_sum = vec![0.0; taus_s.len()];
    let mut poi_min = vec![1.0f64; taus_s.len()];
    let (mut observed_cells, mut observed_sum) = (0usize, 0.0);
    let (mut revisit_sum, mut revisit_n) = (0.0, 0usize);
    let mut per_cell_gaps: Vec<Vec<(i64, i64)>> = Vec::with_capacity(cells);
    let mut intervals: Vec<TimeRange> = Vec::new();
    for k in 0..cells {
        let lo = region.lo_hz + k as f64 * cell_hz;
        let cell = FreqRange::new(lo, (lo + cell_hz).min(region.hi_hz.max(lo)));
        intervals.clear();
        intervals.extend(
            relevant
                .iter()
                .filter(|v| v.covered.lo_hz <= cell.lo_hz && cell.hi_hz <= v.covered.hi_hz)
                .map(|v| v.time),
        );
        let observed = poi_fraction(&intervals, span, 0);
        observed_sum += observed;
        if !intervals.is_empty() {
            observed_cells += 1;
        }
        for (i, tau) in taus_s.iter().enumerate() {
            let p = poi_fraction(&intervals, span, (tau * 1e9) as i64);
            poi_sum[i] += p;
            poi_min[i] = poi_min[i].min(p);
        }
        let merged = merge(&intervals, span);
        if merged.len() >= 2 {
            revisit_sum +=
                (merged[merged.len() - 1].0 - merged[0].0) as f64 / 1e9 / (merged.len() - 1) as f64;
            revisit_n += 1;
        }
        per_cell_gaps.push(complement(&merged, span));
    }
    let mean_revisit_s = (revisit_n > 0).then(|| revisit_sum / revisit_n as f64);
    let gap_threshold_s = 2.0 * nominal_revisit_s.or(mean_revisit_s).unwrap_or(span_s / 2.0);
    let threshold_ns = (gap_threshold_s * 1e9) as i64;
    let mut gaps: Vec<CoverageGap> = Vec::new();
    let mut open: Vec<(usize, i64, i64)> = Vec::new(); // (gap index, start, end) of the previous cell
    let mut truncated = false;
    for (k, cell_gaps) in per_cell_gaps.iter().enumerate() {
        let lo = region.lo_hz + k as f64 * cell_hz;
        let hi = (lo + cell_hz).min(region.hi_hz.max(lo));
        let mut next_open = Vec::new();
        for &(a, b) in cell_gaps.iter().filter(|(a, b)| b - a > threshold_ns) {
            if let Some(&(gi, _, _)) = open.iter().find(|(_, oa, ob)| *oa == a && *ob == b) {
                gaps[gi].freq.hi_hz = hi;
                next_open.push((gi, a, b));
            } else if gaps.len() < MAX_GAPS {
                gaps.push(CoverageGap {
                    freq: FreqRange::new(lo, hi),
                    time: TimeRange::new(
                        Timestamp::from_unix_nanos(a),
                        Timestamp::from_unix_nanos(b),
                    ),
                });
                next_open.push((gaps.len() - 1, a, b));
            } else {
                truncated = true;
            }
        }
        open = next_open;
    }
    let n = cells as f64;
    RegionPoi {
        region,
        span,
        cell_hz,
        cells,
        observed_cells,
        observed_fraction: observed_sum / n,
        mean_revisit_s,
        poi: taus_s
            .iter()
            .zip(&poi_sum)
            .map(|(&tau_s, &sum)| {
                let p_poi = sum / n;
                PoiEntry {
                    tau_s,
                    p_poi,
                    rate_hz,
                    p_at_least_one: rate_hz.map(|r| p_at_least_one(p_poi, r, span_s)),
                }
            })
            .collect(),
        poi_min: if cells > 0 {
            poi_min
        } else {
            vec![0.0; taus_s.len()]
        },
        gap_threshold_s,
        gaps,
        gaps_truncated: truncated,
    }
}

/// Merged `[start, end)` intervals clipped to `span`, ns.
fn merge(intervals: &[TimeRange], span: TimeRange) -> Vec<(i64, i64)> {
    let (s0, s1) = (span.start.as_unix_nanos(), span.end.as_unix_nanos());
    let mut iv: Vec<(i64, i64)> = intervals
        .iter()
        .map(|t| {
            (
                t.start.as_unix_nanos().max(s0),
                t.end.as_unix_nanos().min(s1),
            )
        })
        .filter(|(a, b)| b > a)
        .collect();
    iv.sort_unstable();
    let mut out: Vec<(i64, i64)> = Vec::with_capacity(iv.len());
    for (a, b) in iv {
        match out.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

fn complement(merged: &[(i64, i64)], span: TimeRange) -> Vec<(i64, i64)> {
    let (mut t, s1) = (span.start.as_unix_nanos(), span.end.as_unix_nanos());
    let mut out = Vec::new();
    for &(a, b) in merged {
        if a > t {
            out.push((t, a));
        }
        t = t.max(b);
    }
    if s1 > t {
        out.push((t, s1));
    }
    out
}

/// Fixed-capacity ring of the scheduler's recent planned windows (cut-corrected), the in-memory
/// source of `Scheduler::region_poi` until the observation log (T-115) is queried instead.
#[derive(Clone, Debug)]
pub(crate) struct CoverageRing {
    buf: Vec<CoverageVisit>,
    next: usize,
    len: usize,
}

impl CoverageRing {
    pub(crate) fn new(capacity: usize) -> Self {
        let empty = CoverageVisit {
            covered: FreqRange::new(0.0, 0.0),
            time: TimeRange::instant(Timestamp::UNIX_EPOCH),
        };
        Self {
            buf: vec![empty; capacity.max(1)],
            next: 0,
            len: 0,
        }
    }

    fn last_index(&self) -> Option<usize> {
        (self.len > 0).then(|| (self.next + self.buf.len() - 1) % self.buf.len())
    }

    /// Records a visit; one contiguous with the last visit of the same extent extends it.
    pub(crate) fn push(&mut self, v: CoverageVisit) {
        if let Some(i) = self.last_index() {
            let last = &mut self.buf[i];
            if last.covered == v.covered && last.time.end == v.time.start {
                last.time.end = v.time.end;
                return;
            }
        }
        self.buf[self.next] = v;
        self.next = (self.next + 1) % self.buf.len();
        self.len = (self.len + 1).min(self.buf.len());
    }

    /// Cuts the visits of the running step (the last `n`) at `end`.
    pub(crate) fn cut_last(&mut self, n: usize, end: Timestamp) {
        let cap = self.buf.len();
        for k in 0..n.min(self.len) {
            let i = (self.next + cap - 1 - k) % cap;
            let v = &mut self.buf[i];
            v.time.end = v.time.end.min(end).max(v.time.start);
        }
    }

    /// Visits, oldest first.
    pub(crate) fn to_vec(&self) -> Vec<CoverageVisit> {
        let cap = self.buf.len();
        (0..self.len)
            .map(|k| self.buf[(self.next + cap - self.len + k) % cap])
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_model::attention::schedule::nominal_poi;

    fn tr(a_s: f64, b_s: f64) -> TimeRange {
        TimeRange::new(
            Timestamp::from_unix_nanos((a_s * 1e9) as i64),
            Timestamp::from_unix_nanos((b_s * 1e9) as i64),
        )
    }

    #[test]
    fn periodic_revisits_match_the_formula_and_gaps_are_disclosed() {
        // A 10 MHz region swept by one 15 MHz window for 0.4 s every 30 s over 1 h, except a
        // 10-minute hole.
        let region = FreqRange::new(100e6, 110e6);
        let covered = FreqRange::new(97.5e6, 112.5e6);
        let visits: Vec<CoverageVisit> = (0..120)
            .map(|i| i as f64 * 30.0)
            .filter(|t| !(1200.0..1800.0).contains(t))
            .map(|t| CoverageVisit {
                covered,
                time: tr(t, t + 0.4),
            })
            .collect();
        let span = tr(0.0, 3600.0);
        let r = region_poi(
            &visits,
            region,
            span,
            1e6,
            &DEFAULT_POI_TAUS_S,
            Some(0.01),
            Some(30.0),
        );
        assert_eq!((r.cells, r.observed_cells), (10, 10));
        // Without the hole it would be the nominal formula; with it, 100/120 of the visits.
        let expect = nominal_poi(1.0, 0.4, 30.0) * 100.0 / 120.0;
        assert!(
            (r.poi[2].p_poi - expect).abs() < 1e-3,
            "{:?} vs {expect}",
            r.poi[2]
        );
        assert!(r.poi[0].p_at_least_one.unwrap() > 0.0);
        assert_eq!(r.gaps.len(), 1, "{:?}", r.gaps);
        assert_eq!(r.gaps[0].freq, region);
        assert!((r.gaps[0].time.duration_ns() as f64 / 1e9 - 629.6).abs() < 0.01);
        // A window that does not contain a whole cell does not observe it.
        let partial = [CoverageVisit {
            covered: FreqRange::new(100e6, 104.5e6),
            time: span,
        }];
        let r = region_poi(&partial, region, span, 1e6, &[0.0], None, None);
        assert_eq!(r.observed_cells, 4);
        assert!((r.observed_fraction - 0.4).abs() < 1e-9);
    }

    #[test]
    fn irregular_revisits_match_a_monte_carlo_burst_count() {
        // Deterministic irregular schedule (LCG gaps), 20 ms bursts at uniform random times.
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (x >> 11) as f64 / (1u64 << 53) as f64
        };
        let covered = FreqRange::new(0.0, 20e6);
        let mut visits = Vec::new();
        let mut t = 0.0;
        while t < 3600.0 {
            let d = 0.2 + 3.0 * next();
            visits.push(CoverageVisit {
                covered,
                time: tr(t, t + d),
            });
            t += d + 20.0 * next();
        }
        let span = tr(0.0, 3600.0);
        let r = region_poi(
            &visits,
            FreqRange::new(5e6, 6e6),
            span,
            1e6,
            &[0.02],
            None,
            None,
        );
        let (n, mut hit) = (200_000, 0);
        for _ in 0..n {
            let s = next() * (3600.0 - 0.02);
            if visits.iter().any(|v| {
                v.time.start.as_unix_nanos() as f64 / 1e9 < s + 0.02
                    && s < v.time.end.as_unix_nanos() as f64 / 1e9
            }) {
                hit += 1;
            }
        }
        let measured = hit as f64 / n as f64;
        let p = r.poi[0].p_poi;
        let se = (p * (1.0 - p) / n as f64).sqrt();
        assert!(
            (measured - p).abs() < 4.0 * se + 1e-4,
            "measured {measured} vs exact {p}"
        );
    }

    #[test]
    fn ring_extends_contiguous_visits_and_cuts() {
        let mut ring = CoverageRing::new(2);
        let c = FreqRange::new(0.0, 1.0);
        ring.push(CoverageVisit {
            covered: c,
            time: tr(0.0, 1.0),
        });
        ring.push(CoverageVisit {
            covered: c,
            time: tr(1.0, 2.0),
        });
        assert_eq!(ring.to_vec().len(), 1);
        ring.push(CoverageVisit {
            covered: FreqRange::new(1.0, 2.0),
            time: tr(2.0, 3.0),
        });
        ring.push(CoverageVisit {
            covered: c,
            time: tr(3.0, 4.0),
        });
        ring.cut_last(1, Timestamp::from_unix_nanos(3_500_000_000));
        let v = ring.to_vec();
        assert_eq!(v.len(), 2);
        assert_eq!(v[1].time, tr(3.0, 3.5));
    }
}
