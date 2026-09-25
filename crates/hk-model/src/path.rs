//! Traced paths through (time × frequency) — the map's "directions line" (T-897, docs/23 §10.6
//! rule 2, ADR-0023 §2's `paths` layer).
//!
//! # What a path is
//!
//! An **ordered list of `(t, f)` vertices**, each at an absolute capture time, each naming the
//! detection it was measured from. It is the shape an emission took through the plane when that
//! shape is not a rectangle: a chirp's diagonal, a hopper's staircase, a sweeper's sawtooth.
//! ADR-0017 §1.3(a) named the gap this closes — a 10-second chirp's box is the **bounding box** of
//! its sweep "with the per-detection ladder underneath it", and the polyline `f(t)` through that
//! ladder was deferred. This module is that polyline, and it is drawn **beside** the boxes, never
//! instead of them: an interval box (ADR-0019) stays the presence record; a path adds the route.
//!
//! # Derived, not stored — the same discipline as presence intervals
//!
//! A path is a **pure function of stored detections** ([`derive_paths`]), recomputed on every read
//! the way [`crate::presence`] intervals are recomputed from the observation ledger. Detections are
//! immutable, so a path cannot drift from the measurements it is drawn from, and there is no second
//! record to keep in step. Nothing here writes, and nothing here is detection *input*.
//!
//! # Blind, from measurement only
//!
//! Every vertex is a number a detector measured: a detection's centre at its time midpoint, or a
//! hop dwell's start and end. No band plan, catalogue or known hop set is read; nothing is snapped
//! to a raster. The only arithmetic beyond reading a detection is at a chirp's two ends, where the
//! first and last vertex are carried from the neighbouring centre along the measured local slope to
//! the detection's own start/end time — and **clamped inside that detection's measured frequency
//! extent**, so an end vertex can never leave the box it belongs to ([`VertexAt`] says which
//! vertices those are).
//!
//! # The three producers
//!
//! | kind | what the detections look like | test |
//! |---|---|---|
//! | [`PathKind::Chirp`] | a **ladder**: successive detections abutting in time *and* frequency, each centre stepped the same way | ≥ 3 detections, one direction, each step ≥ ½ width, excursion ≥ 1 median width |
//! | [`PathKind::Sweep`] | a chirp that **repeats** (sawtooth: ramps joined by a flyback) or **reverses** (triangle) | ≥ 2 ramps of one continuity family |
//! | [`PathKind::Hop`] | dwells that **abut in time and jump in frequency**, alike in width and length | ≥ [`PathConfig::min_hops`] dwells over ≥ 3 channels |
//!
//! A frequency-stable emission produces **no path**: a carrier cut into segments by the detector's
//! `max_duration_s` steps by a fraction of its own width (jitter), which fails the step test, and it
//! never jumps, which fails the hop test. An isolated burst has no neighbour to chain to.
//!
//! # Honest limits
//!
//! - **A sweep inside one analysis frame has no ladder** (ADR-0017 §1.3(b)): the detection is one
//!   hull and no path is drawn for it. The sweep-rate *field* (`hk_pipeline::chains::sweep`) is the
//!   measurement for that case; it is a number about a region, not a route, and is not faked here.
//! - **A bursty hopper** (packets separated by silence longer than the link gap) chains only its
//!   contiguous runs. The tracker's hop *set* (`hk_detect::track`) still finds its channels.
//! - Paths are per chain, not per emitter identity: two ramps too far apart to link are two chirps.

use serde::{Deserialize, Serialize};

use crate::detection::Detection;
use crate::ids::{DetectionId, ProvenanceId, SurveyId};
use crate::region::{FreqRange, TimeRange};
use crate::time::Timestamp;

/// The producer's name and version, stamped on every path's provenance.
pub const PATH_METHOD: &str = "hk-model/path@1";

/// What kind of route a path traces. A **morphology**, never an identity: "this energy hopped"
/// says nothing about what hopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PathKind {
    /// One monotone sweep over the emission's time extent.
    Chirp,
    /// A sweep that repeats (sawtooth) or reverses (triangle): two or more ramps.
    Sweep,
    /// A frequency-hop sequence: contiguous dwells on distinct channels.
    Hop,
}

