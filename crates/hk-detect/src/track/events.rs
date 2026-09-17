//! What the tracker emits: [`TrackEvent`]s (appended, never overwritten) and the summaries they
//! carry.

use hk_model::{BurstLengths, ProvenanceId, SegmentKind, TimeRange, Timestamp, Track, TrackId};

pub use super::stats::Periodicity;

/// Why a track's member stream crossed a segment boundary. The track continues.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundaryKind {
    /// LNA/VGA/amp changed.
    GainChange,
    /// The tuned centre changed.
    Retune,
    /// Another provenance change (overload flag, filter, clock…).
    Provenance,
    /// A detector transition (gap, floor-segment reset) without a provenance change.
    Discontinuity,
}

impl From<BoundaryKind> for SegmentKind {
    fn from(k: BoundaryKind) -> Self {
        match k {
            BoundaryKind::GainChange => SegmentKind::GainChange,
            BoundaryKind::Retune => SegmentKind::Retune,
            BoundaryKind::Provenance => SegmentKind::Provenance,
            BoundaryKind::Discontinuity => SegmentKind::Discontinuity,
        }
    }
}

/// A segment boundary inside a track.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SegmentBoundary {
    /// Track.
    pub track: TrackId,
    /// Start of the first member after the boundary.
    pub at: Timestamp,
    /// Kind.
    pub kind: BoundaryKind,
    /// Provenance before.
    pub from: ProvenanceId,
    /// Provenance after.
    pub to: ProvenanceId,
}

/// Why a track closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseCause {
    /// Idle longer than its timeout (observed time).
    Idle,
    /// [`Tracker::finish`](super::Tracker::finish).
    EndOfStream,
    /// The live-track cap was reached and this was the stalest.
    Capacity,
}

/// Burst-length distribution (running moments + log histogram quantiles, ±15 %).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Distribution {
    /// Bursts.
    pub count: u64,
    /// Mean, s.
    pub mean_s: f64,
    /// Standard deviation, s (0 for one burst).
    pub std_s: f64,
    /// Shortest, s.
    pub min_s: f64,
    /// Longest, s.
    pub max_s: f64,
    /// Median, s.
    pub p50_s: f64,
    /// 90th percentile, s.
    pub p90_s: f64,
}

impl From<Distribution> for BurstLengths {
    fn from(d: Distribution) -> Self {
        BurstLengths {
            count: d.count,
            mean_s: d.mean_s,
            std_s: d.std_s,
            min_s: d.min_s,
            p50_s: d.p50_s,
            p90_s: d.p90_s,
            max_s: d.max_s,
        }
    }
}

/// The observed time extent of one open track (T-388): what
/// [`Tracker::live_extents_into`](crate::Tracker::live_extents_into) publishes every flush so a
/// live signal's box can grow without waiting for the 5 s inventory offer.
///
/// It is deliberately two timestamps and an id and **nothing else**. A presence extension is new
/// *time*, not new geometry: the frequency edges of the box on screen came from the inventory row
/// and are not restated here, so a fast path can never disagree with the slow one about where a
/// signal is — only about how far it has been heard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiveExtent {
    /// The open track.
    pub track: TrackId,
    /// First member's start, stream ns.
    pub t_start_ns: i64,
    /// **End of the last burst actually measured**, stream ns — never a clock read. A track that
    /// has gone quiet keeps the end it went quiet at.
    pub t_end_ns: i64,
}

/// A track with every C10 feature, including those docs/07's `Track` does not store yet.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackSummary {
    /// The docs/07 aggregate as persisted.
    pub track: Track,
    /// Bursts (tone lobes and split/transition continuations count once).
    pub burst_count: u64,
    /// On-air time (union of member extents), s.
    pub on_time_s: f64,
    /// Observed time the duty cycle is normalised by, s.
    pub observed_s: f64,
    /// Periodicity, when the fold found one.
    pub period: Option<Periodicity>,
    /// Burst lengths (finished bursts).
    pub burst_length: Option<Distribution>,
    /// Inter-arrival coefficient of variation.
    pub inter_arrival_cv: Option<f64>,
    /// Segment boundaries crossed.
    pub segments: u32,
    /// Hop set this channel belongs to.
    pub hop_set: Option<TrackId>,
    /// Closed inside (or in the skirt of) a continuous, much wider and ≥ 6 dB stronger track that
    /// covered its whole life ([`crate::TrackerConfig::inband_fragment_bw_ratio`]): modulation/edge
    /// flicker of that emission, not an emitter of its own. It gets no inventory entry (T-101).
    pub inband_fragment: bool,
    /// Share of member detections with a suspect flag (spur, image, IMD, compressed, clipped).
    pub suspect_fraction: f64,
    /// Member detections confirmed as emitter candidates (at emission or later).
    pub confirmed_detections: u64,
    /// `last start + period`, when periodic.
    pub next_burst_eta: Option<Timestamp>,
    /// Why it closed, if it has.
    pub closed: Option<CloseCause>,
}

/// A hop set: channel tracks linked by contiguous, similar dwells on a raster.
#[derive(Clone, Debug, PartialEq)]
pub struct HopSetSummary {
    /// Hop-set id: the id of its aggregate Track (`timing.hop_set_hz`, `co_occurring` = members).
    pub id: TrackId,
    /// Member channel centres, Hz, ascending.
    pub channels_hz: Vec<f64>,
    /// Member channel tracks, in `channels_hz` order.
    pub members: Vec<TrackId>,
    /// Raster step, Hz.
    pub raster_hz: Option<f64>,
    /// Hop rate `1 / mean(successor start − predecessor start)`, hops/s.
    pub hop_rate_hz: Option<f64>,
    /// Mean member dwell, s.
    pub dwell_s: Option<f64>,
    /// Hops linked.
    pub hops: u64,
    /// First linked dwell start to last linked dwell end.
    pub time: TimeRange,
}

/// Tracker output, in order. Merges and splits are appended events; nothing is overwritten.
#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum TrackEvent {
    /// A new track.
    Opened {
        /// Track.
        track: TrackId,
        /// First member start.
        at: Timestamp,
        /// First burst centre, Hz.
        f_center_hz: f64,
        /// First burst bandwidth, Hz.
        bandwidth_hz: f64,
    },
    /// A gain change, retune or transition inside a track.
    Segment(SegmentBoundary),
    /// `from` merged into `into` (`from` keeps its member links and state `MergedInto`).
    Merged {
        /// Absorbed track (or hop set).
        from: TrackId,
        /// Surviving track (or hop set).
        into: TrackId,
        /// When.
        at: Timestamp,
    },
    /// Track `from`'s bursts separated into two stable clusters: one continues as `from`, the
    /// other as the new track `into` (`split_from = from`; announced by its own `Opened` just
    /// before). `from` keeps its history and links.
    TrackSplit {
        /// Continuing track.
        from: TrackId,
        /// New track.
        into: TrackId,
        /// When.
        at: Timestamp,
    },
    /// A box straddling several tracks (a split-frame bridge) was spread back onto them.
    Split {
        /// The fused detection (linked to `tracks[0]`, the largest overlap).
        detection: hk_model::DetectionId,
        /// Tracks it was spread onto.
        tracks: [TrackId; 2],
    },
    /// A hop set qualified.
    HopSetFormed(HopSetSummary),
    /// Every member of a hop set closed.
    HopSetClosed(HopSetSummary),
    /// A track closed (final summary).
    Closed(TrackSummary),
}
