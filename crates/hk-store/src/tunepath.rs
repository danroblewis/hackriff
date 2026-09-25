//! **The device's own route through frequency, as a traced path** (T-898, docs/23 §10.6 rule 2).
//!
//! The user's second map-UI principle is that anything with (time × frequency) coordinates belongs
//! *on* the map. The radio's own movement has coordinates: at every instant it was tuned somewhere,
//! and each retune moved it. That is a polyline — "like Google Maps' directions line" — and this
//! module derives it.
//!
//! # It is a read of records that already exist, not a new ledger
//!
//! The input is exactly the [`CoverageSpan`]s the coverage map is rasterised from — IQ-ring journal
//! segments, sealed observation-log dwells and sweep visits, and the dwell in flight — each already
//! carrying the tune in force (`center_hz`, `sample_rate_hz`), the interval, and which front end
//! ([`Device`]). Nothing here asks the hardware anything or keeps a second history: if the coverage
//! map says the radio was at 433.92 MHz from t0 to t1, the path goes through (t0, 433.92 MHz).
//!
//! # Three things the derivation has to get right
//!
//! - **One timeline per front end** (the several-SDRs rule). Two radios are two routes; they never
//!   merge into one line, and [`Device::Unknown`] — a record that named no radio — is its own
//!   timeline, never folded into a named one (the same rule `/api/coverage`'s `devices[]` keeps).
//! - **The same tune, seen by several sources, is one leg.** A dwell is usually described three
//!   times over — by the ring journal, by the sealed record, and (at the live edge) by the open
//!   dwell — and a notched window contributes three spans of its own (T-595's analysed sides plus
//!   the DC notch). All of those carry the *same* centre, so they are unioned into one leg rather
//!   than drawn as a stack of coincident lines.
//! - **A gap is not a leg.** Where no record covers an interval, nothing says where the radio was,
//!   and a line drawn across it would be an invented claim (the same honesty the coverage grey
//!   keeps). So a run is **broken** at a gap longer than [`TunePathConfig::join_gap_ns`]: the
//!   device gets several [`TunePath`]s, each labelled with it, and the client draws no ink between
//!   them.
//!
//! # The shape of the line
//!
//! With the canvas's axes (X = frequency, Y = time) a **dwell is a vertical run** — the same centre
//! over an interval — and a **retune is a vertex** at the instant the tune changed, joined to the
//! next dwell's first vertex by a near-horizontal jump. So each leg contributes two vertices, its
//! start and its end, in time order.

use std::collections::BTreeMap;

use hk_model::{TimeRange, Timestamp};

use crate::coverage::{CoverageSpan, Device};

/// The method string served beside a derived path, so an answer names the code that made it.
pub const TUNE_PATH_METHOD: &str = "hk-store/tune-path@1";

/// Longest gap, ns, that two legs may be joined across by default: 2 s.
///
/// A sealed dwell record closes as the retune is commanded and the next opens where it closed, and
/// the ring journal opens a segment on every provenance change, so consecutive legs of a continuous
/// run abut to within a frame. Two seconds is comfortably above that jitter and far below any
/// interval in which the radio could have been somewhere the records do not mention.
pub const DEFAULT_JOIN_GAP_NS: i64 = 2_000_000_000;

/// Most vertices one [`TunePath`] carries. A background sweep visits a hop every few milliseconds,
/// so a wide window's route is genuinely enormous; the newest vertices are kept and the path says
/// it was truncated.
pub const MAX_PATH_VERTICES: usize = 2_000;

/// How [`tune_paths`] joins and bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TunePathConfig {
    /// Longest gap two legs are joined across; a longer one starts a new path.
    pub join_gap_ns: i64,
    /// Most vertices one path carries (the newest are kept).
    pub max_vertices: usize,
}

impl Default for TunePathConfig {
    fn default() -> Self {
        Self {
            join_gap_ns: DEFAULT_JOIN_GAP_NS,
            max_vertices: MAX_PATH_VERTICES,
        }
    }
}

/// One interval at one tune: the unit a dwell's vertical run is drawn from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TuneLeg {
    /// The interval the front end was at this tune, in absolute capture time.
    pub time: TimeRange,
    /// The tuned RF centre, Hz — the path's frequency coordinate over that interval.
    pub center_hz: f64,
    /// The sample rate in force, Hz (the instantaneous bandwidth around the centre).
    pub sample_rate_hz: f64,
}

