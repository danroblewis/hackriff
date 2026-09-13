//! Burst tracking (C10, T-007): links [`DetectionRecord`](crate::DetectionRecord)s into docs/07
//! §2.10 [`Track`](hk_model::Track)s with timing features: inter-arrival statistics,
//! periodicity (period, confidence, jitter), duty cycle, burst-length distribution, and hop sets.
//!
//! # Pipeline (online, in stream order)
//!
//! 1. **Hold.** Records are held `hold_frames` past their end, then routed in end order, so
//!    co-timed boxes emitted a few frames apart are seen together. Impulsive records and records
//!    shorter than `min_part_frames` (split side runs) are skipped.
//! 2. **Continuations.** A box that starts on the frame after a max-duration split box it
//!    overlaps in frequency (same segment) continues *that detection's* track: the burst is
//!    extended, not restarted, so a continuous line over hours is one burst. A box at a detector
//!    segment start continues a burst closed by the transition (`close = Transition`) when it
//!    overlaps it in frequency and starts within `max_transition_gap_s`: a transition is a segment
//!    boundary, not the end of the emitter. A box that continues several tracks' splits (a gap
//!    bridge fused on the split frame) is spread back onto them by frequency overlap and linked
//!    to the largest overlap ([`TrackEvent::Split`]).
//! 3. **Tone-lobe aggregation.** Natural-start boxes (not at a segment start, not a split
//!    continuation) whose starts and ends coincide within `coincidence_frames` and whose
//!    occupied extents are within `lobe_gap_factor ×` the wider one's width merge into one burst
//!    (2-FSK tones); a `marginal` box inside another's span merges the same way (sidelobes). Boxes at a segment start never merge: coincidence there is the
//!    observation edge, not the emitter.
//! 4. **Association.** Gated nearest neighbour over open tracks: centre within
//!    `ε = max(2 bins, 10 % BW)` (or mutual containment), bandwidth ratio ≤ 2, observed idle time
//!    ≤ the track's timeout; cost `Δf/ε + ln(ratio)/ln 2`. A burst overlapping the track's
//!    current burst in time extends it. Otherwise a new track opens **tentative**: no
//!    `Opened`, not drained, links held, until it has `confirm_bursts` bursts, `confirm_on_time_s`
//!    on-time or a hop link. A track that closes tentative (a stream-start lobe, a low-SNR
//!    single-lobe fragment) is discarded (`tentative_discarded`).
//! 5. **Provenance.** Association ignores gain state; a member under a different provenance
//!    records a [`SegmentBoundary`] (gain change, retune, other) and the track continues.
//! 6. **Features** on each finished burst, all bounded per track: inter-arrival and burst-length
//!    moments, a log-spaced length histogram, the latest 64 starts for the periodicity fold
//!    ([`Periodicity`]), union on-time and observed time for the duty cycle.
//! 7. **Hop sets.** A finished dwell whose start is within `max(2 frames, 25 % dwell)` of the end
//!    of a dwell on another, non-overlapping channel with similar BW and dwell links the two
//!    channel tracks; linked channels form a hop set once ≥ 3 channels have ≥ 2 links and there
//!    are ≥ 10 hops. The hop set is an aggregate Track (its id is the hop-set id) with
//!    `hop_set_hz`, `hop_rate_hz` and `co_occurring` = member channel tracks; members carry the
//!    hop-set id in `co_occurring`. **Bursty hoppers** (packets separated by silence, T-031): a
//!    dwell with no contiguous predecessor links to its nearest preceding similar burst (BW ratio
//!    ≤ 1.5, length ratio ≤ `bursty_length_ratio`, silence ≤ `max_silence_s`) on another channel
//!    when that burst's own nearest similar predecessor is on a third channel, the three centres
//!    share a raster, no similar burst on another track overlaps it in time, and neither track is
//!    periodic (`periodic_veto_*`). Two channels alone never link (two interleaved emitters look
//!    the same). Periodic channels do not count towards `min_channels` when the set's links are
//!    mostly bursty or the channels' periods disagree by > 10 % (independent emitters whose bursts
//!    abut by chance; a cyclic hopper's channels share one period).
//!    Every member channel also needs hop links on at least `min_link_fraction` of its bursts.
//! 8. **Lifecycle.** Tracks close after their idle timeout in *observed* time (`idle_timeout_s`,
//!    stretched to 4 inter-arrivals, ≤ 1 h), at the live-track cap, or at [`Tracker::finish`].
//!    Two open tracks that converge (centres within ε/2, BW ratio ≤ 1.25, no simultaneous bursts)
//!    merge into the older one; the younger keeps its member links and becomes `MergedInto`
//!    ([`TrackEvent::Merged`]); [`TrackBatch`] copies its links to the survivor. A track whose
//!    latest 16 bursts form two stable centre (or bandwidth) clusters, both active through the
//!    window, **splits**: the larger cluster continues, the other opens a new track with
//!    `split_from` ([`TrackEvent::TrackSplit`]); a parent and child never re-merge. Events are
//!    appended; nothing is overwritten.
//!
//! # Duty cycle
//!
//! `on-time / observed time` from the first burst start to a horizon: `last start + period`
//! for a periodic track (so a train of n bursts spans n periods), `now` for an open or
//! end-of-stream aperiodic track (silence counts), or the last burst end for an idle-closed one.
//! Observation gaps passed to [`Tracker::observe`] are excluded.
//!
//! # Persistence
//!
//! [`Tracker::drain_into`] fills a [`TrackBatch`] with changed Track aggregates, new
//! track↔detection links and segment boundaries; [`TrackBatch::write`] upserts, links, re-points
//! merged tracks' links and appends the boundaries in **one** repository transaction
//! ([`hk_model::Repository::batch`]). Write the detections first. The Track's `TimingFeatures`
//! carry period confidence and jitter, the burst-length distribution, the segment count, the
//! hop-set id (members) and the hop raster (hop-set aggregates) (T-035); the boundaries are
//! `track_segment` rows. The hop raster is the largest lattice step explaining most channel
//! centres within their uncertainty, weighted by bursts and SNR, preferring a common raster over
//! an incommensurate larger step (`stats::robust_raster`).
//!
//! # Real-time path
//!
//! No allocation in steady state except when a track or hop set is created or closed: the
//! per-track state is fixed-size, and the hold buffer, member ring, split ring and link buffer are
//! preallocated (links grow only if [`Tracker::drain_into`] is not called).

mod config;
mod events;
pub mod inventory;
mod persist;
mod stats;
mod tracker;

pub use config::{HopConfig, PeriodConfig, SplitConfig, TrackerConfig};
pub use events::{
    BoundaryKind, CloseCause, Distribution, HopSetSummary, Periodicity, SegmentBoundary,
    TrackEvent, TrackSummary,
};
pub use persist::TrackBatch;
pub use tracker::{Tracker, TrackerStats};
