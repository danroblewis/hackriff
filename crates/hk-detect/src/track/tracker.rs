//! The association state machine. See the [module docs](super).

use std::ops::Range;

use hk_dsp::SpectrumFrame;
use hk_model::{
    DetectionId, ProvenanceId, TimeRange, Timestamp, TimingFeatures, Track, TrackId, TrackSegment,
    TrackState,
};

use crate::detector::Detector;
use crate::record::{CloseReason, Confirmation, DetectionRecord, DetectorEvent};

use super::config::{HopConfig, TrackerConfig};
use super::events::{
    BoundaryKind, CloseCause, Distribution, HopSetSummary, SegmentBoundary, TrackEvent,
    TrackSummary,
};
use super::persist::TrackBatch;
use super::stats::{
    Coverage, LogHistogram, Moments, RasterChannel, START_RING, StartRing, fold_period, raster,
    robust_raster,
};

const SHAPE_RING: usize = 16;

const PENDING_CAPACITY: usize = 256;
const MEMBER_RING: usize = 1024;
const SPLIT_RING: usize = 256;
const RECENT_BURSTS: usize = 128;
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
    /// Hop links between bursts separated by silence (included in `hop_links`).
    pub bursty_hop_links: u64,
    /// Contiguous hop candidates vetoed because the two channels were keyed at the same time
    /// (T-109: a hopper is on one channel at a time; co-keyed channels are separate emitters).
    pub hop_concurrent_vetoes: u64,
    /// Hop-set raster fits run (T-064: formation, channel-set changes, merges, closing).
    pub hop_raster_fits: u64,
    /// Closed hop-set members dropped (T-064: superseded on their channel, or over the cap).
    pub hop_members_pruned: u64,
    /// Track splits.
    pub splits: u64,
    /// Tentative tracks discarded without confirming (fragments).
    pub tentative_discarded: u64,
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
    /// The detection's threshold-crossing (pixel) box, containing `lo..hi` (T-102).
    px_lo: f64,
    px_hi: f64,
    bin_hz: f64,
    frame_ns: i64,
    close: CloseReason,
    continues: bool,
    at_segment_start: Option<bool>,
    preds: [Option<DetectionId>; 2],
    suspect: bool,
    marginal: bool,
    confirmed: bool,
    snr_db: f64,
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
    px_lo: f64,
    px_hi: f64,
    bin_hz: f64,
    frame_ns: i64,
    segment: u64,
    provenance: ProvenanceId,
    tune: TuneKey,
    continues: u32,
    transitions: u32,
    snr_db: f64,
}