impl PathKind {
    /// The wire name (`chirp` / `sweep` / `hop`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chirp => "chirp",
            Self::Sweep => "sweep",
            Self::Hop => "hop",
        }
    }
}

/// Where inside its detection a vertex was measured.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VertexAt {
    /// The detection's time start. For a hop dwell: its centre frequency. For a ramp's first
    /// vertex: the neighbouring slope carried back to the start, clamped inside the box.
    Start,
    /// The detection's time midpoint and measured centre frequency — no arithmetic at all.
    Centre,
    /// The detection's time end (mirror of [`Self::Start`]).
    End,
}

/// One vertex: an absolute capture time, a frequency, and the detection it came from.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PathVertex {
    /// Absolute capture time.
    pub t: Timestamp,
    /// Frequency, Hz.
    pub f_hz: f64,
    /// The detection this vertex was measured from (per-vertex provenance).
    pub detection: DetectionId,
    /// Where in that detection.
    pub at: VertexAt,
}

/// Which measurements a path was derived from, and by what.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PathProvenance {
    /// [`PATH_METHOD`].
    pub method: String,
    /// Every member detection, in time order.
    pub detections: Vec<DetectionId>,
    /// The members' distinct trust records (gain state, overload, device…).
    pub provenance_refs: Vec<ProvenanceId>,
    /// The members' distinct detector configurations.
    pub detector_versions: Vec<String>,
    /// The members' distinct surveys.
    pub surveys: Vec<SurveyId>,
}

/// A traced path (see the module docs).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TracedPath {
    /// Morphology.
    pub kind: PathKind,
    /// Ordered by time; never empty, always ≥ 2.
    pub vertices: Vec<PathVertex>,
    /// Time extent: first member's start to last member's end.
    pub time: TimeRange,
    /// Frequency extent: the union of the members' measured extents.
    pub freq: FreqRange,
    /// Chirp: least-squares slope of the member centres, Hz/s. Sweep: the median ramp's. Hop: none.
    pub rate_hz_per_s: Option<f64>,
    /// Sweep: number of ramps (≥ 2). Chirp: 1. Hop: 0.
    pub ramps: usize,
    /// Hop: number of dwells. Otherwise 0.
    pub hops: usize,
    /// Hop: the distinct channel centres visited, ascending, Hz. Otherwise empty.
    pub channels_hz: Vec<f64>,
    /// What it was derived from.
    pub provenance: PathProvenance,
}

impl TracedPath {
    /// A stable id: the kind and the first member detection. The same detections give the same id
    /// on every read, so a client can keep a focus across polls.
    pub fn id(&self) -> String {
        format!(
            "{}:{}",
            self.kind.as_str(),
            self.provenance
                .detections
                .first()
                .map(ToString::to_string)
                .unwrap_or_default()
        )
    }
}

/// Link and acceptance tolerances. Every one is a ratio of the detections' own measured width or
/// duration, with an absolute floor only where a ratio of a tiny number would be meaningless.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PathConfig {
    /// Largest silence between two linked detections, as a fraction of the earlier one's
    /// duration (0.25 — the tracker's hop-link rule, `max(2 frames, 25 % dwell)`).
    pub gap_frac: f64,
    /// …and its floor, s (0.010: two analysis frames at 500 kS/s).
    pub gap_floor_s: f64,
    /// A continuity step must move the centre by at least this fraction of the wider member's
    /// width (0.5). A carrier's segment-to-segment jitter is a fraction of a bin and fails it.
    pub min_step_frac: f64,
    /// Two continuity members must touch in frequency within this fraction of the wider one's
    /// width (0.35).
    pub touch_frac: f64,
    /// A chirp's centres must travel at least this many median widths end to end (1.0). The step
    /// rule already implies it for a clean ladder; it is the guard against a chain of small steps
    /// that wanders rather than sweeps.
    pub min_excursion_widths: f64,
    /// Fewest members of a continuity ramp (3): two points make a line through anything.
    pub min_ramp: usize,
    /// Fewest dwells in a hop sequence (5).
    pub min_hops: usize,
    /// Fewest distinct channels a hop sequence visits (3). Two alternating channels are as easily
    /// two emitters taking turns.
    pub min_channels: usize,
    /// Hop dwells must agree in duration and in width within this ratio (2.0).
    pub hop_similarity: f64,
    /// Two ramps repeat one sweep (a sawtooth) when their frequency extents overlap by at least
    /// this fraction of the narrower (0.5).
    pub ramp_overlap_frac: f64,
}