/// A vertex of the drawn line: a (capture time, frequency) point.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TuneVertex {
    /// Absolute capture time.
    pub t: Timestamp,
    /// The tuned centre then, Hz.
    pub f_hz: f64,
    /// The rate in force, Hz.
    pub sample_rate_hz: f64,
    /// Whether this is the start or the end of its leg.
    pub at: TuneVertexAt,
}

/// Which end of its leg a vertex is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TuneVertexAt {
    /// The instant the front end arrived at this tune.
    Start,
    /// The instant it left it (or, for the newest leg, how far the record reaches).
    End,
}

impl TuneVertexAt {
    /// The wire value.
    pub fn as_str(self) -> &'static str {
        match self {
            TuneVertexAt::Start => "start",
            TuneVertexAt::End => "end",
        }
    }
}

/// One front end's continuous route: an ordered run of legs with no unrecorded gap inside it.
#[derive(Clone, Debug, PartialEq)]
pub struct TunePath {
    /// Which front end this is the route of. [`Device::Unknown`] is its own route, never a named
    /// radio's.
    pub device: Device,
    /// The legs, in time order.
    pub legs: Vec<TuneLeg>,
    /// Whether legs were dropped to fit [`TunePathConfig::max_vertices`] (the oldest go first).
    pub truncated: bool,
}

impl TunePath {
    /// The run's time extent.
    pub fn time(&self) -> TimeRange {
        let (a, b) = (
            self.legs.first().map(|l| l.time.start),
            self.legs.last().map(|l| l.time.end),
        );
        match (a, b) {
            (Some(a), Some(b)) => TimeRange::new(a, b),
            _ => TimeRange::new(Timestamp::from_unix_nanos(0), Timestamp::from_unix_nanos(0)),
        }
    }

    /// The lowest and highest centre the route reached, Hz (`None` for an empty route).
    pub fn freq_extent(&self) -> Option<(f64, f64)> {
        let mut it = self.legs.iter().map(|l| l.center_hz);
        let first = it.next()?;
        Some(it.fold((first, first), |(lo, hi), c| (lo.min(c), hi.max(c))))
    }

    /// How many retunes the route contains: a change of centre between consecutive legs.
    pub fn retunes(&self) -> usize {
        self.legs
            .windows(2)
            .filter(|w| w[0].center_hz != w[1].center_hz)
            .count()
    }

    /// The drawn vertices, in time order: each leg's start then its end (a dwell's vertical run),
    /// consecutive legs joined by the retune's jump.
    pub fn vertices(&self) -> Vec<TuneVertex> {
        let mut out = Vec::with_capacity(self.legs.len() * 2);
        for l in &self.legs {
            out.push(TuneVertex {
                t: l.time.start,
                f_hz: l.center_hz,
                sample_rate_hz: l.sample_rate_hz,
                at: TuneVertexAt::Start,
            });
            out.push(TuneVertex {
                t: l.time.end,
                f_hz: l.center_hz,
                sample_rate_hz: l.sample_rate_hz,
                at: TuneVertexAt::End,
            });
        }
        out
    }

    /// A stable id: the device and the run's first instant, so the same run keeps its id as a pane
    /// is panned across it (the rule `/api/paths` keeps for a traced emission).
    pub fn id(&self) -> String {
        format!(
            "tune:{}:{}",
            self.device.as_str(),
            self.legs
                .first()
                .map(|l| l.time.start.as_unix_nanos())
                .unwrap_or_default()
        )
    }
}

/// Every front end's route over `spans`, one [`TunePath`] per continuous run, ordered by device id
/// and then by time.
///
/// `spans` may come from any mix of sources and in any order; duplicates of one tune (the module
/// header's three descriptions of a dwell, and a notched window's three spans) collapse into one
/// leg.
pub fn tune_paths(spans: &[CoverageSpan], cfg: &TunePathConfig) -> Vec<TunePath> {
    let mut by_device: BTreeMap<String, (Device, Vec<TuneLeg>)> = BTreeMap::new();
    for s in spans {
        if s.time.duration_ns() <= 0 || !s.center_hz.is_finite() {
            continue;
        }
        by_device
            .entry(s.device.as_str().to_owned())
            .or_insert_with(|| (s.device.clone(), Vec::new()))
            .1
            .push(TuneLeg {
                time: s.time,
                center_hz: s.center_hz,
                sample_rate_hz: s.sample_rate_hz,
            });
    }
    let mut out = Vec::new();
    for (_, (device, legs)) in by_device {
        for run in runs(legs, cfg) {
            out.push(TunePath {
                device: device.clone(),
                legs: run.0,
                truncated: run.1,
            });
        }
    }
    out
}

