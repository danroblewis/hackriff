//! The association state machine. See the [module docs](super).

use std::ops::Range;

use hk_dsp::SpectrumFrame;
use hk_model::{
    DetectionId, ProvenanceId, TimeRange, Timestamp, TimingFeatures, Track, TrackId, TrackState,
};

use crate::detector::Detector;
use crate::record::{CloseReason, Confirmation, DetectionRecord, DetectorEvent};

use super::config::TrackerConfig;
use super::events::{
    BoundaryKind, CloseCause, Distribution, HopSetSummary, SegmentBoundary, TrackEvent,
    TrackSummary,
};
use super::persist::TrackBatch;
use super::stats::{Coverage, LogHistogram, Moments, START_RING, StartRing, fold_period, raster};

const PENDING_CAPACITY: usize = 256;
const MEMBER_RING: usize = 1024;
const SPLIT_RING: usize = 256;
const RECENT_BURSTS: usize = 32;
const SEGMENT_STARTS: usize = 16;
const MAX_FUSED: usize = 8;
const NS: f64 = 1e9;

/// Counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrackerStats {
    /// Detection records received.
    pub records: u64,
    /// Impulsive records skipped (`track_impulsive` off).
    pub impulsive_skipped: u64,
    /// Records shorter than `min_part_frames` skipped (split side runs).
    pub short_skipped: u64,
    /// Co-timed groups associated (bursts or burst parts).
    pub groups: u64,
    /// Tone-lobe boxes merged into a group beyond its first box.
    pub lobe_parts: u64,
    /// Boxes stitched onto a burst across a max-duration split.
    pub split_continuations: u64,
    /// Boxes stitched onto a burst across a transition.
    pub transition_continuations: u64,
    /// Finished bursts reopened by a late part.
    pub reopened: u64,
    /// Fused boxes spread back onto several tracks.
    pub fused_spread: u64,
    /// Tracks opened.
    pub tracks_opened: u64,
    /// Tracks closed.
    pub tracks_closed: u64,
    /// Track merges.
    pub merges: u64,
    /// Hop links between channel tracks.
    pub hop_links: u64,
    /// Hop sets formed.
    pub hop_sets_formed: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TuneKey {
    center_hz: f64,
    lna_db: f64,
    vga_db: f64,
    amp_on: bool,
}

#[derive(Clone, Copy, Debug)]
struct Part {
    id: DetectionId,
    provenance: ProvenanceId,
    tune: TuneKey,
    segment: u64,
    t0: i64,
    t1: i64,
    lo: f64,
    hi: f64,
    bin_hz: f64,
    frame_ns: i64,
    close: CloseReason,
    continues: bool,
    at_segment_start: Option<bool>,
    preds: [Option<DetectionId>; 2],
    suspect: bool,
    marginal: bool,
    confirmed: bool,
}

impl Part {
    fn natural_start(&self) -> bool {
        self.preds[0].is_none() && self.at_segment_start != Some(true)
    }
}

#[derive(Clone, Copy, Debug)]
struct Group {
    t0: i64,
    t1: i64,
    lo: f64,
    hi: f64,
    bin_hz: f64,
    frame_ns: i64,
    segment: u64,
    provenance: ProvenanceId,
    tune: TuneKey,
    continues: u32,
    transitions: u32,
}

impl Group {
    fn of(parts: &[Part]) -> Group {
        let p = parts[0];
        let mut g = Group {
            t0: p.t0,
            t1: p.t1,
            lo: p.lo,
            hi: p.hi,
            bin_hz: p.bin_hz,
            frame_ns: p.frame_ns,
            segment: p.segment,
            provenance: p.provenance,
            tune: p.tune,
            continues: 0,
            transitions: 0,
        };
        for p in parts {
            g.t0 = g.t0.min(p.t0);
            g.t1 = g.t1.max(p.t1);
            g.lo = g.lo.min(p.lo);
            g.hi = g.hi.max(p.hi);
            g.bin_hz = g.bin_hz.max(p.bin_hz);
            g.frame_ns = g.frame_ns.max(p.frame_ns);
            g.continues += u32::from(p.continues);
            g.transitions += u32::from(p.close == CloseReason::Transition);
        }
        g
    }

    fn fc(&self) -> f64 {
        0.5 * (self.lo + self.hi)
    }