impl Default for PathConfig {
    fn default() -> Self {
        Self {
            gap_frac: 0.25,
            gap_floor_s: 0.010,
            min_step_frac: 0.5,
            touch_frac: 0.35,
            min_excursion_widths: 1.0,
            min_ramp: 3,
            min_hops: 5,
            min_channels: 3,
            hop_similarity: 2.0,
            ramp_overlap_frac: 0.5,
        }
    }
}

// ---- small helpers over one detection ----

fn t_s(t: Timestamp) -> f64 {
    t.as_unix_nanos() as f64 / 1e9
}
fn start_s(d: &Detection) -> f64 {
    t_s(d.time.start)
}
fn end_s(d: &Detection) -> f64 {
    t_s(d.time.end)
}
fn dur_s(d: &Detection) -> f64 {
    (end_s(d) - start_s(d)).max(0.0)
}
fn mid_ns(d: &Detection) -> i64 {
    let (a, b) = (d.time.start.as_unix_nanos(), d.time.end.as_unix_nanos());
    a + (b - a) / 2
}
fn lo(d: &Detection) -> f64 {
    d.f_center_hz - d.obw_hz / 2.0
}
fn hi(d: &Detection) -> f64 {
    d.f_center_hz + d.obw_hz / 2.0
}
fn width(d: &Detection) -> f64 {
    d.obw_hz.max(1.0)
}

/// A detection a path may be traced through: finite, with a time extent and a width, and not
/// flagged as something other than an emission (a spur, an image, an impulse or an IMD product).
fn eligible(d: &Detection) -> bool {
    d.f_center_hz.is_finite()
        && d.obw_hz.is_finite()
        && d.obw_hz > 0.0
        && d.time.end > d.time.start
        && !d.flags.spur_candidate
        && !d.flags.image_candidate
        && !d.flags.impulsive
        && !d.flags.suspect_imd
}

impl PathConfig {
    fn gap_s(&self, d: &Detection) -> f64 {
        (self.gap_frac * dur_s(d)).max(self.gap_floor_s)
    }

    /// `b` may follow `a` in time: it starts after `a`'s first quarter and no later than the link
    /// gap past `a`'s end. The early side is generous on purpose: measured through the mock device
    /// (`hk-pipeline/tests/paths_blind.rs`), a transient beside a hop net opened one dwell's box
    /// 20 ms before the previous dwell's box closed.
    fn follows(&self, a: &Detection, b: &Detection) -> bool {
        let (sa, ea, sb) = (start_s(a), end_s(a), start_s(b));
        sb >= sa + 0.25 * dur_s(a) && sb <= ea + self.gap_s(a)
    }

    /// A continuity step: `b` touches `a` in frequency and has moved by a real fraction of a width.
    fn continues(&self, a: &Detection, b: &Detection) -> bool {
        let w = width(a).max(width(b));
        let touch = self.touch_frac * w;
        let step = (b.f_center_hz - a.f_center_hz).abs();
        lo(b) <= hi(a) + touch && hi(b) >= lo(a) - touch && step >= self.min_step_frac * w
    }

    /// A hop: `b` is disjoint from `a` in frequency, and alike in length and width.
    fn hops_to(&self, a: &Detection, b: &Detection) -> bool {
        let ratio = |x: f64, y: f64| x.max(y) / x.min(y).max(1e-12);
        (b.f_center_hz - a.f_center_hz).abs() >= 0.5 * (width(a) + width(b))
            && ratio(dur_s(a), dur_s(b)) <= self.hop_similarity
            && ratio(width(a), width(b)) <= self.hop_similarity
    }
}