impl Group {
    fn of(parts: &[Part]) -> Group {
        let p = parts[0];
        let mut g = Group {
            t0: p.t0,
            t1: p.t1,
            lo: p.lo,
            hi: p.hi,
            px_lo: p.px_lo,
            px_hi: p.px_hi,
            bin_hz: p.bin_hz,
            frame_ns: p.frame_ns,
            segment: p.segment,
            provenance: p.provenance,
            tune: p.tune,
            continues: 0,
            transitions: 0,
            snr_db: p.snr_db,
        };
        for p in parts {
            g.snr_db = g.snr_db.max(p.snr_db);
            g.t0 = g.t0.min(p.t0);
            g.t1 = g.t1.max(p.t1);
            g.lo = g.lo.min(p.lo);
            g.hi = g.hi.max(p.hi);
            g.px_lo = g.px_lo.min(p.px_lo);
            g.px_hi = g.px_hi.max(p.px_hi);
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
    /// Pixel-box edges' offsets below / above the linked groups' centres, Hz, averaged like `bw`
    /// (T-102): how far the emission's threshold-crossing skirts reach past its OBW.
    px_lo_off: f64,
    px_hi_off: f64,
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
    tentative: bool,
    opened: (i64, f64, f64),
    split_from: Option<TrackId>,
    shapes: [Shape; SHAPE_RING],
    shape_len: usize,
    shape_head: usize,
    /// Sum and count of linked groups' SNR, dB (hop raster weights).
    snr_sum: f64,
    snr_n: u64,
}

/// A finished burst's extent, for the split trigger.
#[derive(Clone, Copy, Debug, Default)]
struct Shape {
    fc: f64,
    bw: f64,
    t0: i64,
}

impl Slot {
    fn new(id: TrackId, seq: u64, g: &Group) -> Self {
        Self {
            live: true,
            id,
            seq,
            fc: g.fc(),
            bw: g.bw(),
            px_lo_off: g.fc() - g.px_lo,
            px_hi_off: g.px_hi - g.fc(),
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
            tentative: true,
            opened: (g.t0, g.fc(), g.bw()),
            split_from: None,
            shapes: [Shape::default(); SHAPE_RING],
            shape_len: 0,
            shape_head: 0,
            snr_sum: 0.0,
            snr_n: 0,
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

/// Recently closed continuous tracks kept as in-band hosts (T-101).
const CLOSED_HOSTS: usize = 32;
/// On-air share of its observed span a track needs to host in-band fragments (T-101).
const HOST_DUTY: f64 = 0.9;
/// A fragment's mean detection SNR is at least this far below its host's, dB (T-101): skirt
/// flicker sits near threshold, while a neighbouring emitter of comparable strength stays its own.
const FRAGMENT_SNR_MARGIN_DB: f64 = 6.0;

/// A continuous track's band, SNR and on-air span, for the in-band fragment rule (T-101).
#[derive(Clone, Copy, Debug)]
struct HostSpan {
    lo: f64,
    hi: f64,
    bw: f64,
    snr_db: f64,
    t_first: i64,
    t_last_end: i64,
}

/// Mean detection SNR of the groups linked to `s`, dB.
fn slot_snr(s: &Slot) -> Option<f64> {
    (s.snr_n > 0).then(|| s.snr_sum / s.snr_n as f64)
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
    bursts: u64,
    period_s: Option<f64>,
    snr_db: f64,
    /// End of the member track's latest burst, ns.
    t_last: i64,
}

impl HopMember {
    /// Counts towards a hop set: enough links, on enough of its bursts, clearly above the floor
    /// (T-084: `min_channel_snr_db`).
    fn qualifies(&self, h: &super::config::HopConfig) -> bool {
        self.links >= h.min_links_per_channel
            && f64::from(self.links) >= h.min_link_fraction * self.bursts as f64
            && self.snr_db >= h.min_channel_snr_db
    }
}

#[derive(Debug)]
struct HopSet {
    id: TrackId,
    members: Vec<HopMember>,
    links: u64,
    contiguous: u64,
    th_sum_ns: f64,
    t_first: i64,
    t_last: i64,
    formed: bool,
    dirty: bool,
    /// Raster of the latest fit (T-064: reused until the channel set changes).
    raster: Option<f64>,
    /// The qualifying members `(track, centre)` at the latest fit, in member order.
    fit: Vec<(TrackId, f64)>,
    /// Stream time of the latest fit, ns (`None`: never fitted).
    fit_at: Option<i64>,
    /// The channel set changed but the refit waits for `raster_refit_s`.
    raster_stale: bool,
    /// Detections of qualifying members dropped by pruning (still part of the set's count).
    retired_detections: u64,
}

impl HopSet {
    fn new(t0: i64, t1: i64) -> Self {
        Self {
            id: TrackId::new(),
            members: Vec::with_capacity(16),
            links: 0,
            contiguous: 0,
            th_sum_ns: 0.0,
            t_first: t0,
            t_last: t1,
            formed: false,
            dirty: false,
            raster: None,
            fit: Vec::new(),
            fit_at: None,
            raster_stale: false,
            retired_detections: 0,
        }
    }

    /// Drops closed members (T-064): those superseded on `m`'s channel by `m` (a qualifying
    /// member with a later last burst) whose last burst is older than `member_retention_s`, then
    /// the oldest closed members beyond `max_members`. Returns how many were dropped.
    fn prune(&mut self, m: &HopMember, hc: &HopConfig, now: i64) -> u64 {
        let before = self.members.len();
        let mut retired = 0;
        if hc.member_retention_s.is_finite() && m.qualifies(hc) {
            let retention = (hc.member_retention_s * NS) as i64;
            self.members.retain(|x| {
                let drop = !x.live
                    && x.track != m.track
                    && x.t_last < m.t_last
                    && now - x.t_last > retention
                    && (x.fc - m.fc).abs() < 0.5 * (x.bw + m.bw);
                if drop && x.qualifies(hc) {
                    retired += x.detections;
                }
                !drop
            });
        }
        while self.members.len() > hc.max_members {
            let Some(k) = (0..self.members.len())
                .filter(|&k| !self.members[k].live)
                .min_by_key(|&k| self.members[k].t_last)
            else {
                break;
            };
            let x = self.members.remove(k);
            if x.qualifies(hc) {
                retired += x.detections;
            }
        }
        self.retired_detections += retired;
        (before - self.members.len()) as u64
    }
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
    closed_hosts: [Option<HostSpan>; CLOSED_HOSTS],
    closed_hosts_head: usize,
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
    staged_segments: Vec<TrackSegment>,
    links: Vec<(TrackId, DetectionId)>,
    tentative_links: Vec<(TrackId, DetectionId)>,
    repoints: Vec<(TrackId, TrackId)>,
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
            closed_hosts: [None; CLOSED_HOSTS],
            closed_hosts_head: 0,
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
            staged_segments: Vec::with_capacity(64),
            links: Vec::with_capacity(4096),
            tentative_links: Vec::with_capacity(256),
            repoints: Vec::with_capacity(16),
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
            + self.staged_segments.capacity() * size_of::<TrackSegment>()
            + (self.links.capacity() + self.tentative_links.capacity())
                * size_of::<(TrackId, DetectionId)>()
            + self.repoints.capacity() * size_of::<(TrackId, TrackId)>()
            + self
                .hop_sets
                .iter()
                .flatten()
                .map(|h| {
                    size_of::<HopSet>()
                        + h.members.capacity() * size_of::<HopMember>()
                        + h.fit.capacity() * size_of::<(TrackId, f64)>()
                })
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
            px_lo: r.f_lo_hz.min(r.f_hi_hz).min(lo),
            px_hi: r.f_hi_hz.max(r.f_lo_hz).max(hi),
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
            snr_db: d.snr_mean_db,
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

    /// Summaries of the open, confirmed tracks.
    pub fn summaries(&self) -> Vec<TrackSummary> {
        (0..self.slots.len())
            .filter(|&i| self.slots[i].live && !self.slots[i].tentative)
            .map(|i| self.summary(i, None))
            .collect()
    }

    /// T-109: summaries of the open, confirmed tracks whose inventory fate is settled enough to
    /// offer before they close, into `out` (cleared first; summaries are built only for these):
    /// at least `min_bursts` bursts, no hop link and no hop set, formed or pending (a member's
    /// inventory row is its hop set's), and not an in-band fragment of a live or recently closed
    /// continuous host (the T-101 rule, evaluated now against the live host set).
    pub fn live_offers_into(&self, min_bursts: u64, out: &mut Vec<TrackSummary>) {
        out.clear();
        for (i, s) in self.slots.iter().enumerate() {
            if !s.live
                || s.tentative
                || s.bursts < min_bursts
                || s.hop_links > 0
                || s.hop_set.is_some()
                || self.inband_fragment(i, true)
            {
                continue;
            }
            out.push(self.summary(i, None));
        }
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

    /// Moves staged track rows (closed, merged, split, hop sets) and every changed open confirmed
    /// track, plus the new track↔detection links and merge re-points, into `batch`. Call it
    /// regularly; links accumulate until then. Tentative tracks and their links stay held.
    pub fn drain_into(&mut self, batch: &mut TrackBatch) {
        batch.upserts.append(&mut self.staged);
        for i in 0..self.slots.len() {
            if self.slots[i].live && self.slots[i].dirty && !self.slots[i].tentative {
                let t = self.summary(i, None).track;
                batch.upserts.push(t);
                self.slots[i].dirty = false;
            }
        }
        // T-064: a changed set's raster is refitted only when its channel set changed since the
        // latest fit, at most once per `raster_refit_s` (a deferred refit is picked up by a later
        // drain even without new links).
        let refit_ns = (self.cfg.hop.raster_refit_s.max(0.0) * NS) as i64;
        for k in 0..self.hop_sets.len() {
            let Some(h) = self.hop_sets[k].as_ref() else {
                continue;
            };
            if !h.formed || !(h.dirty || h.raster_stale) {
                continue;
            }
            let mut push = h.dirty;
            if self.hop_fit_changed(h) {
                if h.fit_at.is_none_or(|t| self.now - t >= refit_ns) {
                    let old = h.raster.map(f64::to_bits);
                    self.refit_hop_raster(k);
                    push |= self.hop_sets[k].as_ref().unwrap().raster.map(f64::to_bits) != old;
                } else {
                    self.hop_sets[k].as_mut().unwrap().raster_stale = true;
                }
            } else {
                self.hop_sets[k].as_mut().unwrap().raster_stale = false;
            }
            if push {
                let t = self.hop_track(self.hop_sets[k].as_ref().unwrap(), false);
                batch.upserts.push(t);
                self.hop_sets[k].as_mut().unwrap().dirty = false;
            }
        }
        batch.links.append(&mut self.links);
        batch.repoints.append(&mut self.repoints);
        batch.segments.append(&mut self.staged_segments);
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
        // T-102: the detector's continuation of a max-duration split can re-open on the split
        // record's last frame or two (1..470 then 469..938 on a real WFM station), not only on
        // the frame after it. An exact-adjacency match missed those, the continuation looked like
        // a natural start, and tone-lobe aggregation fused it with co-timed neighbour stations
        // (a 333 kHz station track grew to 410 kHz).
        let overlap = self.cfg.coincidence_frames.max(0.0).ceil() as u64;
        for e in self.splits.iter().flatten() {
            if e.segment == segment
                && frames_start <= e.frames_end
                && frames_start + overlap >= e.frames_end
                && e.lo < hi
                && lo < e.hi
            {
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
        i
    }

    /// Confirms tentative track `i` if it has enough evidence.
    fn maybe_confirm<F>(&mut self, i: usize, out: &mut F)
    where
        F: FnMut(TrackEvent),
    {
        let s = &self.slots[i];
        if s.tentative
            && (s.bursts >= self.cfg.confirm_bursts
                || s.on_ns as f64 >= self.cfg.confirm_on_time_s * NS
                || s.hop_links > 0)
        {
            self.confirm_track(i, out);
        }
    }

    /// T-101: closing track `i` is an in-band fragment — its centre lies inside a continuous
    /// track's (duty ≥ [`HOST_DUTY`]) detected extent (T-102: the wider of its OBW and its
    /// detections' threshold-crossing boxes, plus the frequency tolerance; skirt flicker is
    /// centred where the host itself crosses threshold and may straddle that edge by half its own
    /// width, while a weak emitter one channel off is centred beyond it),
    /// that extent is at least [`TrackerConfig::inband_fragment_bw_ratio`] times wider and
    /// [`FRAGMENT_SNR_MARGIN_DB`] stronger (mean detection SNR), and `i`'s whole observed life lies
    /// inside the track's. The host is live or among the recently closed continuous tracks.
    ///
    /// `live` (T-109, evaluating an open track for a live offer): a live host's duty is measured
    /// over its recorded life (`t_first..t_last_end`), since its current record is only reported
    /// when it closes (up to the detector's max record duration behind `now`).
    fn inband_fragment(&self, i: usize, live: bool) -> bool {
        let ratio = self.cfg.inband_fragment_bw_ratio;
        let s = &self.slots[i];
        if ratio <= 0.0 || s.bursts == 0 {
            return false;
        }
        let Some(s_snr) = slot_snr(s) else {
            return false;
        };
        let fits = |h: &HostSpan| {
            h.bw >= ratio * s.bw
                && h.lo <= s.fc
                && s.fc <= h.hi
                && h.snr_db >= s_snr + FRAGMENT_SNR_MARGIN_DB
                && h.t_first <= s.t_first
                && h.t_last_end >= s.t_last_end
        };
        self.slots
            .iter()
            .enumerate()
            .filter(|&(j, h)| j != i && h.live && !h.tentative)
            .filter_map(|(_, h)| self.host_span(h, live))
            .chain(self.closed_hosts.iter().flatten().copied())
            .any(|h| fits(&h))
    }

    /// Track `h` as a potential in-band host: continuous (duty ≥ [`HOST_DUTY`]) with an SNR.
    /// `live`: duty over the recorded life only (see [`Self::inband_fragment`]).
    fn host_span(&self, h: &Slot, live: bool) -> Option<HostSpan> {
        let end = if h.cur.is_some() {
            self.now.max(h.t_last_end)
        } else {
            h.t_last_end
        };
        let span = if live && h.cur.is_some() {
            (h.t_last_end - h.t_first).max(1)
        } else {
            (end - h.t_first).max(1)
        };
        let snr_db = slot_snr(h)?;
        let tol = self.cfg.freq_tolerance_bins * h.bin_hz;
        let (below, above) = (h.px_lo_off.max(0.5 * h.bw), h.px_hi_off.max(0.5 * h.bw));
        (h.on_ns as f64 >= HOST_DUTY * span as f64).then_some(HostSpan {
            lo: h.fc - below - tol,
            hi: h.fc + above + tol,
            bw: below + above,
            snr_db,
            t_first: h.t_first,
            t_last_end: end,
        })
    }

    /// Confirms track `i`: `Opened` (with its first burst), its held links released.
    fn confirm_track<F>(&mut self, i: usize, out: &mut F)
    where
        F: FnMut(TrackEvent),
    {
        let s = &mut self.slots[i];
        if !s.tentative {
            return;
        }
        s.tentative = false;
        s.dirty = true;
        let (id, (at, fc, bw)) = (s.id, s.opened);
        self.stats.tracks_opened += 1;
        out(TrackEvent::Opened {
            track: id,
            at: Timestamp::from_unix_nanos(at),
            f_center_hz: fc,
            bandwidth_hz: bw,
        });
        self.release_links(id);
    }

    fn release_links(&mut self, id: TrackId) {
        let mut k = 0;
        while k < self.tentative_links.len() {
            if self.tentative_links[k].0 == id {
                self.links.push(self.tentative_links.remove(k));
            } else {
                k += 1;
            }
        }
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
        let boundary = {
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
            if kind.is_some() {
                s.segments += 1;
            }
            let boundary = kind.filter(|_| !s.tentative).map(|kind| SegmentBoundary {
                track: s.id,
                at: Timestamp::from_unix_nanos(g.t0),
                kind,
                from: s.provenance,
                to: g.provenance,
            });
            s.provenance = g.provenance;
            s.tune = g.tune;
            s.segment = s.segment.max(g.segment);
            boundary
        };
        if let Some(b) = boundary {
            self.staged_segments.push(TrackSegment {
                track: b.track,
                at: b.at,
                kind: b.kind.into(),
            });
            out(TrackEvent::Segment(b));
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
            let (lo_off, hi_off) = (g.fc() - g.px_lo, g.px_hi - g.fc());
            if s.shape_n < 8 {
                s.fc = (s.fc * n + g.fc()) / (n + 1.0);
                s.bw = (s.bw * n + g.bw()) / (n + 1.0);
                s.px_lo_off = (s.px_lo_off * n + lo_off) / (n + 1.0);
                s.px_hi_off = (s.px_hi_off * n + hi_off) / (n + 1.0);
            } else {
                s.fc += (g.fc() - s.fc) / 8.0;
                s.bw += (g.bw() - s.bw) / 8.0;
                s.px_lo_off += (lo_off - s.px_lo_off) / 8.0;
                s.px_hi_off += (hi_off - s.px_hi_off) / 8.0;
            }
            s.shape_n += 1;
            s.bin_hz = s.bin_hz.min(g.bin_hz);
            s.snr_sum += g.snr_db;
            s.snr_n += 1;
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
            let tentative = s.tentative;
            for k in 0..self.scratch.len() {
                let p = self.scratch[k];
                if tentative {
                    self.tentative_links.push((track, p.id));
                } else {
                    self.links.push((track, p.id));
                }
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
        self.maybe_confirm(i, out);
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
                s.shapes[s.shape_head] = Shape {
                    fc: 0.5 * (c.lo + c.hi),
                    bw: c.hi - c.lo,
                    t0: c.t0,
                };
                s.shape_head = (s.shape_head + 1) % SHAPE_RING;
                s.shape_len = (s.shape_len + 1).min(SHAPE_RING);
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
            // Only a closer abutment can replace `best`; skip the veto scan otherwise.
            if best.is_some_and(|(_, g)| gap.abs() >= g) {
                continue;
            }
            // T-109: a hopper is on one channel at a time. When this track was also keyed during
            // `e` (or `e`'s track during `c`), the abutment is two co-keyed emitters repeating (a
            // pager net's channels keying together), not a hop. Overlap within the link tolerance
            // (`gap_tol`: box extension of successive dwells) is not co-keying.
            if self.keyed_during(s.id, c.t0, e.t0, e.t1, gap_tol)
                || self.keyed_during(e.track, e.t0, c.t0, c.t1, gap_tol)
            {
                self.stats.hop_concurrent_vetoes += 1;
                continue;
            }
            best = Some((*e, gap.abs()));
        }
        let me = RecentBurst {
            slot: i,
            track: s.id,
            t0: c.t0,
            t1: c.t1,
            fc: s.fc,
            bw: s.bw,
            len_ns: c.on_ns,
        };
        let tol = (h.gap_frames * c.frame_ns as f64) as i64;
        let bursty = if best.is_none() && h.max_silence_s > 0.0 {
            self.bursty_predecessor(&me, tol, floor)
        } else {
            None
        };
        self.recent[self.recent_head] = Some(me);
        self.recent_head = (self.recent_head + 1) % RECENT_BURSTS;
        if let Some((e, _)) = best {
            self.hop_link(e.slot, i, c.t0 - e.t0, e.t0, c.t1, true, out);
        } else if let Some(e) = bursty {
            self.stats.bursty_hop_links += 1;
            self.hop_link(e.slot, i, c.t0 - e.t0, e.t0, c.t1, false, out);
        }
    }

    /// Whether a recent burst of `track` other than the one starting at `own_t0` overlaps
    /// `t0..t1` by more than `tol` (T-109 concurrency veto for contiguous hop links).
    fn keyed_during(&self, track: TrackId, own_t0: i64, t0: i64, t1: i64, tol: i64) -> bool {
        self.recent
            .iter()
            .flatten()
            .any(|r| r.track == track && r.t0 != own_t0 && r.t0 < t1 - tol && t0 < r.t1 - tol)
    }

    /// Bursts separated by silence: `x`'s nearest similar predecessor `e` on another channel,
    /// provided `e`'s own nearest similar predecessor is on a third channel, the three centres
    /// share a raster, and neither `x`'s nor `e`'s track is periodic. Two channels alone never
    /// link: two interleaved emitters look exactly like a two-channel hopper.
    fn bursty_predecessor(&self, x: &RecentBurst, tol: i64, floor: f64) -> Option<RecentBurst> {
        let distinct =
            |a: &RecentBurst, b: &RecentBurst| (a.fc - b.fc).abs() >= 0.5 * (a.bw + b.bw);
        let e = self.nearest_similar(x, tol, floor)?;
        if e.track == x.track || !distinct(x, &e) {
            return None;
        }
        let p = self.nearest_similar(&e, tol, floor)?;
        if p.track == e.track || p.track == x.track || !distinct(&p, &e) || !distinct(&p, x) {
            return None;
        }
        let mut fcs = [x.fc, e.fc, p.fc];
        fcs.sort_unstable_by(f64::total_cmp);
        raster(
            &fcs,
            1.5 * self.slots[x.slot].bin_hz.max(self.slots[e.slot].bin_hz),
        )?;
        if self.period(x.slot).is_some() || self.period(e.slot).is_some() {
            return None;
        }
        Some(e)
    }

    /// The latest-ending recent burst of similar bandwidth and length that ended before `x`
    /// started (within `tol`) and at most `max_silence_s` earlier, on any live track (`x`'s
    /// included). `None` when a similar burst on another track overlaps `x` in time: concurrent
    /// packets are not one hopper.
    fn nearest_similar(&self, x: &RecentBurst, tol: i64, floor: f64) -> Option<RecentBurst> {
        let h = self.cfg.hop;
        let silence = (h.max_silence_s * NS) as i64;
        let mut best: Option<RecentBurst> = None;
        for e in self.recent.iter().flatten() {
            if (e.track == x.track && e.t0 == x.t0)
                || !self.slots[e.slot].live
                || self.slots[e.slot].id != e.track
            {
                continue;
            }
            let (b1, b2) = (x.bw.max(floor), e.bw.max(floor));
            let (l1, l2) = (x.len_ns.max(1) as f64, e.len_ns.max(1) as f64);
            if b1.max(b2) / b1.min(b2) > h.bandwidth_ratio
                || l1.max(l2) / l1.min(l2) > h.bursty_length_ratio
            {
                continue;
            }
            if e.t0 >= x.t0 || e.t1 > x.t0 + tol {
                let overlaps = e.t0 < x.t1 && x.t0 < e.t1 - tol;
                if overlaps && e.track != x.track {
                    return None;
                }
                continue;
            }
            if x.t0 - e.t1 > silence {
                continue;
            }
            if best.is_none_or(|b| e.t1 > b.t1) {
                best = Some(*e);
            }
        }
        best
    }

    /// Track `slot`'s period when it repeats on a lattice with `periodic_veto_*` evidence.
    fn period(&self, slot: usize) -> Option<f64> {
        let h = self.cfg.hop;
        let s = &self.slots[slot];
        if s.bursts < h.periodic_veto_bursts {
            return None;
        }
        let mut buf = [0i64; START_RING];
        let n = s.starts.sorted_into(&mut buf);
        fold_period(&buf[..n], &self.cfg.period)
            .filter(|p| {
                p.bursts as u64 >= h.periodic_veto_bursts
                    && p.confidence >= h.periodic_veto_confidence
            })
            .map(|p| p.period_s)
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
            bursts: s.bursts,
            period_s: self.period(slot),
            snr_db: if s.snr_n > 0 {
                s.snr_sum / s.snr_n as f64
            } else {
                0.0
            },
            t_last: s.t_last_end,
        }
    }

    fn refresh_member(&mut self, set: usize, slot: usize) {
        let m = self.hop_member(slot);
        let (hc, now) = (self.cfg.hop, self.now);
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
            self.stats.hop_members_pruned += h.prune(&m, &hc, now);
        }
    }

    /// Whether set `h`'s qualifying channels differ from its latest fit (T-064): a member joined,
    /// left or changed qualification, or a centre moved by more than `raster_drift_bins`. Always
    /// true with `raster_drift_bins` 0 (refit on every drain, as before T-064).
    fn hop_fit_changed(&self, h: &HopSet) -> bool {
        let hc = &self.cfg.hop;
        if h.fit_at.is_none() || hc.raster_drift_bins <= 0.0 {
            return true;
        }
        let mut fit = h.fit.iter();
        for m in h.members.iter().filter(|m| m.qualifies(hc)) {
            match fit.next() {
                Some(&(track, fc))
                    if track == m.track && (m.fc - fc).abs() <= hc.raster_drift_bins * m.bin_hz => {
                }
                _ => return true,
            }
        }
        fit.next().is_some()
    }

    /// Fits set `k`'s raster now and records the channel set it was fitted on.
    fn refit_hop_raster(&mut self, k: usize) {
        let Some(h) = self.hop_sets[k].as_ref() else {
            return;
        };
        let raster = self.hop_raster(h);
        self.stats.hop_raster_fits += 1;
        let (hc, now) = (self.cfg.hop, self.now);
        let h = self.hop_sets[k].as_mut().unwrap();
        h.raster = raster;
        h.fit_at = Some(now);
        h.raster_stale = false;
        h.fit.clear();
        h.fit.extend(
            h.members
                .iter()
                .filter(|m| m.qualifies(&hc))
                .map(|m| (m.track, m.fc)),
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn hop_link<F>(
        &mut self,
        a: usize,
        b: usize,
        th_ns: i64,
        t0: i64,
        t1: i64,
        contiguous: bool,
        out: &mut F,
    ) where
        F: FnMut(TrackEvent),
    {
        self.stats.hop_links += 1;
        self.slots[a].hop_links += 1;
        self.slots[b].hop_links += 1;
        self.maybe_confirm(a, out);
        self.maybe_confirm(b, out);
        let set = match (self.slots[a].hop_set, self.slots[b].hop_set) {
            (None, None) => {
                let set = HopSet::new(t0, t1);
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
        if self.hop_sets[set].as_ref().is_some_and(|h| !h.formed) {
            // Formation is judged on the members' current state, not their last-link snapshots.
            let n = self.hop_sets[set].as_ref().map_or(0, |h| h.members.len());
            for k in 0..n {
                let m = self.hop_sets[set].as_ref().unwrap().members[k];
                let s = &self.slots[m.slot];
                if s.live && s.id == m.track {
                    let fresh = self.hop_member(m.slot);
                    self.hop_sets[set].as_mut().unwrap().members[k] = fresh;
                }
            }
        }
        let hc = self.cfg.hop;
        let now = self.now;
        let formed_now = {
            let h = self.hop_sets[set].as_mut().expect("hop set");
            h.links += 1;
            h.contiguous += u64::from(contiguous);
            h.th_sum_ns += th_ns as f64;
            h.t_first = h.t_first.min(t0);
            h.t_last = h.t_last.max(t1);
            h.dirty = true;
            // Periodic channels are independent emitters, not hop channels, when the links are
            // mostly bursty or their periods disagree (a cyclic hopper's channels share one).
            let bursty = 2 * h.contiguous < h.links;
            let (pmin, pmax) = h
                .members
                .iter()
                .filter(|m| m.qualifies(&hc))
                .filter_map(|m| m.period_s)
                .fold((f64::INFINITY, 0.0f64), |(a, b), p| (a.min(p), b.max(p)));
            let drop_periodic = bursty || pmax > 1.1 * pmin;
            let channels = h
                .members
                .iter()
                .filter(|m| m.qualifies(&hc) && !(drop_periodic && m.period_s.is_some()))
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
            self.refit_hop_raster(set);
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
        let mut other = self.hop_sets[drop].take().unwrap();
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
            h.contiguous += other.contiguous;
            h.th_sum_ns += other.th_sum_ns;
            h.t_first = h.t_first.min(other.t_first);
            h.t_last = h.t_last.max(other.t_last);
            h.retired_detections += other.retired_detections;
            h.dirty = true;
        }
        if other_formed {
            self.refit_hop_raster(keep);
            other.raster = self.hop_raster(&other);
            self.stats.hop_raster_fits += 1;
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
            .filter(|m| m.qualifies(&self.cfg.hop))
            .collect();
        qual.sort_by(|a, b| a.fc.total_cmp(&b.fc));
        let n = qual.len().max(1) as f64;
        let bin = qual.iter().map(|m| m.bin_hz).fold(0.0, f64::max);
        let bw = qual.iter().map(|m| m.bw).sum::<f64>() / n;
        let dwell = qual.iter().map(|m| m.dwell_s).sum::<f64>() / n;
        let detections = qual.iter().map(|m| m.detections).sum::<u64>() + h.retired_detections;
        (
            qual.iter().map(|m| m.fc).collect(),
            qual.iter().map(|m| m.track).collect(),
            bin,
            bw,
            detections,
            dwell,
        )
    }

    /// Raster of the qualifying members' centres, robust to noisy centre estimates (T-035). The
    /// fit costs `O(channels²)` channel fits: summaries and rows read the cached `HopSet::raster`
    /// instead, refitted by [`Tracker::refit_hop_raster`] (T-064).
    fn hop_raster(&self, h: &HopSet) -> Option<f64> {
        let qual = || h.members.iter().filter(|m| m.qualifies(&self.cfg.hop));
        let bin = qual().map(|m| m.bin_hz).fold(0.0, f64::max);
        let channels: Vec<RasterChannel> = qual()
            .map(|m| RasterChannel {
                fc: m.fc,
                bw: m.bw,
                bursts: m.bursts,
                snr_db: m.snr_db,
            })
            .collect();
        robust_raster(&channels, 1.5 * bin)
    }

    fn hop_summary(&self, h: &HopSet) -> HopSetSummary {
        let (channels, members, _, _, _, dwell) = self.hop_channels(h);
        HopSetSummary {
            id: h.id,
            raster_hz: h.raster,
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
                hop_raster_hz: h.raster,
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
            if !self.try_split(i, id, out) {
                self.try_merge(i, id, out);
            }
        }
    }

    /// Split trigger: track `i`'s latest `SHAPE_RING` bursts form two clusters in centre (or
    /// bandwidth), each with `min_per_cluster` bursts and each active in both halves of the
    /// window. The larger cluster (tie: the one active first) continues; the other opens a new
    /// track with `split_from`. History and links stay on the parent.
    fn try_split<F>(&mut self, i: usize, id: TrackId, out: &mut F) -> bool
    where
        F: FnMut(TrackEvent),
    {
        let sc = self.cfg.split;
        let s = &self.slots[i];
        if !sc.enabled
            || !s.live
            || s.id != id
            || s.tentative
            || s.hop_set.is_some()
            || s.cur.is_some()
            || s.shape_len < SHAPE_RING
        {
            return false;
        }
        let n = SHAPE_RING;
        let m = sc.min_per_cluster.clamp(1, n / 2);
        let floor = self.cfg.freq_tolerance_bins * s.bin_hz;
        let mut sh = s.shapes;
        let bw_mean = sh.iter().map(|x| x.bw).sum::<f64>() / n as f64;
        let eps = floor.max(self.cfg.freq_tolerance_fraction * bw_mean.max(floor));
        sh.sort_unstable_by(|a, b| a.fc.total_cmp(&b.fc));
        let mut cut = best_cut(&sh.map(|x| x.fc), m)
            .filter(|&(_, sep, sd)| {
                sep >= sc.min_separation_eps * eps && sep >= sc.separation_sigma * sd
            })
            .map(|(k, _, _)| k);
        if cut.is_none() {
            sh.sort_unstable_by(|a, b| a.bw.total_cmp(&b.bw));
            cut = best_cut(&sh.map(|x| x.bw.max(floor).ln()), m)
                .filter(|&(_, sep, sd)| {
                    sep >= sc.bandwidth_ratio.ln() && sep >= sc.separation_sigma * sd
                })
                .map(|(k, _, _)| k);
        }
        let Some(k) = cut else { return false };
        let (lo_c, hi_c) = sh.split_at(k);
        let tmin = sh.iter().map(|x| x.t0).min().unwrap_or(0);
        let tmax = sh.iter().map(|x| x.t0).max().unwrap_or(0);
        let mid = tmin + (tmax - tmin) / 2;
        let sustained = |c: &[Shape]| c.iter().any(|x| x.t0 < mid) && c.iter().any(|x| x.t0 >= mid);
        if !sustained(lo_c) || !sustained(hi_c) {
            return false;
        }
        let first = |c: &[Shape]| c.iter().map(|x| x.t0).min().unwrap_or(i64::MAX);
        let lo_stays =
            lo_c.len() > hi_c.len() || (lo_c.len() == hi_c.len() && first(lo_c) <= first(hi_c));
        let (stay, go) = if lo_stays { (lo_c, hi_c) } else { (hi_c, lo_c) };
        let mean =
            |c: &[Shape], f: fn(&Shape) -> f64| c.iter().map(f).sum::<f64>() / c.len() as f64;
        let (par_fc, par_bw) = (mean(stay, |x| x.fc), mean(stay, |x| x.bw));
        let (ch_fc, ch_bw) = (mean(go, |x| x.fc), mean(go, |x| x.bw));
        let mut go_t0 = [0i64; SHAPE_RING];
        for (d, x) in go_t0.iter_mut().zip(go) {
            *d = x.t0;
        }
        let go_t0 = &go_t0[..go.len()];
        let parent = {
            let s = &mut self.slots[i];
            s.fc = par_fc;
            s.bw = par_bw;
            // The shapes keep no pixel extent: the parent's skirt is re-measured from here.
            s.px_lo_off = 0.5 * par_bw;
            s.px_hi_off = 0.5 * par_bw;
            s.shape_len = 0;
            s.shape_head = 0;
            let mut buf = [0i64; START_RING];
            let k = s.starts.sorted_into(&mut buf);
            let mut ring = StartRing::default();
            for &t in &buf[..k] {
                if !go_t0.contains(&t) {
                    ring.push(t);
                }
            }
            s.starts = ring;
            s.dirty = true;
            *s
        };
        // The parent row precedes the child's (split_from references it).
        let row = self.summary(i, None).track;
        self.staged.push(row);
        let g = Group {
            t0: self.now,
            t1: self.now,
            lo: ch_fc - 0.5 * ch_bw,
            hi: ch_fc + 0.5 * ch_bw,
            px_lo: ch_fc - 0.5 * ch_bw,
            px_hi: ch_fc + 0.5 * ch_bw,
            bin_hz: parent.bin_hz,
            frame_ns: parent.last.map_or(1, |l| l.frame_ns),
            segment: parent.segment,
            provenance: parent.provenance,
            tune: parent.tune,
            continues: 0,
            transitions: 0,
            snr_db: if parent.snr_n > 0 {
                parent.snr_sum / parent.snr_n as f64
            } else {
                0.0
            },
        };
        let j = self.open(&g, out);
        {
            let c = &mut self.slots[j];
            c.split_from = Some(parent.id);
            c.shape_n = go.len() as u64;
            if parent.snr_n > 0 {
                c.snr_sum = g.snr_db;
                c.snr_n = 1;
            }
        }
        self.confirm_track(j, out);
        self.stats.splits += 1;
        out(TrackEvent::TrackSplit {
            from: parent.id,
            into: self.slots[j].id,
            at: Timestamp::from_unix_nanos(self.now),
        });
        true
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
            if j == i
                || !b.live
                || b.hop_set.is_some()
                || a.split_from == Some(b.id)
                || b.split_from == Some(a.id)
            {
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
            t.px_lo_off = (t.px_lo_off * nt + f.px_lo_off * nf) / (nt + nf);
            t.px_hi_off = (t.px_hi_off * nt + f.px_hi_off * nf) / (nt + nf);
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
            t.snr_sum += f.snr_sum;
            t.snr_n += f.snr_n;
            if t.cur.is_none() {
                t.cur = f.cur;
            }
            if t.last.is_none_or(|l| f.last.is_some_and(|fl| fl.t1 > l.t1)) {
                t.last = f.last;
            }
            t.dirty = true;
        }
        let into_id = self.slots[into].id;
        for l in self.tentative_links.iter_mut() {
            if l.0 == f.id {
                l.0 = into_id;
            }
        }
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
        // A merge with a confirmed track confirms the survivor; a tentative (never announced)
        // fragment is absorbed silently.
        if f.tentative {
            self.maybe_confirm(into, out);
        } else {
            self.confirm_track(into, out);
        }
        if !self.slots[into].tentative {
            self.release_links(into_id);
        }
        if !f.tentative {
            let target = self.summary(into, None).track;
            self.staged.push(target);
            let mut merged = self.summary(from, None).track;
            merged.state = TrackState::MergedInto(into_id);
            self.staged.push(merged);
            self.repoints.push((f.id, into_id));
            self.stats.merges += 1;
            out(TrackEvent::Merged {
                from: f.id,
                into: into_id,
                at: Timestamp::from_unix_nanos(self.now),
            });
        }
        self.slots[from].live = false;
        self.free.push(from);
        self.live -= 1;
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
        if self.slots[i].tentative {
            // Never confirmed (a fragment): no Opened was emitted, nothing is persisted.
            let id = self.slots[i].id;
            self.tentative_links.retain(|l| l.0 != id);
            self.slots[i].live = false;
            self.free.push(i);
            self.live -= 1;
            self.stats.tentative_discarded += 1;
            return;
        }
        let summary = self.summary(i, Some(cause));
        if self.cfg.inband_fragment_bw_ratio > 0.0
            && let Some(h) = self.host_span(&self.slots[i], false)
        {
            self.closed_hosts[self.closed_hosts_head] = Some(h);
            self.closed_hosts_head = (self.closed_hosts_head + 1) % CLOSED_HOSTS;
        }
        self.staged.push(summary.track.clone());
        self.slots[i].live = false;
        if let Some(set) = self.slots[i].hop_set {
            self.refresh_member(set, i);
            let done = self.hop_sets[set]
                .as_ref()
                .is_none_or(|h| h.members.iter().all(|m| !m.live));
            if done {
                if let Some(mut h) = self.hop_sets[set].take() {
                    if h.formed {
                        // Closing always refits: the closed row and event are exact.
                        h.raster = self.hop_raster(&h);
                        self.stats.hop_raster_fits += 1;
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
            split_from: s.split_from,
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
                period_confidence: period.map(|p| p.confidence),
                period_jitter_s: period.map(|p| p.jitter_s),
                burst_length: burst_length.map(Into::into),
                segment_count: s.segments,
                hop_set,
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
            inband_fragment: closed.is_some() && self.inband_fragment(i, false),
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

/// Best two-cluster cut of `sorted` values (1-D 2-means, each side ≥ `m`): `(cut index, mean
/// separation, pooled within-cluster standard deviation)`.
fn best_cut(sorted: &[f64; SHAPE_RING], m: usize) -> Option<(usize, f64, f64)> {
    let n = SHAPE_RING;
    let mut best: Option<(usize, f64, f64)> = None;
    for k in m..=n - m {
        let (l, r) = sorted.split_at(k);
        let ml = l.iter().sum::<f64>() / l.len() as f64;
        let mr = r.iter().sum::<f64>() / r.len() as f64;
        let ss = l.iter().map(|x| (x - ml).powi(2)).sum::<f64>()
            + r.iter().map(|x| (x - mr).powi(2)).sum::<f64>();
        if best.is_none_or(|(_, _, b)| ss < b) {
            best = Some((k, mr - ml, ss));
        }
    }
    best.map(|(k, sep, ss)| (k, sep, (ss / (n - 2) as f64).sqrt()))
}