    fn bw(&self) -> f64 {
        self.hi - self.lo
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Normal,
    Split,
    Transition,
}

#[derive(Clone, Copy, Debug)]
struct Burst {
    t0: i64,
    t1: i64,
    on_ns: i64,
    lo: f64,
    hi: f64,
    segment: u64,
    frame_ns: i64,
    pending_splits: u32,
    awaiting_parts: u32,
    awaiting_until: i64,
    split_deadline: i64,
    reopened_len: Option<f64>,
}

#[derive(Clone, Copy, Debug)]
struct Slot {
    live: bool,
    id: TrackId,
    seq: u64,
    fc: f64,
    bw: f64,
    shape_n: u64,
    bin_hz: f64,
    t_first: i64,
    t_last_start: i64,
    t_last_end: i64,
    detections: u64,
    bursts: u64,
    on_ns: i64,
    cur: Option<Burst>,
    last: Option<Burst>,
    starts: StartRing,
    inter: Moments,
    lengths: Moments,
    hist: LogHistogram,
    segments: u32,
    provenance: ProvenanceId,
    tune: TuneKey,
    segment: u64,
    suspect: u64,
    confirmed: u64,
    hop_links: u32,
    hop_set: Option<usize>,
    dirty: bool,
}

impl Slot {
    fn new(id: TrackId, seq: u64, g: &Group) -> Self {
        Self {
            live: true,
            id,
            seq,
            fc: g.fc(),
            bw: g.bw(),
            shape_n: 0,
            bin_hz: g.bin_hz,
            t_first: g.t0,
            t_last_start: g.t0,
            t_last_end: g.t0,
            detections: 0,
            bursts: 0,
            on_ns: 0,
            cur: None,
            last: None,
            starts: StartRing::default(),
            inter: Moments::default(),
            lengths: Moments::default(),
            hist: LogHistogram::default(),
            segments: 0,
            provenance: g.provenance,
            tune: g.tune,
            segment: g.segment,
            suspect: 0,
            confirmed: 0,
            hop_links: 0,
            hop_set: None,
            dirty: true,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct MemberEntry {
    detection: DetectionId,
    slot: usize,
    track: TrackId,
}

#[derive(Clone, Copy, Debug)]
struct SplitEntry {
    segment: u64,
    frames_end: u64,
    lo: f64,
    hi: f64,
    detection: DetectionId,
}

#[derive(Clone, Copy, Debug)]
struct RecentBurst {
    slot: usize,
    track: TrackId,
    t0: i64,
    t1: i64,
    fc: f64,
    bw: f64,
    len_ns: i64,
}

#[derive(Clone, Copy, Debug)]
struct HopMember {
    slot: usize,
    track: TrackId,
    live: bool,
    fc: f64,
    bw: f64,
    bin_hz: f64,
    links: u32,
    dwell_s: f64,
    detections: u64,
}

#[derive(Debug)]
struct HopSet {
    id: TrackId,
    members: Vec<HopMember>,
    links: u64,
    th_sum_ns: f64,
    t_first: i64,
    t_last: i64,
    formed: bool,
    dirty: bool,
}

/// Online burst tracker (C10). Feed it the detector's events in order and call
/// [`Tracker::observe`] (or [`Tracker::observe_frame`]) once per processed frame; it emits
/// [`TrackEvent`]s and stages repository writes for [`Tracker::drain_into`].
pub struct Tracker {
    cfg: TrackerConfig,
    slots: Vec<Slot>,
    free: Vec<usize>,
    live: usize,
    seq: u64,
    pending: Vec<Part>,
    scratch: Vec<Part>,
    splits: Vec<Option<SplitEntry>>,
    split_head: usize,
    members: Vec<Option<MemberEntry>>,
    member_head: usize,
    recent: [Option<RecentBurst>; RECENT_BURSTS],
    recent_head: usize,
    seg_starts: [(u64, u64); SEGMENT_STARTS],
    seg_len: usize,
    seg_head: usize,
    last_segment: Option<u64>,
    coverage: Coverage,
    now: i64,
    next_maintain: i64,
    merge_queue: Vec<(usize, TrackId)>,
    fused: Vec<usize>,
    hop_sets: Vec<Option<HopSet>>,
    hop_free: Vec<usize>,
    staged: Vec<Track>,
    links: Vec<(TrackId, DetectionId)>,
    stats: TrackerStats,
}

impl Tracker {
    /// A tracker with `config`.
    pub fn new(config: TrackerConfig) -> Self {
        Self {
            cfg: config,
            slots: Vec::with_capacity(64),
            free: Vec::with_capacity(64),
            live: 0,
            seq: 0,
            pending: Vec::with_capacity(PENDING_CAPACITY),
            scratch: Vec::with_capacity(64),
            splits: vec![None; SPLIT_RING],
            split_head: 0,
            members: vec![None; MEMBER_RING],
            member_head: 0,
            recent: [None; RECENT_BURSTS],
            recent_head: 0,
            seg_starts: [(0, 0); SEGMENT_STARTS],
            seg_len: 0,
            seg_head: 0,
            last_segment: None,
            coverage: Coverage::default(),
            now: 0,
            next_maintain: 0,
            merge_queue: Vec::with_capacity(64),
            fused: Vec::with_capacity(MAX_FUSED),
            hop_sets: Vec::new(),
            hop_free: Vec::new(),
            staged: Vec::with_capacity(64),
            links: Vec::with_capacity(4096),
            stats: TrackerStats::default(),
        }
    }

    /// Settings.
    pub fn config(&self) -> &TrackerConfig {
        &self.cfg
    }

    /// Counters.
    pub fn stats(&self) -> TrackerStats {
        self.stats
    }

    /// Open tracks.
    pub fn live_tracks(&self) -> usize {
        self.live
    }

    /// Latest stream time seen (frame or record end).
    pub fn now(&self) -> Timestamp {
        Timestamp::from_unix_nanos(self.now)
    }

    /// Bytes held by the tracker's buffers (constant per track; for bounded-memory checks).
    pub fn memory_bytes(&self) -> usize {
        use std::mem::size_of;
        size_of::<Self>()
            + self.slots.capacity() * size_of::<Slot>()
            + self.free.capacity() * size_of::<usize>()
            + self.pending.capacity() * size_of::<Part>()
            + self.scratch.capacity() * size_of::<Part>()
            + self.splits.capacity() * size_of::<Option<SplitEntry>>()
            + self.members.capacity() * size_of::<Option<MemberEntry>>()
            + self.merge_queue.capacity() * size_of::<(usize, TrackId)>()
            + self.staged.capacity() * size_of::<Track>()
            + self.links.capacity() * size_of::<(TrackId, DetectionId)>()
            + self
                .hop_sets
                .iter()
                .flatten()
                .map(|h| size_of::<HopSet>() + h.members.capacity() * size_of::<HopMember>())
                .sum::<usize>()
    }

    /// Forwards one detector event (detections and confirmations; integrated evaluations are
    /// ignored).
    pub fn push<F>(&mut self, event: &DetectorEvent<'_>, out: &mut F)
    where
        F: FnMut(TrackEvent),
    {
        match event {
            DetectorEvent::Detection(r) => self.push_detection(r, out),
            DetectorEvent::Confirmed(c) => self.confirm(c),
            DetectorEvent::Integrated(_) => {}
        }
    }

    /// Records observed time for `[time.start, time.end)` (one detector frame) in detector segment
    /// `segment`, flushes held records and expires idle tracks. Call it for every processed frame,
    /// after forwarding that frame's events: it is how the tracker knows segment starts (a box
    /// starting there may continue a burst closed by the transition) and observation gaps.
    pub fn observe<F>(&mut self, segment: u64, samples: Range<u64>, time: TimeRange, out: &mut F)
    where
        F: FnMut(TrackEvent),
    {
        if self.last_segment != Some(segment) {
            self.seg_starts[self.seg_head] = (segment, samples.start);
            self.seg_head = (self.seg_head + 1) % SEGMENT_STARTS;
            self.seg_len = (self.seg_len + 1).min(SEGMENT_STARTS);
            self.last_segment = Some(segment);
        }
        let (t0, t1) = (time.start.as_unix_nanos(), time.end.as_unix_nanos());
        let slack = (self.cfg.coverage_slack_s * NS) as i64 + (t1 - t0) / 2;
        self.coverage.add(t0, t1, slack);
        self.now = self.now.max(t1);
        while self.flush_one(false, out) {}
        self.maintain(out);
    }

    /// [`Tracker::observe`] for a frame the detector just processed.
    pub fn observe_frame<F>(&mut self, detector: &Detector, frame: &SpectrumFrame, out: &mut F)
    where
        F: FnMut(TrackEvent),
    {
        let Some(seg) = detector.segment() else {
            return;
        };
        let t0 = frame.t.host_time.as_unix_nanos();
        let dur = (frame.sample_count as f64 * NS / frame.spectrum.sample_rate_hz).round() as i64;
        let samples = frame.t.sample_index..frame.t.sample_index + frame.sample_count;
        let time = TimeRange::new(frame.t.host_time, Timestamp::from_unix_nanos(t0 + dur));
        self.observe(seg.segment, samples, time, out);
    }

    /// Takes one detection record (copied; the record is not kept).
    pub fn push_detection<F>(&mut self, r: &DetectionRecord, out: &mut F)
    where
        F: FnMut(TrackEvent),
    {
        self.stats.records += 1;
        let d = &r.detection;
        if d.flags.impulsive && !self.cfg.track_impulsive {
            self.stats.impulsive_skipped += 1;
            return;
        }
        let t0 = d.time.start.as_unix_nanos();
        let t1 = d.time.end.as_unix_nanos().max(t0 + 1);
        let frames = r.frames.end.saturating_sub(r.frames.start).max(1);
        let frame_ns = ((t1 - t0) / frames as i64).max(1);
        let bin_hz = ((r.f_hi_hz - r.f_lo_hz).abs() / r.bins.len().max(1) as f64).max(1e-3);
        let width = d.obw_hz.max(bin_hz);
        let (lo, hi) = (d.f_center_hz - 0.5 * width, d.f_center_hz + 0.5 * width);
        let preds = self.split_preds(r.segment, r.frames.start, lo, hi);
        if frames < self.cfg.min_part_frames && preds[0].is_none() {
            self.stats.short_skipped += 1;
            return;
        }
        if r.continues {
            self.splits[self.split_head] = Some(SplitEntry {
                segment: r.segment,
                frames_end: r.frames.end,
                lo,
                hi,
                detection: d.id,
            });
            self.split_head = (self.split_head + 1) % SPLIT_RING;
        }
        let f = &d.flags;
        let tune = &r.provenance.tune;
        let part = Part {
            id: d.id,
            provenance: d.provenance_ref,
            tune: TuneKey {
                center_hz: tune.center_hz,
                lna_db: tune.lna_db,
                vga_db: tune.vga_db,
                amp_on: tune.amp_on,
            },
            segment: r.segment,
            t0,
            t1,
            lo,
            hi,
            bin_hz,
            frame_ns,
            close: r.close,
            continues: r.continues,
            at_segment_start: self.segment_start(r.segment).map(|s| s == r.samples.start),
            preds,
            suspect: f.clipped
                || f.spur_candidate
                || f.image_candidate
                || f.suspect_imd
                || f.compressed,
            marginal: f.marginal,
            confirmed: r.candidate.is_confirmed(),
        };
        self.now = self.now.max(t1);
        if self.pending.len() >= PENDING_CAPACITY {
            self.flush_one(true, out);
        }
        self.pending.push(part);
        while self.flush_one(false, out) {}
        self.maintain(out);
    }

    /// Counts a later emitter-candidate confirmation on the member's track (members are
    /// remembered for the latest 1024 linked detections, which covers the detector's 10 s repeat
    /// window at ≤ 100 detections/s).
    pub fn confirm(&mut self, c: &Confirmation) {
        let hit = self
            .members
            .iter()
            .flatten()
            .find(|m| m.detection == c.detection)
            .copied();
        if let Some(m) = hit {
            let s = &mut self.slots[m.slot];
            if s.live && s.id == m.track {
                s.confirmed += 1;
                s.dirty = true;
            }
        }
    }

    /// Flushes every held record and closes every track ([`CloseCause::EndOfStream`]).
    pub fn finish<F>(&mut self, out: &mut F)
    where
        F: FnMut(TrackEvent),
    {
        while self.flush_one(true, out) {}
        let mut open: Vec<(i64, usize)> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.cur.filter(|_| s.live).map(|c| (c.t1, i)))
            .collect();
        open.sort_unstable();
        for (_, i) in open {
            if let Some(c) = self.slots[i].cur.as_mut() {
                c.pending_splits = 0;
                c.awaiting_parts = 0;
            }
            self.finalize(i, out);
        }
        for i in 0..self.slots.len() {
            if self.slots[i].live {
                self.close(i, CloseCause::EndOfStream, out);
            }
        }
    }

    /// Summaries of the open tracks.
    pub fn summaries(&self) -> Vec<TrackSummary> {
        (0..self.slots.len())
            .filter(|&i| self.slots[i].live)
            .map(|i| self.summary(i, None))
            .collect()
    }

    /// Formed, open hop sets.
    pub fn hop_sets(&self) -> Vec<HopSetSummary> {
        self.hop_sets
            .iter()
            .flatten()
            .filter(|h| h.formed)
            .map(|h| self.hop_summary(h))
            .collect()
    }

    /// Moves staged track rows (closed, merged, hop sets) and every changed open track, plus the
    /// new track↔detection links, into `batch`. Call it regularly; links accumulate until then.
    pub fn drain_into(&mut self, batch: &mut TrackBatch) {
        batch.upserts.append(&mut self.staged);
        for i in 0..self.slots.len() {
            if self.slots[i].live && self.slots[i].dirty {
                let t = self.summary(i, None).track;
                batch.upserts.push(t);
                self.slots[i].dirty = false;
            }
        }
        for k in 0..self.hop_sets.len() {
            let dirty = self.hop_sets[k]
                .as_ref()
                .is_some_and(|h| h.formed && h.dirty);
            if dirty {
                let t = self.hop_track(self.hop_sets[k].as_ref().unwrap(), false);
                batch.upserts.push(t);
                self.hop_sets[k].as_mut().unwrap().dirty = false;
            }
        }
        batch.links.append(&mut self.links);
        batch.linked_at = Timestamp::from_unix_nanos(self.now);
    }

    // ---- routing ----

    fn segment_start(&self, segment: u64) -> Option<u64> {
        self.seg_starts[..self.seg_len]
            .iter()
            .find(|(s, _)| *s == segment)
            .map(|&(_, start)| start)
    }

    fn split_preds(
        &self,
        segment: u64,
        frames_start: u64,
        lo: f64,
        hi: f64,
    ) -> [Option<DetectionId>; 2] {
        let mut out = [None; 2];
        let mut n = 0;
        for e in self.splits.iter().flatten() {
            if e.segment == segment && e.frames_end == frames_start && e.lo < hi && lo < e.hi {
                out[n] = Some(e.detection);
                n += 1;
                if n == 2 {
                    break;
                }
            }
        }
        out
    }

    fn member_slot(&self, detection: DetectionId) -> Option<usize> {
        self.members
            .iter()
            .flatten()
            .find(|m| m.detection == detection)
            .filter(|m| self.slots[m.slot].live && self.slots[m.slot].id == m.track)
            .map(|m| m.slot)
    }

    /// Routes the held record with the earliest end once it is due (or `force`).
    fn flush_one<F>(&mut self, force: bool, out: &mut F) -> bool
    where
        F: FnMut(TrackEvent),
    {
        let Some((i, p)) = self
            .pending
            .iter()
            .copied()
            .enumerate()
            .min_by_key(|(_, p)| p.t1)
        else {
            return false;
        };
        let hold = i64::from(self.cfg.hold_frames) * p.frame_ns;
        if !force && p.t1 + hold > self.now {
            return false;
        }
        self.pending.swap_remove(i);
        self.scratch.clear();
        self.scratch.push(p);
        self.stats.groups += 1;

        // 1. A box continuing a max-duration split (it starts on the frame after a split box it
        //    overlaps). The tracks awaiting a split at that instant are matched by frequency: one
        //    whose centre it covers continues; several (a fused box) are spread; else the best
        //    overlap, else the predecessor's own track.
        if p.preds[0].is_some() {
            let g = Group::of(&self.scratch);
            self.fused.clear();
            let mut best: Option<(usize, f64)> = None;
            for (i, s) in self.slots.iter().enumerate() {
                let Some(c) = s.cur.filter(|_| s.live) else {
                    continue;
                };
                let tol = (self.cfg.coincidence_frames * p.frame_ns.max(c.frame_ns) as f64) as i64;
                if c.pending_splits == 0 || c.segment != p.segment || (p.t0 - c.t1).abs() > tol {
                    continue;
                }
                let eps = self.cfg.freq_tolerance_bins * p.bin_hz.max(s.bin_hz);
                let w = (0.5 * s.bw).max(eps);
                let ov = p.hi.min(s.fc + w) - p.lo.max(s.fc - w);
                if ov <= 0.0 {
                    continue;
                }
                if s.fc >= p.lo && s.fc <= p.hi && self.fused.len() < MAX_FUSED {
                    self.fused.push(i);
                }
                if best.is_none_or(|(_, b)| ov > b) {
                    best = Some((i, ov));
                }
            }
            let target = match self.fused.len() {
                0 => best
                    .map(|(i, _)| i)
                    .or_else(|| p.preds.iter().flatten().find_map(|d| self.member_slot(*d))),
                1 => Some(self.fused[0]),
                _ => {
                    self.spread(&g, Mode::Split, out);
                    return true;
                }
            };
            if let Some(slot) = target {
                self.apply(slot, &g, Mode::Split, true, out);
                return true;
            }
        }
        // 2. A box at a segment start continuing a burst closed by the transition.
        if p.at_segment_start != Some(false) {
            if let Some(slot) = self.transition_slot(&p) {
                let g = Group::of(&self.scratch);
                self.apply(slot, &g, Mode::Transition, true, out);
                return true;
            }
        }
        // 3. Tone-lobe aggregation: co-timed natural-start boxes, transitively.
        if p.natural_start() {
            let mut k = 0;
            while k < self.scratch.len() {
                let a = self.scratch[k];
                let mut j = 0;
                while j < self.pending.len() {
                    let b = self.pending[j];
                    if b.natural_start() && self.co_timed(&a, &b) {
                        self.scratch.push(self.pending.swap_remove(j));
                        self.stats.lobe_parts += 1;
                    } else {
                        j += 1;
                    }
                }
                k += 1;
            }
        }
        let g = Group::of(&self.scratch);
        self.associate(&g, out);
        true
    }

    fn co_timed(&self, a: &Part, b: &Part) -> bool {
        if a.segment != b.segment {
            return false;
        }
        let tol = (self.cfg.coincidence_frames * a.frame_ns.max(b.frame_ns) as f64) as i64;
        let coincide = (a.t0 - b.t0).abs() <= tol && (a.t1 - b.t1).abs() <= tol;
        // A weak (marginal) sidelobe box lies inside its burst's span without matching its edges.
        let within = |x: &Part, y: &Part| x.marginal && x.t0 >= y.t0 - tol && x.t1 <= y.t1 + tol;
        if !coincide && !within(a, b) && !within(b, a) {
            return false;
        }
        let gap = a.lo.max(b.lo) - a.hi.min(b.hi);
        gap <= self.cfg.lobe_gap_factor * (a.hi - a.lo).max(b.hi - b.lo)
    }

    fn transition_slot(&self, p: &Part) -> Option<usize> {
        let trans_gap = (self.cfg.max_transition_gap_s * NS) as i64;
        let mut best: Option<(usize, f64)> = None;
        for (i, s) in self.slots.iter().enumerate() {
            if !s.live {
                continue;
            }
            let Some(c) = s.cur else { continue };
            if c.awaiting_parts == 0 || p.segment == c.segment {
                continue;
            }
            let tol = (self.cfg.coincidence_frames * p.frame_ns.max(c.frame_ns) as f64) as i64;
            let gap = p.t0 - c.t1;
            let max_gap = if p.at_segment_start == Some(true) {
                trans_gap + tol
            } else {
                tol
            };
            if gap < -tol || gap > max_gap {
                continue;
            }
            let eps = self.cfg.freq_tolerance_bins * p.bin_hz.max(s.bin_hz);
            if p.hi.min(c.hi + eps) <= p.lo.max(c.lo - eps) {
                continue;
            }
            let cost = (0.5 * (p.lo + p.hi) - 0.5 * (c.lo + c.hi)).abs();
            if best.is_none_or(|(_, b)| cost < b) {
                best = Some((i, cost));
            }
        }
        best.map(|(i, _)| i)
    }

    fn timeout_ns(&self, s: &Slot) -> i64 {
        let mut t = self.cfg.idle_timeout_s;
        if let Some(m) = s.inter.mean() {
            t = t.max(self.cfg.idle_timeout_intervals * m);
        }
        (t.min(self.cfg.max_idle_timeout_s.max(self.cfg.idle_timeout_s)) * NS) as i64
    }

    /// Gated cost of `g` on track `s`: centre within ε (or mutual containment) and similar BW.
    fn gate(&self, s: &Slot, g: &Group) -> Option<f64> {
        let floor = self.cfg.freq_tolerance_bins * s.bin_hz.max(g.bin_hz);
        let (bs, bg) = (s.bw.max(floor), g.bw().max(floor));
        let eps = floor.max(self.cfg.freq_tolerance_fraction * bs.max(bg));
        let df = (g.fc() - s.fc).abs();
        let inside = df <= 0.5 * s.bw && s.fc >= g.lo && s.fc <= g.hi;
        if df > eps && !inside {
            return None;
        }
        let ratio = bs.max(bg) / bs.min(bg);
        if ratio > self.cfg.bandwidth_ratio {
            return None;
        }
        Some(df / eps + ratio.ln() / self.cfg.bandwidth_ratio.max(1.0 + 1e-9).ln())
    }

    fn associate<F>(&mut self, g: &Group, out: &mut F)
    where
        F: FnMut(TrackEvent),
    {
        let mut best: Option<(usize, f64)> = None;
        for i in 0..self.slots.len() {
            let s = &self.slots[i];
            if !s.live || self.coverage.observed(s.t_last_end, g.t0) > self.timeout_ns(s) {
                continue;
            }
            if let Some(cost) = self.gate(s, g) {
                if best.is_none_or(|(_, b)| cost < b) {
                    best = Some((i, cost));
                }
            }
        }
        if let Some((i, _)) = best {
            self.apply(i, g, Mode::Normal, true, out);
            return;
        }
        // A box fused across tracks awaiting a split continuation (T-006 re-probe: a gap bridge
        // landing on the split frame): spread it back by frequency overlap.
        self.fused.clear();
        for (i, s) in self.slots.iter().enumerate() {
            let Some(c) = s.cur.filter(|_| s.live) else {
                continue;
            };
            let tol = (self.cfg.coincidence_frames * g.frame_ns.max(c.frame_ns) as f64) as i64;
            if c.pending_splits > 0
                && (g.t0 - c.t1).abs() <= tol
                && s.fc >= g.lo
                && s.fc <= g.hi
                && s.bw * 1.5 < g.bw()
                && self.fused.len() < MAX_FUSED
            {
                self.fused.push(i);
            }
        }
        if self.fused.len() >= 2 {
            self.spread(g, Mode::Split, out);
            return;
        }
        let i = self.open(g, out);
        self.apply(i, g, Mode::Normal, true, out);
    }

    fn spread<F>(&mut self, g: &Group, mode: Mode, out: &mut F)
    where
        F: FnMut(TrackEvent),
    {
        let n = self.fused.len().min(MAX_FUSED);
        let mut slots = [0usize; MAX_FUSED];
        slots[..n].copy_from_slice(&self.fused[..n]);
        let overlap =
            |s: &Slot| (g.hi.min(s.fc + 0.5 * s.bw) - g.lo.max(s.fc - 0.5 * s.bw)).max(0.0);
        slots[..n]
            .sort_unstable_by(|&a, &b| overlap(&self.slots[b]).total_cmp(&overlap(&self.slots[a])));
        let ids = [self.slots[slots[0]].id, self.slots[slots[1]].id];
        for (k, &i) in slots[..n].iter().enumerate() {
            let s = &self.slots[i];
            let (lo, hi) = (g.lo.max(s.fc - 0.5 * s.bw), g.hi.min(s.fc + 0.5 * s.bw));
            let sub = if hi > lo { Group { lo, hi, ..*g } } else { *g };
            self.apply(i, &sub, mode, k == 0, out);
        }
        self.stats.fused_spread += 1;
        out(TrackEvent::Split {
            detection: self.scratch[0].id,
            tracks: ids,
        });
    }

    fn open<F>(&mut self, g: &Group, out: &mut F) -> usize
    where
        F: FnMut(TrackEvent),
    {
        if self.live >= self.cfg.max_live_tracks.max(1) {
            if let Some(stalest) = (0..self.slots.len())
                .filter(|&i| self.slots[i].live)
                .min_by_key(|&i| self.slots[i].t_last_end)
            {
                self.close(stalest, CloseCause::Capacity, out);
            }
        }
        self.seq += 1;
        let slot = Slot::new(TrackId::new(), self.seq, g);
        let i = match self.free.pop() {
            Some(i) => {
                self.slots[i] = slot;
                i
            }
            None => {
                self.slots.push(slot);
                self.slots.len() - 1
            }
        };
        self.live += 1;
        self.stats.tracks_opened += 1;
        out(TrackEvent::Opened {
            track: slot.id,
            at: Timestamp::from_unix_nanos(g.t0),
            f_center_hz: g.fc(),
            bandwidth_hz: g.bw(),
        });
        i
    }

    /// Applies a group (the parts in `scratch`) to track `i`.
    fn apply<F>(&mut self, i: usize, g: &Group, mode: Mode, link: bool, out: &mut F)
    where
        F: FnMut(TrackEvent),
    {
        let cfg = self.cfg;
        let tol = (cfg.coincidence_frames * g.frame_ns as f64) as i64;
        let trans_gap = (cfg.max_transition_gap_s * NS) as i64;
        let hold = i64::from(cfg.hold_frames) * g.frame_ns;

        // Provenance / segment boundary: the track continues, the boundary is recorded.
        {
            let s = &mut self.slots[i];
            let kind = if g.provenance != s.provenance {
                Some(if g.tune.center_hz != s.tune.center_hz {
                    BoundaryKind::Retune
                } else if g.tune.lna_db != s.tune.lna_db
                    || g.tune.vga_db != s.tune.vga_db
                    || g.tune.amp_on != s.tune.amp_on
                {
                    BoundaryKind::GainChange
                } else {
                    BoundaryKind::Provenance
                })
            } else if mode == Mode::Transition && g.segment != s.segment {
                Some(BoundaryKind::Discontinuity)
            } else {
                None
            };
            if let Some(kind) = kind {
                s.segments += 1;
                out(TrackEvent::Segment(SegmentBoundary {
                    track: s.id,
                    at: Timestamp::from_unix_nanos(g.t0),
                    kind,
                    from: s.provenance,
                    to: g.provenance,
                }));
            }
            s.provenance = g.provenance;
            s.tune = g.tune;
            s.segment = s.segment.max(g.segment);
        }

        // Reopen a finished burst that this part continues or overlaps.
        {
            let s = &mut self.slots[i];
            if s.cur.is_none() {
                if let Some(last) = s.last {
                    let near = match mode {
                        Mode::Split => g.t0 <= last.t1 + tol,
                        Mode::Transition => g.t0 <= last.t1 + trans_gap + tol,
                        Mode::Normal => g.t0 < last.t1,
                    };
                    if near && g.t1 > last.t0 {
                        s.cur = Some(Burst {
                            reopened_len: Some(last.on_ns as f64 / NS),
                            pending_splits: 0,
                            awaiting_parts: 0,
                            ..last
                        });
                        s.last = None;
                        self.stats.reopened += 1;
                    }
                }
            }
        }

        let extend = match self.slots[i].cur {
            Some(c) => match mode {
                Mode::Split | Mode::Transition => true,
                Mode::Normal => g.t0 < c.t1 + tol,
            },
            None => false,
        };
        if extend {
            let s = &mut self.slots[i];
            let add = (g.t1 - g.t0.max(s.t_last_end)).max(0);
            s.on_ns += add;
            let c = s.cur.as_mut().expect("extend needs a burst");
            c.on_ns += add;
            c.t1 = c.t1.max(g.t1);
            c.lo = c.lo.min(g.lo);
            c.hi = c.hi.max(g.hi);
            c.segment = c.segment.max(g.segment);
            c.frame_ns = c.frame_ns.max(g.frame_ns);
            match mode {
                Mode::Split => {
                    c.pending_splits = c.pending_splits.saturating_sub(1);
                    self.stats.split_continuations += 1;
                }
                Mode::Transition => {
                    c.awaiting_parts = c.awaiting_parts.saturating_sub(1);
                    self.stats.transition_continuations += 1;
                }
                Mode::Normal => {}
            }
        } else {
            if let Some(c) = self.slots[i].cur.as_mut() {
                c.pending_splits = 0;
                c.awaiting_parts = 0;
                self.finalize(i, out);
            }
            let s = &mut self.slots[i];
            if s.bursts > 0 && g.t0 > s.t_last_start {
                s.inter.push((g.t0 - s.t_last_start) as f64 / NS);
            }
            s.bursts += 1;
            s.starts.push(g.t0);
            s.t_last_start = s.t_last_start.max(g.t0);
            let add = (g.t1 - g.t0.max(s.t_last_end)).max(0);
            s.on_ns += add;
            s.cur = Some(Burst {
                t0: g.t0,
                t1: g.t1,
                on_ns: g.t1 - g.t0,
                lo: g.lo,
                hi: g.hi,
                segment: g.segment,
                frame_ns: g.frame_ns,
                pending_splits: 0,
                awaiting_parts: 0,
                awaiting_until: 0,
                split_deadline: 0,
                reopened_len: None,
            });
        }

        let s = &mut self.slots[i];
        s.t_last_end = s.t_last_end.max(g.t1);
        if mode == Mode::Normal && link {
            let n = s.shape_n as f64;
            if s.shape_n < 8 {
                s.fc = (s.fc * n + g.fc()) / (n + 1.0);
                s.bw = (s.bw * n + g.bw()) / (n + 1.0);
            } else {
                s.fc += (g.fc() - s.fc) / 8.0;
                s.bw += (g.bw() - s.bw) / 8.0;
            }
            s.shape_n += 1;
            s.bin_hz = s.bin_hz.min(g.bin_hz);
        }
        let c = s.cur.as_mut().expect("burst applied");
        if g.continues > 0 {
            c.pending_splits += g.continues;
            c.split_deadline = g.t1 + (cfg.split_wait_s * NS) as i64;
        }
        if g.transitions > 0 {
            c.awaiting_parts += g.transitions;
            c.awaiting_until = g.t1 + trans_gap + hold;
        }
        s.dirty = true;
        if link {
            let track = s.id;
            for k in 0..self.scratch.len() {
                let p = self.scratch[k];
                self.links.push((track, p.id));
                self.members[self.member_head] = Some(MemberEntry {
                    detection: p.id,
                    slot: i,
                    track,
                });
                self.member_head = (self.member_head + 1) % MEMBER_RING;
                let s = &mut self.slots[i];
                s.detections += 1;
                s.suspect += u64::from(p.suspect);
                s.confirmed += u64::from(p.confirmed);
            }
        }
        let c = self.slots[i].cur.expect("burst applied");
        if c.pending_splits == 0 && c.awaiting_parts == 0 {
            self.finalize(i, out);
        }
    }

    /// Ends track `i`'s current burst: length statistics, hop linkage, merge candidate.
    fn finalize<F>(&mut self, i: usize, out: &mut F)
    where
        F: FnMut(TrackEvent),
    {
        let s = &mut self.slots[i];
        let Some(c) = s.cur.take() else { return };
        let len = c.on_ns as f64 / NS;
        let fresh = match c.reopened_len {
            Some(old) => {
                s.lengths.replace(old, len);
                s.hist.remove(old);
                s.hist.add(len);
                false
            }
            None => {
                s.lengths.push(len);
                s.hist.add(len);
                true
            }
        };
        s.last = Some(Burst {
            reopened_len: None,
            ..c
        });
        s.dirty = true;
        let id = s.id;
        if fresh {
            if self.cfg.hop.enabled {
                self.hop_check(i, &c, out);
            }
            if self.merge_queue.len() < self.merge_queue.capacity()
                && !self.merge_queue.iter().any(|&(k, _)| k == i)
            {
                self.merge_queue.push((i, id));
            }
        }
    }

    // ---- hop sets ----

    fn hop_check<F>(&mut self, i: usize, c: &Burst, out: &mut F)
    where
        F: FnMut(TrackEvent),
    {
        let h = self.cfg.hop;
        let s = self.slots[i];
        let gap_tol =
            (h.gap_frames * c.frame_ns as f64).max(h.gap_fraction * c.on_ns as f64) as i64;
        let floor = self.cfg.freq_tolerance_bins * s.bin_hz;
        let mut best: Option<(RecentBurst, i64)> = None;
        for e in self.recent.iter().flatten() {
            if e.track == s.id || !self.slots[e.slot].live || self.slots[e.slot].id != e.track {
                continue;
            }
            let gap = c.t0 - e.t1;
            if e.t0 >= c.t0 || gap.abs() > gap_tol {
                continue;
            }
            if (s.fc - e.fc).abs() < 0.5 * (s.bw + e.bw) {
                continue;
            }
            let (b1, b2) = (s.bw.max(floor), e.bw.max(floor));
            let (l1, l2) = (c.on_ns.max(1) as f64, e.len_ns.max(1) as f64);
            if b1.max(b2) / b1.min(b2) > h.bandwidth_ratio
                || l1.max(l2) / l1.min(l2) > h.length_ratio
            {
                continue;
            }
            if best.is_none_or(|(_, g)| gap.abs() < g) {
                best = Some((*e, gap.abs()));
            }
        }
        self.recent[self.recent_head] = Some(RecentBurst {
            slot: i,
            track: s.id,
            t0: c.t0,
            t1: c.t1,
            fc: s.fc,
            bw: s.bw,
            len_ns: c.on_ns,
        });
        self.recent_head = (self.recent_head + 1) % RECENT_BURSTS;
        if let Some((e, _)) = best {
            self.hop_link(e.slot, i, c.t0 - e.t0, e.t0, c.t1, out);
        }
    }

    fn hop_member(&self, slot: usize) -> HopMember {
        let s = &self.slots[slot];
        HopMember {
            slot,
            track: s.id,
            live: s.live,
            fc: s.fc,
            bw: s.bw,
            bin_hz: s.bin_hz,
            links: s.hop_links,
            dwell_s: s.lengths.mean().unwrap_or(0.0),
            detections: s.detections,
        }
    }

    fn refresh_member(&mut self, set: usize, slot: usize) {
        let m = self.hop_member(slot);
        if let Some(h) = self.hop_sets[set].as_mut() {
            if let Some(x) = h
                .members
                .iter_mut()
                .find(|x| x.slot == slot && x.track == m.track)
            {
                *x = m;
            } else {
                h.members.push(m);
            }
        }
    }

    fn hop_link<F>(&mut self, a: usize, b: usize, th_ns: i64, t0: i64, t1: i64, out: &mut F)
    where
        F: FnMut(TrackEvent),
    {
        self.stats.hop_links += 1;
        self.slots[a].hop_links += 1;
        self.slots[b].hop_links += 1;
        let set = match (self.slots[a].hop_set, self.slots[b].hop_set) {
            (None, None) => {
                let set = HopSet {
                    id: TrackId::new(),
                    members: Vec::with_capacity(16),
                    links: 0,
                    th_sum_ns: 0.0,
                    t_first: t0,
                    t_last: t1,
                    formed: false,
                    dirty: false,
                };
                let k = match self.hop_free.pop() {
                    Some(k) => {
                        self.hop_sets[k] = Some(set);
                        k
                    }
                    None => {
                        self.hop_sets.push(Some(set));
                        self.hop_sets.len() - 1
                    }
                };
                self.slots[a].hop_set = Some(k);
                self.slots[b].hop_set = Some(k);
                k
            }
            (Some(x), None) => {
                self.slots[b].hop_set = Some(x);
                x
            }
            (None, Some(y)) => {
                self.slots[a].hop_set = Some(y);
                y
            }
            (Some(x), Some(y)) if x == y => x,
            (Some(x), Some(y)) => self.hop_union(x, y, out),
        };
        self.refresh_member(set, a);
        self.refresh_member(set, b);
        let hc = self.cfg.hop;
        let now = self.now;
        let formed_now = {
            let h = self.hop_sets[set].as_mut().expect("hop set");
            h.links += 1;
            h.th_sum_ns += th_ns as f64;
            h.t_first = h.t_first.min(t0);
            h.t_last = h.t_last.max(t1);
            h.dirty = true;
            let channels = h
                .members
                .iter()
                .filter(|m| m.links >= hc.min_links_per_channel)
                .count();
            let qualifies = channels >= hc.min_channels && h.links >= hc.min_hops;
            let formed_now = qualifies && !h.formed;
            h.formed |= qualifies;
            formed_now
        };
        let _ = now;
        if formed_now {
            self.stats.hop_sets_formed += 1;
            let members: Vec<usize> = self.hop_sets[set]
                .as_ref()
                .unwrap()
                .members
                .iter()
                .filter(|m| m.live)
                .map(|m| m.slot)
                .collect();
            for m in members {
                self.slots[m].dirty = true;
            }
            let summary = self.hop_summary(self.hop_sets[set].as_ref().unwrap());
            out(TrackEvent::HopSetFormed(summary));
        }
    }

    fn hop_union<F>(&mut self, x: usize, y: usize, out: &mut F) -> usize
    where
        F: FnMut(TrackEvent),
    {
        let (hx, hy) = (
            self.hop_sets[x].as_ref().unwrap(),
            self.hop_sets[y].as_ref().unwrap(),
        );
        let keep_x = hx.formed || (!hy.formed && hx.members.len() >= hy.members.len());
        let (keep, drop) = if keep_x { (x, y) } else { (y, x) };
        let other = self.hop_sets[drop].take().unwrap();
        for m in &other.members {
            if self.slots[m.slot].live && self.slots[m.slot].id == m.track {
                self.slots[m.slot].hop_set = Some(keep);
            }
        }
        let at = other.t_last;
        let (other_id, other_formed) = (other.id, other.formed);
        {
            let h = self.hop_sets[keep].as_mut().unwrap();
            h.members.extend(other.members.iter().copied());
            h.links += other.links;
            h.th_sum_ns += other.th_sum_ns;
            h.t_first = h.t_first.min(other.t_first);
            h.t_last = h.t_last.max(other.t_last);
            h.dirty = true;
        }
        if other_formed {
            let into = self.hop_sets[keep].as_ref().unwrap();
            let into_id = into.id;
            let target = self.hop_track(into, false);
            self.staged.push(target);
            let mut merged = self.hop_track(&other, false);
            merged.state = TrackState::MergedInto(into_id);
            self.staged.push(merged);
            self.stats.merges += 1;
            out(TrackEvent::Merged {
                from: other_id,
                into: into_id,
                at: Timestamp::from_unix_nanos(at),
            });
        }
        self.hop_free.push(drop);
        keep
    }

    fn hop_channels(&self, h: &HopSet) -> (Vec<f64>, Vec<TrackId>, f64, f64, u64, f64) {
        let mut qual: Vec<HopMember> = h
            .members
            .iter()
            .copied()
            .filter(|m| m.links >= self.cfg.hop.min_links_per_channel)
            .collect();
        qual.sort_by(|a, b| a.fc.total_cmp(&b.fc));
        let n = qual.len().max(1) as f64;
        let bin = qual.iter().map(|m| m.bin_hz).fold(0.0, f64::max);
        let bw = qual.iter().map(|m| m.bw).sum::<f64>() / n;
        let dwell = qual.iter().map(|m| m.dwell_s).sum::<f64>() / n;
        let detections = qual.iter().map(|m| m.detections).sum();
        (
            qual.iter().map(|m| m.fc).collect(),
            qual.iter().map(|m| m.track).collect(),
            bin,
            bw,
            detections,
            dwell,
        )
    }

    fn hop_summary(&self, h: &HopSet) -> HopSetSummary {
        let (channels, members, bin, _, _, dwell) = self.hop_channels(h);
        HopSetSummary {
            id: h.id,
            raster_hz: raster(&channels, 1.5 * bin),
            channels_hz: channels,
            members,
            hop_rate_hz: (h.links > 0 && h.th_sum_ns > 0.0)
                .then(|| 1.0 / (h.th_sum_ns / h.links as f64 / NS)),
            dwell_s: (dwell > 0.0).then_some(dwell),
            hops: h.links,
            time: TimeRange::new(
                Timestamp::from_unix_nanos(h.t_first),
                Timestamp::from_unix_nanos(h.t_last),
            ),
        }
    }

    fn hop_track(&self, h: &HopSet, closed: bool) -> Track {
        let (channels, members, _, bw, detections, _) = self.hop_channels(h);
        let (lo, hi) = (
            channels.first().copied().unwrap_or(0.0),
            channels.last().copied().unwrap_or(0.0),
        );
        Track {
            id: h.id,
            state: if closed {
                TrackState::Closed
            } else {
                TrackState::Open
            },
            split_from: None,
            time: TimeRange::new(
                Timestamp::from_unix_nanos(h.t_first),
                Timestamp::from_unix_nanos(h.t_last),
            ),
            f_center_hz: 0.5 * (lo + hi),
            bandwidth_hz: (hi - lo) + bw,
            detection_count: detections,
            timing: TimingFeatures {
                hop_rate_hz: (h.links > 0 && h.th_sum_ns > 0.0)
                    .then(|| 1.0 / (h.th_sum_ns / h.links as f64 / NS)),
                hop_set_hz: channels,
                co_occurring: members,
                ..TimingFeatures::default()
            },
            updated_at: Timestamp::from_unix_nanos(self.now),
        }
    }

    // ---- lifecycle ----

    fn maintain<F>(&mut self, out: &mut F)
    where
        F: FnMut(TrackEvent),
    {
        if self.now < self.next_maintain {
            return;
        }
        self.next_maintain = self.now + (self.cfg.maintain_interval_s * NS).max(1.0) as i64;
        let now = self.now;
        for i in 0..self.slots.len() {
            if !self.slots[i].live {
                continue;
            }
            if let Some(c) = self.slots[i].cur.as_mut() {
                if c.awaiting_parts > 0 && now > c.awaiting_until {
                    c.awaiting_parts = 0;
                }
                if c.pending_splits > 0 && now > c.split_deadline {
                    c.pending_splits = 0;
                }
                if c.awaiting_parts == 0 && c.pending_splits == 0 {
                    self.finalize(i, out);
                }
            }
            let s = &self.slots[i];
            if s.cur.is_none() && self.coverage.observed(s.t_last_end, now) > self.timeout_ns(s) {
                self.close(i, CloseCause::Idle, out);
            }
        }
        while let Some((i, id)) = self.merge_queue.pop() {
            self.try_merge(i, id, out);
        }
    }

    fn try_merge<F>(&mut self, i: usize, id: TrackId, out: &mut F)
    where
        F: FnMut(TrackEvent),
    {
        let a = &self.slots[i];
        if !a.live || a.id != id || a.hop_set.is_some() || a.cur.is_some() {
            return;
        }
        let Some(a_last) = a.last else { return };
        for j in 0..self.slots.len() {
            let b = &self.slots[j];
            if j == i || !b.live || b.hop_set.is_some() {
                continue;
            }
            let floor = self.cfg.freq_tolerance_bins * a.bin_hz.max(b.bin_hz);
            let (ba, bb) = (a.bw.max(floor), b.bw.max(floor));
            let eps = floor.max(self.cfg.freq_tolerance_fraction * ba.max(bb));
            if (a.fc - b.fc).abs() > self.cfg.merge_fraction * eps
                || ba.max(bb) / ba.min(bb) > self.cfg.merge_bandwidth_ratio
            {
                continue;
            }
            // Simultaneous bursts are two co-channel emitters, not fragments of one.
            if let Some(c) = b.cur.or(b.last) {
                if c.t0 < a_last.t1 && a_last.t0 < c.t1 {
                    continue;
                }
            }
            let (into, from) = if a.seq < b.seq { (i, j) } else { (j, i) };
            self.merge(into, from, out);
            return;
        }
    }

    fn merge<F>(&mut self, into: usize, from: usize, out: &mut F)
    where
        F: FnMut(TrackEvent),
    {
        let f = self.slots[from];
        {
            let t = &mut self.slots[into];
            let (nt, nf) = (t.bursts.max(1) as f64, f.bursts.max(1) as f64);
            t.fc = (t.fc * nt + f.fc * nf) / (nt + nf);
            t.bw = (t.bw * nt + f.bw * nf) / (nt + nf);
            t.detections += f.detections;
            t.bursts += f.bursts;
            t.on_ns += f.on_ns;
            t.starts.absorb(&f.starts);
            t.inter.absorb(&f.inter);
            t.lengths.absorb(&f.lengths);
            t.hist.absorb(&f.hist);
            t.t_first = t.t_first.min(f.t_first);
            t.t_last_start = t.t_last_start.max(f.t_last_start);
            t.t_last_end = t.t_last_end.max(f.t_last_end);
            t.suspect += f.suspect;
            t.confirmed += f.confirmed;
            t.segments += f.segments;
            if t.cur.is_none() {
                t.cur = f.cur;
            }
            if t.last.is_none_or(|l| f.last.is_some_and(|fl| fl.t1 > l.t1)) {
                t.last = f.last;
            }
            t.dirty = true;
        }
        let into_id = self.slots[into].id;
        for m in self.members.iter_mut().flatten() {
            if m.slot == from && m.track == f.id {
                m.slot = into;
                m.track = into_id;
            }
        }
        for r in self.recent.iter_mut().flatten() {
            if r.slot == from && r.track == f.id {
                r.slot = into;
                r.track = into_id;
            }
        }
        let target = self.summary(into, None).track;
        self.staged.push(target);
        let mut merged = self.summary(from, None).track;
        merged.state = TrackState::MergedInto(into_id);
        self.staged.push(merged);
        self.slots[from].live = false;
        self.free.push(from);
        self.live -= 1;
        self.stats.merges += 1;
        out(TrackEvent::Merged {
            from: f.id,
            into: into_id,
            at: Timestamp::from_unix_nanos(self.now),
        });
    }

    fn close<F>(&mut self, i: usize, cause: CloseCause, out: &mut F)
    where
        F: FnMut(TrackEvent),
    {
        if let Some(c) = self.slots[i].cur.as_mut() {
            c.pending_splits = 0;
            c.awaiting_parts = 0;
            self.finalize(i, out);
        }
        let summary = self.summary(i, Some(cause));
        self.staged.push(summary.track.clone());
        self.slots[i].live = false;
        if let Some(set) = self.slots[i].hop_set {
            self.refresh_member(set, i);
            let done = self.hop_sets[set]
                .as_ref()
                .is_none_or(|h| h.members.iter().all(|m| !m.live));
            if done {
                if let Some(h) = self.hop_sets[set].take() {
                    if h.formed {
                        self.staged.push(self.hop_track(&h, true));
                        out(TrackEvent::HopSetClosed(self.hop_summary(&h)));
                    }
                }
                self.hop_free.push(set);
            }
        }
        self.free.push(i);
        self.live -= 1;
        self.stats.tracks_closed += 1;
        out(TrackEvent::Closed(summary));
    }

    fn summary(&self, i: usize, closed: Option<CloseCause>) -> TrackSummary {
        let s = &self.slots[i];
        let mut buf = [0i64; START_RING];
        let n = s.starts.sorted_into(&mut buf);
        let period = fold_period(&buf[..n], &self.cfg.period);
        let horizon = match (period, closed) {
            (Some(p), _) => s.t_last_start + (p.period_s * NS) as i64,
            (None, Some(CloseCause::Idle | CloseCause::Capacity)) => s.t_last_end,
            (None, _) => self.now.max(s.t_last_end),
        }
        .max(s.t_last_end);
        let observed = self.coverage.observed(s.t_first, horizon);
        let duty = (observed > 0).then(|| (s.on_ns as f64 / observed as f64).min(1.0));
        let burst_length = s.lengths.mean().map(|mean| Distribution {
            count: s.lengths.n,
            mean_s: mean,
            std_s: s.lengths.std().unwrap_or(0.0),
            min_s: s.lengths.min,
            max_s: s.lengths.max,
            p50_s: s.hist.quantile(0.5).unwrap_or(mean),
            p90_s: s.hist.quantile(0.9).unwrap_or(mean),
        });
        let hop_set = s
            .hop_set
            .and_then(|k| self.hop_sets[k].as_ref())
            .filter(|h| h.formed)
            .map(|h| h.id);
        let (ia_mean, ia_std) = (s.inter.mean(), s.inter.std());
        let track = Track {
            id: s.id,
            state: if closed.is_some() {
                TrackState::Closed
            } else {
                TrackState::Open
            },
            split_from: None,
            time: TimeRange::new(
                Timestamp::from_unix_nanos(s.t_first),
                Timestamp::from_unix_nanos(s.t_last_end),
            ),
            f_center_hz: s.fc,
            bandwidth_hz: s.bw,
            detection_count: s.detections,
            timing: TimingFeatures {
                period_s: period.map(|p| p.period_s),
                duty_cycle: duty,
                inter_arrival_mean_s: ia_mean,
                inter_arrival_std_s: ia_std,
                co_occurring: hop_set.into_iter().collect(),
                ..TimingFeatures::default()
            },
            updated_at: Timestamp::from_unix_nanos(self.now),
        };
        TrackSummary {
            track,
            burst_count: s.bursts,
            on_time_s: s.on_ns as f64 / NS,
            observed_s: observed as f64 / NS,
            period,
            burst_length,
            inter_arrival_cv: match (ia_mean, ia_std) {
                (Some(m), Some(sd)) if m > 0.0 => Some(sd / m),
                _ => None,
            },
            segments: s.segments,
            hop_set,
            suspect_fraction: if s.detections > 0 {
                s.suspect as f64 / s.detections as f64
            } else {
                0.0
            },
            confirmed_detections: s.confirmed,
            next_burst_eta: period
                .map(|p| Timestamp::from_unix_nanos(s.t_last_start + (p.period_s * NS) as i64)),
            closed,
        }
    }
}