/// Traces every path the detections support. `dets` may be in any order; the answer is ordered by
/// start time and is a pure function of the input set.
pub fn derive_paths(dets: &[Detection], cfg: &PathConfig) -> Vec<TracedPath> {
    let mut ds: Vec<&Detection> = dets.iter().filter(|d| eligible(d)).collect();
    ds.sort_by(|a, b| {
        a.time
            .start
            .cmp(&b.time.start)
            .then(a.f_center_hz.total_cmp(&b.f_center_hz))
            .then(a.id.cmp(&b.id))
    });
    let mut used = vec![false; ds.len()];
    let mut out = Vec::new();

    // 1. Continuity ramps (the ladder under a chirp), then repeated ramps into sweeps.
    let ramps = chains(&ds, &mut used, cfg, |a, b, prev| {
        cfg.continues(a, b)
            // Keep one direction inside a chain; a reversal starts a new ramp, joined below.
            && prev.is_none_or(|p| (b.f_center_hz - a.f_center_hz).signum() == p.signum())
    });
    let mut ramps: Vec<Vec<usize>> = ramps
        .into_iter()
        .filter(|r| {
            let ok = ramp_ok(&ds, r, cfg);
            if !ok {
                for &i in r {
                    used[i] = false;
                }
            }
            ok
        })
        .collect();
    ramps.sort_by(|a, b| ds[a[0]].time.start.cmp(&ds[b[0]].time.start));
    for group in group_ramps(&ds, &ramps, cfg) {
        out.push(ramp_path(&ds, &group));
    }

    // 2. Hop sequences over what no ramp claimed.
    let hops = chains(&ds, &mut used, cfg, |a, b, _| cfg.hops_to(a, b));
    for h in hops {
        if hop_ok(&ds, &h, cfg) {
            out.push(hop_path(&ds, &h));
        } else {
            for &i in &h {
                used[i] = false;
            }
        }
    }

    out.sort_by(|a, b| a.time.start.cmp(&b.time.start).then(a.id().cmp(&b.id())));
    out
}

/// Greedy chains: from each unclaimed detection in start order, repeatedly take the unclaimed
/// successor that `link` accepts with the smallest time gap (ties: the smallest frequency step).
/// `link(a, b, prev_step)` sees the chain's previous centre step, if any.
fn chains(
    ds: &[&Detection],
    used: &mut [bool],
    cfg: &PathConfig,
    link: impl Fn(&Detection, &Detection, Option<f64>) -> bool,
) -> Vec<Vec<usize>> {
    let mut out = Vec::new();
    for s in 0..ds.len() {
        if used[s] {
            continue;
        }
        let mut chain = vec![s];
        used[s] = true;
        let mut prev: Option<f64> = None;
        loop {
            let a = ds[*chain.last().expect("non-empty")];
            let horizon = end_s(a) + cfg.gap_s(a);
            let mut best: Option<(usize, f64, f64)> = None;
            // `ds` is sorted by start: scan forward from the tail until starts pass the horizon.
            for (j, b) in ds.iter().enumerate().skip(chain[chain.len() - 1] + 1) {
                if start_s(b) > horizon {
                    break;
                }
                if used[j] || !cfg.follows(a, b) || !link(a, b, prev) {
                    continue;
                }
                let gap = (start_s(b) - end_s(a)).abs();
                let step = (b.f_center_hz - a.f_center_hz).abs();
                if best.is_none_or(|(_, g, st)| (gap, step) < (g, st)) {
                    best = Some((j, gap, step));
                }
            }
            let Some((j, _, _)) = best else { break };
            prev = Some(ds[j].f_center_hz - a.f_center_hz);
            used[j] = true;
            chain.push(j);
        }
        if chain.len() > 1 {
            out.push(chain);
        } else {
            used[s] = false;
        }
    }
    out
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    if v.is_empty() {
        return 0.0;
    }
    let m = v.len() / 2;
    if v.len() % 2 == 1 {
        v[m]
    } else {
        0.5 * (v[m - 1] + v[m])
    }
}

fn ramp_ok(ds: &[&Detection], r: &[usize], cfg: &PathConfig) -> bool {
    if r.len() < cfg.min_ramp {
        return false;
    }
    let fc: Vec<f64> = r.iter().map(|&i| ds[i].f_center_hz).collect();
    let excursion = (fc[fc.len() - 1] - fc[0]).abs();
    excursion >= cfg.min_excursion_widths * median(r.iter().map(|&i| width(ds[i])).collect())
}