/// Merges one device's raw legs into a timeline, then cuts it into continuous runs.
fn runs(mut legs: Vec<TuneLeg>, cfg: &TunePathConfig) -> Vec<(Vec<TuneLeg>, bool)> {
    legs.sort_by(|a, b| {
        a.time
            .start
            .as_unix_nanos()
            .cmp(&b.time.start.as_unix_nanos())
            .then(a.center_hz.total_cmp(&b.center_hz))
    });
    // One timeline: the same tune described by several sources is one leg, and a later leg at a
    // different centre never overlaps an earlier one (a front end has one tune at a time, so an
    // overlap is record jitter at a seal — the later leg starts where the earlier ended).
    let mut merged: Vec<TuneLeg> = Vec::with_capacity(legs.len());
    for leg in legs {
        match merged.last_mut() {
            Some(prev)
                if prev.center_hz == leg.center_hz
                    && leg.time.start.as_unix_nanos() - prev.time.end.as_unix_nanos()
                        <= cfg.join_gap_ns =>
            {
                if leg.time.end > prev.time.end {
                    prev.time = TimeRange::new(prev.time.start, leg.time.end);
                    prev.sample_rate_hz = leg.sample_rate_hz;
                }
            }
            Some(prev) if leg.time.start < prev.time.end => {
                // A different tune overlapping the previous one: clip it to start at the seal.
                let start = prev.time.end;
                if leg.time.end > start {
                    merged.push(TuneLeg {
                        time: TimeRange::new(start, leg.time.end),
                        ..leg
                    });
                }
            }
            _ => merged.push(leg),
        }
    }
    // Cut at every gap the records do not cover.
    let mut out: Vec<(Vec<TuneLeg>, bool)> = Vec::new();
    let mut run: Vec<TuneLeg> = Vec::new();
    for leg in merged {
        let gapped = run.last().is_some_and(|p: &TuneLeg| {
            leg.time.start.as_unix_nanos() - p.time.end.as_unix_nanos() > cfg.join_gap_ns
        });
        if gapped && !run.is_empty() {
            out.push(bound(std::mem::take(&mut run), cfg));
        }
        run.push(leg);
    }
    if !run.is_empty() {
        out.push(bound(run, cfg));
    }
    out
}