/// Least-squares slope of the ramp's centres against their time midpoints, Hz/s.
fn slope(ds: &[&Detection], r: &[usize]) -> f64 {
    let pts: Vec<(f64, f64)> = r
        .iter()
        .map(|&i| (mid_ns(ds[i]) as f64 / 1e9, ds[i].f_center_hz))
        .collect();
    let n = pts.len() as f64;
    let (mt, mf) = pts
        .iter()
        .fold((0.0, 0.0), |(a, b), (t, f)| (a + t / n, b + f / n));
    let (num, den) = pts.iter().fold((0.0, 0.0), |(a, b), (t, f)| {
        (a + (t - mt) * (f - mf), b + (t - mt) * (t - mt))
    });
    if den > 0.0 { num / den } else { 0.0 }
}

fn ramp_extent(ds: &[&Detection], r: &[usize]) -> (f64, f64) {
    r.iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(l, h), &i| {
            (l.min(lo(ds[i])), h.max(hi(ds[i])))
        })
}

/// Ramps that belong to one sweep: the next ramp starts within the link gap of this one's end, and
/// either reverses from where this one stopped (triangle: they touch in frequency) or repeats it
/// (sawtooth: same direction, extents overlapping by `ramp_overlap_frac`).
fn group_ramps(ds: &[&Detection], ramps: &[Vec<usize>], cfg: &PathConfig) -> Vec<Vec<Vec<usize>>> {
    let mut taken = vec![false; ramps.len()];
    let mut groups = Vec::new();
    for s in 0..ramps.len() {
        if taken[s] {
            continue;
        }
        taken[s] = true;
        let mut g = vec![ramps[s].clone()];
        loop {
            let cur = g.last().expect("non-empty");
            let last = ds[*cur.last().expect("ramp")];
            let (clo, chi) = ramp_extent(ds, cur);
            let dir = slope(ds, cur).signum();
            let next = (0..ramps.len()).find(|&k| {
                if taken[k] {
                    return false;
                }
                let first = ds[ramps[k][0]];
                let gap = start_s(first) - end_s(last);
                if gap > cfg.gap_s(last) || gap < -0.5 * dur_s(last) {
                    return false;
                }
                let (nlo, nhi) = ramp_extent(ds, &ramps[k]);
                let ndir = slope(ds, &ramps[k]).signum();
                if ndir == dir {
                    let overlap = (chi.min(nhi) - clo.max(nlo)).max(0.0);
                    overlap >= cfg.ramp_overlap_frac * (chi - clo).min(nhi - nlo)
                } else {
                    cfg.continues(last, first) || {
                        let touch = cfg.touch_frac * width(last).max(width(first));
                        lo(first) <= hi(last) + touch && hi(first) >= lo(last) - touch
                    }
                }
            });
            match next {
                Some(k) => {
                    taken[k] = true;
                    g.push(ramps[k].clone());
                }
                None => break,
            }
        }
        groups.push(g);
    }
    groups
}

fn provenance(ds: &[&Detection], members: &[usize]) -> PathProvenance {
    let mut refs: Vec<ProvenanceId> = Vec::new();
    let mut versions: Vec<String> = Vec::new();
    let mut surveys: Vec<SurveyId> = Vec::new();
    for &i in members {
        let d = ds[i];
        if !refs.contains(&d.provenance_ref) {
            refs.push(d.provenance_ref);
        }
        if !versions.contains(&d.detector_version) {
            versions.push(d.detector_version.clone());
        }
        if !surveys.contains(&d.survey_id) {
            surveys.push(d.survey_id);
        }
    }
    PathProvenance {
        method: PATH_METHOD.to_owned(),
        detections: members.iter().map(|&i| ds[i].id).collect(),
        provenance_refs: refs,
        detector_versions: versions,
        surveys,
    }
}

fn extent(ds: &[&Detection], members: &[usize]) -> (TimeRange, FreqRange) {
    let t0 = members
        .iter()
        .map(|&i| ds[i].time.start)
        .min()
        .expect("members");
    let t1 = members
        .iter()
        .map(|&i| ds[i].time.end)
        .max()
        .expect("members");
    let (l, h) = ramp_extent(ds, members);
    (TimeRange::new(t0, t1), FreqRange::new(l, h))
}

/// One ramp's vertices: its first detection's start, every member's centre, its last one's end.
/// The two ends carry the neighbouring measured slope to the box edge in time and are clamped
/// inside that detection's own frequency extent.
fn ramp_vertices(ds: &[&Detection], r: &[usize], out: &mut Vec<PathVertex>) {
    let at = |i: usize| (mid_ns(ds[i]) as f64 / 1e9, ds[i].f_center_hz);
    let clamp = |d: &Detection, f: f64| f.clamp(lo(d), hi(d));
    let (first, second) = (r[0], r[1]);
    let (lastm, last) = (r[r.len() - 2], r[r.len() - 1]);
    let local = |a: usize, b: usize| {
        let ((ta, fa), (tb, fb)) = (at(a), at(b));
        if tb > ta { (fb - fa) / (tb - ta) } else { 0.0 }
    };
    let d0 = ds[first];
    let (t0, f0) = at(first);
    out.push(PathVertex {
        t: d0.time.start,
        f_hz: clamp(d0, f0 - local(first, second) * (t0 - start_s(d0))),
        detection: d0.id,
        at: VertexAt::Start,
    });
    for &i in r {
        out.push(PathVertex {
            t: Timestamp::from_unix_nanos(mid_ns(ds[i])),
            f_hz: ds[i].f_center_hz,
            detection: ds[i].id,
            at: VertexAt::Centre,
        });
    }
    let dn = ds[last];
    let (tn, fnn) = at(last);
    out.push(PathVertex {
        t: dn.time.end,
        f_hz: clamp(dn, fnn + local(lastm, last) * (end_s(dn) - tn)),
        detection: dn.id,
        at: VertexAt::End,
    });
}

fn ramp_path(ds: &[&Detection], group: &[Vec<usize>]) -> TracedPath {
    let members: Vec<usize> = group.iter().flatten().copied().collect();
    let mut vertices = Vec::new();
    for r in group {
        ramp_vertices(ds, r, &mut vertices);
    }
    let (time, freq) = extent(ds, &members);
    let rates: Vec<f64> = group.iter().map(|r| slope(ds, r)).collect();
    let (kind, rate) = if group.len() == 1 {
        (PathKind::Chirp, rates[0])
    } else {
        (PathKind::Sweep, median(rates))
    };
    TracedPath {
        kind,
        vertices,
        time,
        freq,
        rate_hz_per_s: Some(rate),
        ramps: group.len(),
        hops: 0,
        channels_hz: Vec::new(),
        provenance: provenance(ds, &members),
    }
}

/// The distinct channels a hop chain visits: centres within the median dwell width of one another
/// are one channel (its mean centre).
fn channels(ds: &[&Detection], h: &[usize]) -> Vec<f64> {
    let w = median(h.iter().map(|&i| width(ds[i])).collect());
    let mut fc: Vec<f64> = h.iter().map(|&i| ds[i].f_center_hz).collect();
    fc.sort_by(f64::total_cmp);
    let mut out: Vec<(f64, usize)> = Vec::new();
    for f in fc {
        match out.last_mut() {
            Some((c, n)) if (f - *c / *n as f64).abs() <= w => {
                *c += f;
                *n += 1;
            }
            _ => out.push((f, 1)),
        }
    }
    out.into_iter().map(|(c, n)| c / n as f64).collect()
}

fn hop_ok(ds: &[&Detection], h: &[usize], cfg: &PathConfig) -> bool {
    h.len() >= cfg.min_hops && channels(ds, h).len() >= cfg.min_channels
}

fn hop_path(ds: &[&Detection], h: &[usize]) -> TracedPath {
    let mut vertices = Vec::with_capacity(2 * h.len());
    for &i in h {
        let d = ds[i];
        for (t, at) in [(d.time.start, VertexAt::Start), (d.time.end, VertexAt::End)] {
            vertices.push(PathVertex {
                t,
                f_hz: d.f_center_hz,
                detection: d.id,
                at,
            });
        }
    }
    let (time, freq) = extent(ds, h);
    TracedPath {
        kind: PathKind::Hop,
        vertices,
        time,
        freq,
        rate_hz_per_s: None,
        ramps: 0,
        hops: h.len(),
        channels_hz: channels(ds, h),
        provenance: provenance(ds, h),
    }
}

#[cfg(test)]
#[path = "path_tests.rs"]
mod tests;