/// Keeps at most `max_vertices / 2` legs — the newest, the end a live view is looking at.
fn bound(mut legs: Vec<TuneLeg>, cfg: &TunePathConfig) -> (Vec<TuneLeg>, bool) {
    let max_legs = (cfg.max_vertices / 2).max(1);
    let truncated = legs.len() > max_legs;
    if truncated {
        legs.drain(..legs.len() - max_legs);
    }
    (legs, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coverage::Analysis;
    use hk_model::FreqRange;

    fn ts(s: f64) -> Timestamp {
        Timestamp::from_unix_nanos((s * 1e9) as i64)
    }

    fn span(device: &str, t0: f64, t1: f64, center_hz: f64) -> CoverageSpan {
        CoverageSpan {
            device: if device == "unknown" {
                Device::Unknown
            } else {
                Device::Id(device.to_owned())
            },
            time: TimeRange::new(ts(t0), ts(t1)),
            freq: FreqRange::new(center_hz - 1e6, center_hz + 1e6),
            analysis: Analysis::Analysed,
            center_hz,
            sample_rate_hz: 2e6,
        }
    }

    #[test]
    fn a_dwell_is_two_vertices_and_a_retune_is_where_the_centre_changes() {
        let spans = [
            span("a", 0.0, 10.0, 100e6),
            span("a", 10.0, 20.0, 200e6),
            span("a", 20.0, 25.0, 300e6),
        ];
        let paths = tune_paths(&spans, &TunePathConfig::default());
        assert_eq!(paths.len(), 1, "one continuous run: {paths:?}");
        let p = &paths[0];
        assert_eq!(p.device, Device::Id("a".into()));
        assert_eq!(p.retunes(), 2);
        assert_eq!(p.freq_extent(), Some((100e6, 300e6)));
        let v = p.vertices();
        assert_eq!(v.len(), 6);
        // Each dwell is a vertical run: same frequency, two instants.
        assert_eq!((v[0].f_hz, v[1].f_hz), (100e6, 100e6));
        assert_eq!(v[0].at, TuneVertexAt::Start);
        assert_eq!(v[1].at, TuneVertexAt::End);
        // The retune's vertex is at the recorded instant, at the new centre.
        assert_eq!((v[2].t, v[2].f_hz), (ts(10.0), 200e6));
        assert_eq!(v[5].t, ts(25.0));
        assert!(!p.truncated);
    }

    #[test]
    fn one_tune_described_by_several_sources_is_one_leg() {
        // The ring journal, the sealed record, the open dwell and the DC notch all describe the
        // same dwell; overlapping and duplicate spans must not stack into coincident lines.
        let spans = [
            span("a", 0.0, 10.0, 100e6),
            span("a", 0.0, 10.0, 100e6),
            span("a", 4.0, 12.0, 100e6),
            span("a", 11.9, 14.0, 100e6),
        ];
        let paths = tune_paths(&spans, &TunePathConfig::default());
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].legs.len(), 1, "{:?}", paths[0].legs);
        assert_eq!(paths[0].legs[0].time, TimeRange::new(ts(0.0), ts(14.0)));
        assert_eq!(paths[0].retunes(), 0);
    }

    #[test]
    fn a_gap_no_record_covers_breaks_the_route_rather_than_drawing_across_it() {
        let spans = [
            span("a", 0.0, 10.0, 100e6),
            span("a", 60.0, 70.0, 100e6),
            span("a", 70.0, 80.0, 200e6),
        ];
        let paths = tune_paths(&spans, &TunePathConfig::default());
        assert_eq!(paths.len(), 2, "the 50 s gap is not a leg: {paths:?}");
        assert_eq!(paths[0].legs.len(), 1);
        assert_eq!(paths[1].legs.len(), 2);
        assert_eq!(paths[0].time(), TimeRange::new(ts(0.0), ts(10.0)));
        assert_eq!(paths[1].time(), TimeRange::new(ts(60.0), ts(80.0)));
        assert_ne!(paths[0].id(), paths[1].id());
    }

    #[test]
    fn each_front_end_gets_its_own_route_and_unknown_is_its_own() {
        let spans = [
            span("a", 0.0, 10.0, 100e6),
            span("b", 0.0, 10.0, 900e6),
            span("unknown", 0.0, 10.0, 50e6),
        ];
        let paths = tune_paths(&spans, &TunePathConfig::default());
        assert_eq!(paths.len(), 3);
        let devices: Vec<&str> = paths.iter().map(|p| p.device.as_str()).collect();
        assert_eq!(devices, ["a", "b", "unknown"]);
        assert!(paths.iter().all(|p| p.legs.len() == 1));
    }

    #[test]
    fn an_overlapping_later_tune_starts_where_the_earlier_one_ended() {
        let spans = [span("a", 0.0, 10.0, 100e6), span("a", 9.0, 20.0, 200e6)];
        let paths = tune_paths(&spans, &TunePathConfig::default());
        assert_eq!(paths.len(), 1);
        assert_eq!(
            paths[0].legs.iter().map(|l| l.time).collect::<Vec<_>>(),
            [
                TimeRange::new(ts(0.0), ts(10.0)),
                TimeRange::new(ts(10.0), ts(20.0))
            ],
            "a front end has one tune at a time"
        );
    }

    #[test]
    fn a_long_route_keeps_its_newest_vertices_and_says_it_was_cut() {
        let cfg = TunePathConfig {
            max_vertices: 6,
            ..Default::default()
        };
        let spans: Vec<CoverageSpan> = (0..10)
            .map(|k| {
                let t = f64::from(k);
                span("a", t, t + 1.0, 100e6 + f64::from(k) * 1e6)
            })
            .collect();
        let paths = tune_paths(&spans, &cfg);
        assert_eq!(paths.len(), 1);
        assert!(paths[0].truncated);
        assert_eq!(paths[0].vertices().len(), 6);
        assert_eq!(paths[0].time().end, ts(10.0), "the newest end is kept");
    }
}
