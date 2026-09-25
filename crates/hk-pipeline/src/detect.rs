//! Always-on reader 1: STFT → noise floor → detection → tracking, and floor events → anomaly
//! lifecycle (+ correlation with the offline feed cache). Every repository write runs on a
//! separate writer thread.
//!
//! - **STFT:** Hann, 0 % overlap, `K` averages (see [`crate::config`] for why), SK on.
//! - **Short bursts (T-075):** every raw chunk also goes through [`BurstDetector`] (time-domain
//!   energy, µs timing) for bursts below the STFT's resolution (ADS-B squitters, OOK). Its
//!   records are stored like any detection but never reach the tracker (untracked), and its
//!   longest burst stays below the STFT path's minimum duration ([`burst_config`]).
//! - **Clip counts:** the reader scans each raw ci8 chunk for clipped samples and keeps their
//!   stream indices until the frame covering them is processed, so every frame gets its exact
//!   clip count (`hk_detect::count_clipped_ci8` semantics).
//! - **Writer thread (ADR-0006 batching, T-037b).** Every `flush_interval_s` of stream time
//!   (0.5 s) or `detection_batch` detections the reader hands one [`WriteBatch`] (detections,
//!   track upserts and links, closed-track inventory events, floor events) to the
//!   `hk-detect-writer` thread over a bounded queue of [`WRITER_QUEUE`] batches, so a slow or
//!   locked database never stalls detection. The writer keeps the T-027 fix-round semantics:
//!   detections whose write failed stay pending and are retried (with every new batch and every
//!   [`RETRY_INTERVAL`]); track links and the closes that summarise them wait until every
//!   detection is stored; anomalies follow, then correlation, whose feed-cache I/O runs before
//!   the repository lock is taken again. Trust verdicts the control thread queued
//!   ([`crate::verify`]) are stored in the same pass.
//! - **Backpressure accounting.** Live: a full queue keeps the batch on the reader, merged into
//!   the next flush (`writer_queue_full`); only a carry-over beyond [`MAX_CARRY_DETECTIONS`]
//!   detections makes the reader wait (`writer_blocked`), because dropping a batch would leave
//!   later track links naming detections that were never stored. Lossless replay: the reader
//!   waits for queue room (`writer_blocked`) while the flow gate holds capture. At the end of the
//!   stream the reader waits until the writer has stored every detection and link before the
//!   final closes reach the chains.
//! - **Dense frames:** a record spanning a frame the detector could not label completely (dense,
//!   or runs dropped at the live-component cap) gets `DetectionFlags::dense_skipped`
//!   (`dense_flagged`).
//! - **Capture name:** the stream's first [`CAPTURE_NAME_SAMPLES`] samples, their first index and
//!   time and the rate are hashed (FNV-1a 64) into a content-derived name, identical on every
//!   replay of the same IQ. The writer passes it to [`crate::Inventory::capture_name`]; closed
//!   tracks are held on the reader until the name exists, so every track sighting carries it
//!   (T-034 replay dedup).
//! - **Control events:** member boxes of each track, a track's first trustworthy confirmation
//!   (not a spur, image or impulsive candidate), closes, merges and (with the scheduler)
//!   segment capture results.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::sync::{Arc, PoisonError};
use std::thread;
use std::time::Duration;

use hk_context::occupancy::channels::{DetectionExtent, dc_only_suspect};
use hk_context::{Correlator, FeedCache, FloorAnomalies, FloorAnomalyConfig, Site};
use hk_core::ReadOutcome;
use hk_detect::clip::is_clipped_ci8;
use hk_detect::track::TrackSummary;
use hk_detect::{
    BandProfile, BurstConfig, BurstDetector, ClipCount, Confirmation, DetectionProfile,
    DetectionRecord, DetectionWriter, Detector, DetectorConfig, DetectorEvent, LiveExtent,
    TrackBatch, TrackEvent, Tracker, TrackerConfig,
};
use hk_dsp::floor::{FloorConfig, FloorEvent, NoiseFloorTracker};
use hk_dsp::window::WindowKind;
use hk_dsp::{InputInfo, SpectrumFrame, StftConfig, WelchConfig};
use hk_model::{DetectionId, FreqRange, Timestamp, TrackId, TrustVerdict};
use num_complex::Complex;

use crate::dc_twin::{LiveDcTwins, Observed};
use crate::events::{Candidate, ControlEvent, MemberBox};
use crate::presence::PresenceStream;
use crate::run::Shared;
use crate::stats::{add, inc, set};

/// Boxes and links older than this (stream time) are forgotten.
const MEMORY_NS: i64 = 120_000_000_000;

/// Open confirmed channel tracks are offered to the inventory at most this often, stream ns
/// (T-109).
const LIVE_OFFER_NS: i64 = 5_000_000_000;

/// Bursts an open track needs before it is offered live (T-109): a repeating emitter, not a
/// one-off or a continuous carrier (those enter at close, as before).
const LIVE_MIN_BURSTS: u64 = 4;
/// Air an open track needs before it is offered live when it has too few bursts to qualify that
/// way (T-403), s.
///
/// **This is what gets a continuous carrier an inventory row before its track closes.** A broadcast
/// station is one long burst, so [`LIVE_MIN_BURSTS`] never admits it; its row used to come from a
/// chain, and when no chain claimed it — a mode the chain rejected, an admission race lost — it had
/// no row at all until the idle timeout, which is tens of seconds of an obvious, strong signal
/// missing from the Candidate list. Two seconds is the same air the confirmation rule's continuous
/// route asks for, so the row exists no later than the first moment a decision could be taken on
/// it, and (the duty cycle still being short of the rule's threshold that early) the user sees the
/// candidate before the system confirms it.
const LIVE_MIN_ON_AIR_S: f64 = 2.0;
/// Open tracks are re-weighed against the confirmation rule at most this often, stream ns (T-403).
///
/// Faster than [`LIVE_OFFER_NS`] on purpose. An offer **writes** — a sighting, and possibly a new
/// inventory row — so it is paced at 5 s; a review only re-reads the rule for a row that already
/// exists, and for an entry that is no longer a candidate it costs two indexed lookups and stops.
/// The cadence is a floor under the latency: the evidence route B asks for is complete about 2 s
/// into a continuous emission, and at 5 s the decision would wait three seconds longer than the
/// measurement did — the shape of the defect this exists to fix, one order of magnitude smaller.
const LIVE_REVIEW_NS: i64 = 1_000_000_000;
/// Open tracks whose observed extent one flush may carry to the writer (T-388). The presence
/// stream caps what it publishes again; this only bounds the batch.
const MAX_LIVE_EXTENTS: usize = 256;
/// Batches the writer queue holds.
pub(crate) const WRITER_QUEUE: usize = 8;
/// Retry period for failed writes (and queued trust verdicts) while no batch arrives.
pub(crate) const RETRY_INTERVAL: Duration = Duration::from_millis(200);
/// Live carry-over beyond which the reader waits for the writer instead of growing it.
pub(crate) const MAX_CARRY_DETECTIONS: usize = 65_536;
/// Samples hashed into the capture name.
pub(crate) const CAPTURE_NAME_SAMPLES: usize = 16_384;
/// Dense-frame indices kept for flagging records (records never span more frames).
const DENSE_MEMORY_FRAMES: u64 = 1 << 20;
/// Longest end-of-stream wait for the writer.
const SYNC_TIMEOUT: Duration = Duration::from_secs(120);

// Short-lived per frame (moved straight into the tracker and the writer), like hk-detect's
// own `TrackEvent`.
#[allow(clippy::large_enum_variant)]
enum Owned {
    Det(DetectionRecord),
    Conf(Confirmation),
}

#[derive(Clone, Debug, Default)]
struct TrackState {
    f_lo: f64,
    f_hi: f64,
    first_sample: u64,
    bursty: Option<bool>,
    sent: bool,
}

/// One flush of repository work, in write order.
#[derive(Default)]
struct WriteBatch {
    detections: Vec<DetectionRecord>,
    tracks: TrackBatch,
    closed: Vec<TrackEvent>,
    /// Open confirmed channel tracks offered to the inventory before they close (T-109).
    live: Vec<TrackSummary>,
    /// T-403: open tracks the offer predicate excludes **only** for having too few bursts — the
    /// continuous carriers — re-weighed against the confirmation rule. Disjoint from `live`, and
    /// creates nothing: the inventory reviews only a track it already gave a row.
    live_reviews: Vec<TrackSummary>,
    /// The **observed** time extent of every track the same predicate would offer, collected on
    /// every flush rather than every `LIVE_OFFER_NS` (T-388): what the presence stream publishes so
    /// a live signal's box grows without waiting for the next database offer. Times come from the
    /// tracker's `t_last_end`, so a track that stopped carries the end it stopped at.
    live_extents: Vec<LiveExtent>,
    floor: Vec<FloorEvent>,
    now_ns: i64,
    capture_name: Option<String>,
}

fn merge_tracks(into: &mut TrackBatch, mut from: TrackBatch) {
    into.upserts.append(&mut from.upserts);
    into.links.append(&mut from.links);
    into.repoints.append(&mut from.repoints);
    into.segments.append(&mut from.segments);
    into.linked_at = into.linked_at.max(from.linked_at);
}

impl WriteBatch {
    fn is_empty(&self) -> bool {
        self.detections.is_empty()
            && self.tracks.is_empty()
            && self.closed.is_empty()
            && self.live.is_empty()
            && self.live_reviews.is_empty()
            && self.live_extents.is_empty()
            && self.floor.is_empty()
            && self.capture_name.is_none()
    }

    fn merge(&mut self, other: WriteBatch) {
        let WriteBatch {
            mut detections,
            tracks,
            mut closed,
            live,
            live_reviews,
            live_extents,
            mut floor,
            now_ns,
            capture_name,
        } = other;
        self.detections.append(&mut detections);
        merge_tracks(&mut self.tracks, tracks);
        self.closed.append(&mut closed);
        if !live.is_empty() {
            // Only the newest offer matters: each summary supersedes the older one of its track.
            self.live = live;
        }
        if !live_reviews.is_empty() {
            // Same rule: a review is a snapshot of a life still being lived, so a carried-over
            // older one would weigh less evidence than has already been measured.
            self.live_reviews = live_reviews;
        }
        if !live_extents.is_empty() {
            // Same rule, and more so: an extent is a *latest observed end*, so a carried-over older
            // one would publish a box top behind the one already measured. The newest wins.
            self.live_extents = live_extents;
        }
        self.floor.append(&mut floor);
        self.now_ns = self.now_ns.max(now_ns);
        if capture_name.is_some() {
            self.capture_name = capture_name;
        }
    }
}

enum WriterMsg {
    Batch(Box<WriteBatch>),
    /// Reply once everything received before it has been written (or attempted).
    Sync(SyncSender<()>),
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv(h: u64, bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(h, |h, &b| (h ^ u64::from(b)).wrapping_mul(FNV_PRIME))
}

/// The content-derived capture name (see the module docs).
#[derive(Default)]
pub(crate) struct CaptureNamer {
    hash: Option<u64>,
    samples: usize,
    head: Option<(u64, i64)>,
    name: Option<String>,
    done: bool,
}

impl CaptureNamer {
    /// Hashes the leading samples of a chunk starting at `first_sample` / `host_ns`.
    pub fn push(&mut self, first_sample: u64, host_ns: i64, samples: &[Complex<i8>]) {
        if self.done || self.samples >= CAPTURE_NAME_SAMPLES {
            return;
        }
        self.head.get_or_insert((first_sample, host_ns));
        let take = (CAPTURE_NAME_SAMPLES - self.samples).min(samples.len());
        let h = samples[..take]
            .iter()
            .fold(self.hash.unwrap_or(FNV_OFFSET), |h, x| {
                fnv(h, &[x.re as u8, x.im as u8])
            });
        self.hash = Some(h);
        self.samples += take;
    }

    /// Enough samples hashed.
    pub fn complete(&self) -> bool {
        self.samples >= CAPTURE_NAME_SAMPLES
    }

    /// Finalises the name (at the sample count, or at the end of a shorter stream). A stream
    /// with no samples has no name.
    pub fn finalize(&mut self, fs: f64) {
        if self.done {
            return;
        }
        let Some((index, ns)) = self.head else {
            return;
        };
        let mut h = self.hash.unwrap_or(FNV_OFFSET);
        h = fnv(h, &index.to_le_bytes());
        h = fnv(h, &ns.to_le_bytes());
        h = fnv(h, &fs.to_bits().to_le_bytes());
        h = fnv(h, &(self.samples as u64).to_le_bytes());
        self.name = Some(format!("hk-iq:fnv1a64:{h:016x}"));
        self.done = true;
    }

    /// The name has been finalised.
    pub fn named(&self) -> bool {
        self.done
    }

    /// The name, once (for the next batch).
    fn take(&mut self) -> Option<String> {
        self.name.take()
    }
}

struct DetectNode {
    shared: Arc<Shared>,
    tx: Sender<ControlEvent>,
    writer: SyncSender<WriterMsg>,
    lossless: bool,
    floor: NoiseFloorTracker,
    det: Detector,
    tracker: Tracker,
    batch: TrackBatch,
    carry: WriteBatch,
    clips: VecDeque<u64>,
    pending: Vec<DetectionRecord>,
    pending_floor: Vec<FloorEvent>,
    closed: Vec<TrackEvent>,
    boxes: HashMap<DetectionId, MemberBox>,
    good: HashSet<DetectionId>,
    confirmed: HashSet<DetectionId>,
    det_track: HashMap<DetectionId, TrackId>,
    order: VecDeque<(i64, DetectionId)>,
    tracks: HashMap<TrackId, TrackState>,
    links_seen: usize,
    last_flush_ns: Option<i64>,
    live_offered_ns: Option<i64>,
    live_reviewed_ns: Option<i64>,
    /// T-403: tracks already offered, so one that becomes offerable between grid ticks gets its
    /// row on the review cadence instead of waiting out `LIVE_OFFER_NS`. Rebuilt from the offerable
    /// set on every grid tick, so it is bounded by the tracker's open slots and forgets closed
    /// tracks by construction.
    offered: HashSet<TrackId>,
    /// Reused for the tracker's live offers (T-109).
    live_buf: Vec<TrackSummary>,
    /// Scratch for [`MAX_LIVE_EXTENTS`] open-track extents (T-388), reused every flush.
    extent_buf: Vec<LiveExtent>,
    now_ns: i64,
    segments: u64,
    segment_start: Timestamp,
    scheduler_captures: bool,
    dense_seen: (u64, u64),
    dense_frames: VecDeque<u64>,
    namer: CaptureNamer,
    finishing: bool,
    /// T-174 (ADR-0012 §2.6): recent clean detections and open DC flags across tunings.
    twins: LiveDcTwins,
    /// This batch's tracked detections, for [`Self::twins`].
    twin_batch: Vec<Observed>,
    /// Refuted DC flags (reused).
    refuted: Vec<DetectionId>,
    /// DC-flagged detections that become trustworthy (`good`) when their flag is refuted.
    dc_goodable: HashSet<DetectionId>,
}

impl DetectNode {
    fn new(
        shared: Arc<Shared>,
        tx: Sender<ControlEvent>,
        writer: SyncSender<WriterMsg>,
    ) -> anyhow::Result<Self> {
        let floor_cfg = FloorConfig::default();
        let floor = NoiseFloorTracker::new(floor_cfg)
            .map_err(|e| anyhow::anyhow!("floor tracker: {e:?}"))?;
        let mut dcfg = DetectorConfig::new(shared.survey_id).with_floor_config(&floor_cfg);
        for b in &shared.cfg.settings.short_burst_bands_hz {
            dcfg.band_profiles.push(BandProfile {
                freq: FreqRange::new(b[0], b[1]),
                profile: DetectionProfile::short_burst(),
            });
        }
        let scheduler_captures = shared.cfg.drive_scheduler;
        let mut det = Detector::new(dcfg).map_err(|e| anyhow::anyhow!("detector config: {e:?}"))?;
        det.retain_capture_results(scheduler_captures);
        let twins = LiveDcTwins::new(shared.dc_twin);
        Ok(Self {
            twins,
            twin_batch: Vec::new(),
            refuted: Vec::new(),
            dc_goodable: HashSet::new(),
            lossless: shared.gate.enabled(),
            shared,
            tx,
            writer,
            floor,
            det,
            tracker: Tracker::new(TrackerConfig::default()),
            batch: TrackBatch::new(),
            carry: WriteBatch::default(),
            clips: VecDeque::new(),
            pending: Vec::new(),
            pending_floor: Vec::new(),
            closed: Vec::new(),
            boxes: HashMap::new(),
            good: HashSet::new(),
            confirmed: HashSet::new(),
            det_track: HashMap::new(),
            order: VecDeque::new(),
            tracks: HashMap::new(),
            links_seen: 0,
            last_flush_ns: None,
            live_offered_ns: None,
            live_reviewed_ns: None,
            offered: HashSet::new(),
            live_buf: Vec::new(),
            extent_buf: Vec::new(),
            now_ns: 0,
            segments: 0,
            segment_start: Timestamp::UNIX_EPOCH,
            scheduler_captures,
            dense_seen: (0, 0),
            dense_frames: VecDeque::new(),
            namer: CaptureNamer::default(),
            finishing: false,
        })
    }

    fn send(&self, ev: ControlEvent) {
        let _ = self.tx.send(ev);
    }

    /// Records the detector frame just processed as incompletely labelled when the dense or
    /// dropped-run counters moved.
    fn note_dense(&mut self, dense: u64, dropped: u64, frame: u64) {
        if (dense, dropped) != self.dense_seen {
            self.dense_seen = (dense, dropped);
            self.dense_frames.push_back(frame);
        }
        let keep = frame.saturating_sub(DENSE_MEMORY_FRAMES);
        while self.dense_frames.front().is_some_and(|&f| f < keep) {
            self.dense_frames.pop_front();
        }
    }

    fn spans_dense(&self, frames: &Range<u64>) -> bool {
        let i = self.dense_frames.partition_point(|&f| f < frames.start);
        self.dense_frames.get(i).is_some_and(|&f| f < frames.end)
    }

    fn process_frame(&mut self, frame: &SpectrumFrame) {
        let a = frame.t.sample_index;
        let b = a + frame.sample_count;
        while self.clips.front().is_some_and(|&i| i < a) {
            self.clips.pop_front();
        }
        let clipped = self.clips.partition_point(|&i| i < b) as u64;
        let mut floor_events = std::mem::take(&mut self.pending_floor);
        let floor = self.floor.update(frame, |e| floor_events.push(e.clone()));
        let mut evs: Vec<Owned> = Vec::new();
        self.det.process(
            frame,
            floor,
            ClipCount::new(clipped, frame.sample_count),
            &mut |ev: DetectorEvent<'_>| match ev {
                DetectorEvent::Detection(r) => evs.push(Owned::Det(r)),
                DetectorEvent::Confirmed(c) => evs.push(Owned::Conf(c)),
                DetectorEvent::Integrated(_) => {}
            },
        );
        self.pending_floor = floor_events;
        self.now_ns = frame.t.host_time.as_unix_nanos();

        let stats = self.det.stats();
        let (segments, dense, dropped, frames) = (
            stats.segments,
            stats.dense_frames,
            stats.dropped_runs,
            stats.frames,
        );
        let (invalid, guarded) = (stats.invalid_floor_frames, stats.guarded_frames);
        if segments != self.segments {
            if self.scheduler_captures && self.segments > 0 {
                if let Some(c) = self.det.last_capture() {
                    inc(&self.shared.counters.detect.captures);
                    self.send(ControlEvent::Capture {
                        t_start: self.segment_start,
                        capture: Box::new(c.clone()),
                    });
                }
            }
            self.segments = segments;
            self.segment_start = frame.t.host_time;
        }
        self.note_dense(dense, dropped, frames);
        let dc = &self.shared.counters.detect;
        set(&dc.dense_frames, dense);
        set(&dc.invalid_floor_frames, invalid);
        set(&dc.guarded_frames, guarded);

        let mut tev = Vec::new();
        self.handle_events(evs, &mut tev);
        self.tracker
            .observe_frame(&self.det, frame, &mut |te| tev.push(te));
        self.handle_track_events(tev);
        self.drain_links();
        let due = self
            .last_flush_ns
            .is_none_or(|t| self.now_ns - t >= self.flush_ns())
            || self.pending.len() >= self.shared.cfg.settings.detection_batch;
        if due {
            self.flush();
        }
    }

    /// A short-burst detection (T-075): stored untracked (never offered to the tracker).
    fn push_burst(&mut self, r: DetectionRecord) {
        inc(&self.shared.counters.detect.detections);
        self.pending.push(r);
    }

    fn flush_ns(&self) -> i64 {
        (self.shared.cfg.settings.flush_interval_s * 1e9) as i64
    }

    fn handle_events(&mut self, evs: Vec<Owned>, tev: &mut Vec<TrackEvent>) {
        let shared = Arc::clone(&self.shared);
        let dc = &shared.counters.detect;
        for ev in evs {
            match ev {
                Owned::Det(mut r) => {
                    inc(&dc.detections);
                    if self.spans_dense(&r.frames) {
                        r.detection.flags.dense_skipped = true;
                        inc(&dc.dense_flagged);
                    }
                    let id = r.detection.id;
                    let f = &r.detection.flags;
                    let dc_only = dc_only_suspect(f);
                    if !(f.spur_candidate || f.image_candidate || f.impulsive) {
                        self.good.insert(id);
                    } else if dc_only && !(f.image_candidate || f.impulsive) {
                        self.dc_goodable.insert(id);
                    }
                    let extent = DetectionExtent::of(&r.detection);
                    let suspect = extent.suspect;
                    self.twin_batch.push(Observed {
                        id,
                        extent,
                        dc_only,
                        own_lo: Some(r.provenance.get().tune.center_hz).filter(|f| f.is_finite()),
                    });
                    if r.candidate.is_confirmed() {
                        self.confirmed.insert(id);
                    }
                    let t = r.detection.time.start;
                    let (f_lo_hz, f_hi_hz) = occupied_box(&r);
                    self.boxes.insert(
                        id,
                        MemberBox {
                            detection: id,
                            samples: r.samples.clone(),
                            f_lo_hz,
                            f_hi_hz,
                            t_start: t,
                            continues: r.continues,
                            snr_db: r.detection.snr_mean_db,
                            // The occupancy engine's §2.6 rule, so candidates and FCO agree; a
                            // DC flag a twin refutes is cleared below (T-174).
                            suspect,
                        },
                    );
                    self.order.push_back((t.as_unix_nanos(), id));
                    self.tracker.push_detection(&r, &mut |te| tev.push(te));
                    self.pending.push(r);
                }
                Owned::Conf(c) => {
                    inc(&dc.confirmations);
                    self.tracker.confirm(&c);
                    self.confirmed.insert(c.detection);
                    if let Some(&t) = self.det_track.get(&c.detection) {
                        self.maybe_confirm(t, c.detection);
                    }
                }
            }
        }
        self.apply_dc_twins();
        while self
            .order
            .front()
            .is_some_and(|&(t, _)| self.now_ns - t > MEMORY_NS)
        {
            let (_, id) = self.order.pop_front().expect("front");
            self.boxes.remove(&id);
            self.good.remove(&id);
            self.dc_goodable.remove(&id);
            self.confirmed.remove(&id);
            self.det_track.remove(&id);
        }
    }

    /// T-174 (ADR-0012 §2.6): the batch's detections go through the live DC-twin index. A refuted
    /// DC flag clears its box's suspect flag and, with no other untrustworthy flag, makes the
    /// detection trustworthy for its track's candidate. A box already sent to the control thread
    /// (linked) is retracted there and its confirmed track offered again.
    fn apply_dc_twins(&mut self) {
        if self.twin_batch.is_empty() {
            return;
        }
        let mut refuted = std::mem::take(&mut self.refuted);
        self.twins
            .observe(self.now_ns, &self.twin_batch, &mut refuted);
        self.twin_batch.clear();
        for id in refuted.drain(..) {
            let Some(b) = self.boxes.get_mut(&id) else {
                continue;
            };
            if !b.suspect {
                continue;
            }
            b.suspect = false;
            let member = b.clone();
            // T-948: and the track stops counting it as the receiver's own line, so an emission
            // the receiver was merely tuned on top of is admitted to the inventory.
            self.tracker.refute_dc(id);
            if self.dc_goodable.remove(&id) {
                self.good.insert(id);
            }
            if let Some(&track) = self.det_track.get(&id) {
                self.send(ControlEvent::MemberRefuted { track, member });
                if self.confirmed.contains(&id) {
                    self.maybe_confirm(track, id);
                }
            }
        }
        self.refuted = refuted;
    }

    fn handle_track_events(&mut self, tev: Vec<TrackEvent>) {
        let dc = &self.shared.counters.detect;
        for te in tev {
            match te {
                TrackEvent::Opened { track, .. } => {
                    inc(&dc.tracks_opened);
                    self.tracks.entry(track).or_default();
                }
                TrackEvent::Closed(summary) => {
                    inc(&dc.tracks_closed);
                    let track = summary.track.id;
                    self.tracks.remove(&track);
                    self.closed.push(TrackEvent::Closed(summary.clone()));
                    self.send(ControlEvent::TrackClosed {
                        track,
                        summary: Box::new(summary),
                    });
                }
                TrackEvent::Merged { from, into, at } => {
                    inc(&dc.track_merges);
                    // The inventory withdraws a live entry of the absorbed track (T-109).
                    self.closed.push(TrackEvent::Merged { from, into, at });
                    self.send(ControlEvent::TrackMerged { from, into });
                }
                ev @ (TrackEvent::HopSetFormed(_) | TrackEvent::HopSetClosed(_)) => {
                    self.closed.push(ev);
                }
                _ => {}
            }
        }
    }

    fn drain_links(&mut self) {
        self.tracker.drain_into(&mut self.batch);
        let new: Vec<(TrackId, DetectionId)> = self.batch.links[self.links_seen..].to_vec();
        self.links_seen = self.batch.links.len();
        for (t, d) in new {
            self.det_track.insert(d, t);
            if let Some(m) = self.boxes.get(&d).cloned() {
                let st = self.tracks.entry(t).or_default();
                if st.f_hi == 0.0 {
                    st.f_lo = m.f_lo_hz;
                    st.f_hi = m.f_hi_hz;
                    st.first_sample = m.samples.start;
                } else {
                    st.f_lo = st.f_lo.min(m.f_lo_hz);
                    st.f_hi = st.f_hi.max(m.f_hi_hz);
                    st.first_sample = st.first_sample.min(m.samples.start);
                }
                let fs = self.shared.fs;
                let dur_s = (m.samples.end - m.samples.start) as f64 / fs;
                if m.continues {
                    st.bursty = Some(false);
                } else if st.bursty.is_none() && dur_s < 0.5 {
                    st.bursty = Some(true);
                }
                self.send(ControlEvent::Member {
                    track: t,
                    member: m,
                });
            }
            if self.confirmed.contains(&d) {
                self.maybe_confirm(t, d);
            }
        }
    }

    fn maybe_confirm(&mut self, t: TrackId, d: DetectionId) {
        if !self.good.contains(&d) {
            return;
        }
        let Some(m) = self.boxes.get(&d).cloned() else {
            return;
        };
        let Some(st) = self.tracks.get_mut(&t) else {
            return;
        };
        if st.sent || st.f_hi == 0.0 {
            return;
        }
        st.sent = true;
        let cand = Candidate {
            track: Some(t),
            detection: Some(d),
            f_lo_hz: st.f_lo,
            f_hi_hz: st.f_hi,
            first_sample: st.first_sample,
            trigger_sample: m.samples.end,
            bursty: st.bursty,
        };
        inc(&self.shared.counters.detect.tracks_confirmed);
        self.send(ControlEvent::TrackConfirmed(cand));
    }

    /// Hands the flush to the writer thread (see the module docs for the backpressure rules).
    fn flush(&mut self) {
        self.last_flush_ns = Some(self.now_ns);
        // Closes wait for the capture name (the inventory's re-measurement key), except at the
        // end of the stream.
        let closed = if self.namer.named() || self.finishing {
            std::mem::take(&mut self.closed)
        } else {
            Vec::new()
        };
        // T-109: a channel that keeps keying (a pager net, a repeating beacon) never idles out, so
        // its track would reach the inventory only when the run stops. Offer open confirmed
        // channel tracks with a few bursts (T-403: **or** seconds of unbroken air) and a settled
        // fate (no hop link or set, not an in-band fragment) every `LIVE_OFFER_NS`; the sighting is
        // keyed by the track, so a re-offer (and the final close) adds only new bursts, and the
        // inventory retracts the entry when the track ends with no sighting after all.
        //
        // T-403: the same summaries are also **reviewed** against the confirmation rule, on their
        // own faster cadence — see `LIVE_REVIEW_NS`. One pass builds them for both, so the extra
        // cadence costs the summaries and nothing else.
        let live_now = self.namer.named() && !self.finishing;
        let offer_now = live_now
            && self
                .live_offered_ns
                .is_none_or(|t| self.now_ns - t >= LIVE_OFFER_NS);
        let review_now = live_now
            && self
                .live_reviewed_ns
                .is_none_or(|t| self.now_ns - t >= LIVE_REVIEW_NS);
        if offer_now || review_now {
            self.tracker
                .live_offers_into(LIVE_MIN_BURSTS, LIVE_MIN_ON_AIR_S, &mut self.live_buf);
        }
        // Crosses to the writer thread: allocates only when something is offered or reviewed.
        //
        // T-403: a track that becomes offerable just after a flush used to wait most of
        // `LIVE_OFFER_NS` for its first row, because the cadence is a global grid rather than a
        // per-track one. That whole wait is in front of every later decision about it, so a track
        // that has never been offered is offered on the review cadence instead; the 5 s grid then
        // governs the re-offers, which is where the repeated database work actually is.
        let live = if offer_now {
            self.live_offered_ns = Some(self.now_ns);
            self.offered.clear();
            self.offered
                .extend(self.live_buf.iter().map(|s| s.track.id));
            self.live_buf.clone()
        } else if review_now {
            let fresh: Vec<TrackSummary> = self
                .live_buf
                .iter()
                .filter(|s| !self.offered.contains(&s.track.id))
                .cloned()
                .collect();
            self.offered.extend(fresh.iter().map(|s| s.track.id));
            fresh
        } else {
            Vec::new()
        };
        let live_reviews = if review_now {
            self.live_reviewed_ns = Some(self.now_ns);
            self.live_buf.drain(..).collect()
        } else {
            self.live_buf.clear();
            Vec::new()
        };
        // T-388: every flush, not every `LIVE_OFFER_NS`. The extents are the same tracks under the
        // same predicate, but reading two timestamps out of each slot instead of building a
        // `TrackSummary`, so the fast path costs a fraction of the offer it rides beside.
        let live_extents = if self.namer.named() && !self.finishing {
            self.tracker.live_extents_into(&mut self.extent_buf);
            self.extent_buf.truncate(MAX_LIVE_EXTENTS);
            self.extent_buf.drain(..).collect()
        } else {
            Vec::new()
        };
        let batch = WriteBatch {
            detections: std::mem::take(&mut self.pending),
            tracks: std::mem::take(&mut self.batch),
            closed,
            live,
            live_reviews,
            live_extents,
            floor: std::mem::take(&mut self.pending_floor),
            now_ns: self.now_ns,
            capture_name: self.namer.take(),
        };
        // Links moved out were already forwarded to the control thread.
        self.links_seen = 0;
        self.carry.merge(batch);
        if self.carry.is_empty() {
            return;
        }
        let batch = Box::new(std::mem::take(&mut self.carry));
        let wait = self.lossless || self.finishing || batch.detections.len() > MAX_CARRY_DETECTIONS;
        let dc = &self.shared.counters.detect;
        match self.writer.try_send(WriterMsg::Batch(batch)) {
            Ok(()) => {}
            Err(TrySendError::Full(WriterMsg::Batch(batch))) if !wait => {
                inc(&dc.writer_queue_full);
                self.carry = *batch;
            }
            Err(TrySendError::Full(msg)) => {
                inc(&dc.writer_blocked);
                if self.writer.send(msg).is_err() {
                    inc(&dc.db_errors);
                }
            }
            Err(TrySendError::Disconnected(_)) => inc(&dc.db_errors),
        }
    }

    /// Flushes everything and waits until the writer has written it.
    fn flush_and_wait(&mut self) {
        self.flush();
        let (ack, done) = mpsc::sync_channel(1);
        if self.writer.send(WriterMsg::Sync(ack)).is_ok() {
            let _ = done.recv_timeout(SYNC_TIMEOUT);
        }
    }

    fn finish(&mut self) {
        self.finishing = true;
        self.namer.finalize(self.shared.fs);
        let mut evs = Vec::new();
        self.det.finish(&mut |ev: DetectorEvent<'_>| match ev {
            DetectorEvent::Detection(r) => evs.push(Owned::Det(r)),
            DetectorEvent::Confirmed(c) => evs.push(Owned::Conf(c)),
            DetectorEvent::Integrated(_) => {}
        });
        let st = self.det.stats();
        let (dense, dropped, frames) = (st.dense_frames, st.dropped_runs, st.frames);
        self.note_dense(dense, dropped, frames);
        let mut tev = Vec::new();
        self.handle_events(evs, &mut tev);
        self.tracker.finish(&mut |te| tev.push(te));
        // Links first, so members and confirmations reach the chains before the closes.
        let closes: Vec<TrackEvent> = tev
            .iter()
            .filter(|e| matches!(e, TrackEvent::Closed(_)))
            .cloned()
            .collect();
        let others: Vec<TrackEvent> = tev
            .into_iter()
            .filter(|e| !matches!(e, TrackEvent::Closed(_)))
            .collect();
        self.handle_track_events(others);
        self.drain_links();
        // Every detection and link is stored before the closes detach the chains that write
        // rows referencing them.
        self.flush_and_wait();
        self.handle_track_events(closes);
        self.drain_links();
        self.flush_and_wait();
        let st = self.det.stats();
        let (dense, invalid, guarded) =
            (st.dense_frames, st.invalid_floor_frames, st.guarded_frames);
        let dc = &self.shared.counters.detect;
        set(&dc.dense_frames, dense);
        set(&dc.invalid_floor_frames, invalid);
        set(&dc.guarded_frames, guarded);
    }
}

/// The writer thread's state: everything received and not yet stored.
struct Writer {
    shared: Arc<Shared>,
    detections: DetectionWriter,
    pending: Vec<DetectionRecord>,
    tracks: TrackBatch,
    closed: Vec<TrackEvent>,
    live: Vec<TrackSummary>,
    /// T-403: the latest live reviews, replaced by each batch the same way the offers are.
    live_reviews: Vec<TrackSummary>,
    /// The latest observed extents (T-388), replaced by each batch and published by [`Writer::write`].
    live_extents: Vec<LiveExtent>,
    /// The `presence` stream: track→emitter bindings the offers teach it, and the tick gate.
    presence: PresenceStream,
    floor: Vec<FloorEvent>,
    anomalies: Option<FloorAnomalies>,
    correlator: Option<(Correlator, FeedCache)>,
    site: Option<Site>,
    now_ns: i64,
}

impl Writer {
    fn new(shared: Arc<Shared>) -> anyhow::Result<Self> {
        let run = format!(
            "{}:{}",
            shared
                .cfg
                .device_id
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || "._:-".contains(c) {
                    c
                } else {
                    '-'
                })
                .collect::<String>(),
            shared.survey_id
        );
        let anomalies = FloorAnomalies::new(FloorAnomalyConfig::new(run)).ok();
        let correlator = match &shared.cfg.feeds_dir {
            Some(dir) => Some((Correlator::default(), FeedCache::open(dir)?)),
            None => None,
        };
        let site = shared.cfg.settings.site.map(|s| Site::new(s[0], s[1]));
        let presence = PresenceStream::new(shared.cfg.stream_sink.as_ref())?;
        Ok(Self {
            detections: DetectionWriter::new(shared.cfg.settings.detection_batch),
            shared,
            pending: Vec::new(),
            tracks: TrackBatch::new(),
            closed: Vec::new(),
            live: Vec::new(),
            live_reviews: Vec::new(),
            live_extents: Vec::new(),
            presence,
            floor: Vec::new(),
            anomalies,
            correlator,
            site,
            now_ns: 0,
        })
    }

    fn absorb(&mut self, batch: WriteBatch) {
        let WriteBatch {
            detections,
            tracks,
            closed,
            live,
            live_reviews,
            live_extents,
            floor,
            now_ns,
            capture_name,
        } = batch;
        if let Some(name) = capture_name {
            self.shared
                .inventory
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .capture_name(&name);
        }
        self.pending.extend(detections);
        merge_tracks(&mut self.tracks, tracks);
        self.closed.extend(closed);
        if !live.is_empty() {
            self.live = live;
        }
        if !live_reviews.is_empty() {
            self.live_reviews = live_reviews;
        }
        if !live_extents.is_empty() {
            self.live_extents = live_extents;
        }
        self.floor.extend(floor);
        self.now_ns = self.now_ns.max(now_ns);
    }

    fn has_work(&self) -> bool {
        !self.pending.is_empty()
            || self.detections.pending() > 0
            || !self.tracks.is_empty()
            || !self.closed.is_empty()
            || !self.live.is_empty()
            || !self.floor.is_empty()
    }

    /// T-410 (ADR-0019): one presence tick — publish the **endpoints** of each open track's
    /// interval, for the tracks the inventory has already given a row. A continuing interval
    /// publishes nothing.
    ///
    /// Deliberately **not** part of [`Writer::write`] and **not** in [`Writer::has_work`]: this
    /// writes nothing to the repository and must not make the writer take the lock (or count a
    /// batch) on a run where the only thing happening is that a signal is still on the air. It runs
    /// once per loop iteration and gates itself to `PRESENCE_PUSH_NS`.
    ///
    /// **An empty extent list is not "nothing to say".** Under T-388's contract it was, because
    /// every record extended something; under ADR-0019 it means *everything stopped*, which is the
    /// most important thing this stream ever says — so the tick runs whenever the stream still
    /// holds an open interval, and only the truly idle case returns early.
    fn publish_presence(&mut self) {
        if (self.live_extents.is_empty() && !self.presence.has_state())
            || !self.presence.due(self.now_ns)
        {
            return;
        }
        // A batch at the cap may have been truncated, so absence from it is not evidence a track
        // closed (`PresenceStream::plan`). Below the cap the list is the whole live set.
        let complete = self.live_extents.len() < MAX_LIVE_EXTENTS;
        // The inventory is the one place that knows which emitter a track's row is, and the lock is
        // taken only for that lookup: no repository work, no writes.
        let (published, deferred) = {
            let inv = self
                .shared
                .inventory
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            self.presence
                .tick(self.now_ns, &self.live_extents, complete, |t| {
                    inv.emitter_of_track(t)
                })
        };
        let dc = &self.shared.counters.detect;
        add(&dc.presence_extensions, published as u64);
        add(&dc.presence_extensions_truncated, deferred as u64);
    }

    /// One write pass (see the module docs for the order and the retry rules).
    fn write(&mut self) {
        let verdicts = self.shared.counters.verdicts.take();
        if !self.has_work() && verdicts.is_empty() {
            return;
        }
        let shared = Arc::clone(&self.shared);
        let dc = &shared.counters.detect;
        inc(&dc.db_batches);
        let now = Timestamp::from_unix_nanos(self.now_ns);
        let mut opened = Vec::new();
        {
            let mut repo = shared.repo();
            let mut retry = Vec::new();
            for r in std::mem::take(&mut self.pending) {
                let buffered = self.detections.pending();
                if self.detections.push(&mut repo, &r).is_err() {
                    inc(&dc.db_errors);
                    // Not buffered (its provenance could not be interned): keep it for the next
                    // pass. A buffered record whose batch write failed stays in the writer.
                    if self.detections.pending() == buffered {
                        retry.push(r);
                    }
                }
            }
            if self.detections.flush(&mut repo).is_err() {
                inc(&dc.db_errors);
            }
            set(&dc.detections_written, self.detections.written());
            self.pending = retry;
            // Track links (and the closes that summarise them) name detections: write them only
            // once every detection is stored, otherwise keep them for the next pass.
            let detections_stored = self.pending.is_empty() && self.detections.pending() == 0;
            let tracks_stored = detections_stored
                && match self.tracks.write(&mut repo) {
                    Ok((tracks, links)) => {
                        add(&dc.track_rows, tracks as u64);
                        add(&dc.track_links, links as u64);
                        // T-913: links whose detection was not stored are counted, not silent.
                        set(&dc.track_links_dropped, self.tracks.links_dropped());
                        true
                    }
                    Err(_) => {
                        inc(&dc.db_errors);
                        false
                    }
                };
            if tracks_stored
                && !(self.closed.is_empty() && self.live.is_empty() && self.live_reviews.is_empty())
            {
                let mut inv = shared
                    .inventory
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                // Live offers first: a carried batch can hold an older snapshot of a track whose
                // close, merge or hop set arrived since, and the end must come after the offer so
                // the inventory can retract it (T-109).
                for s in self.live.drain(..) {
                    if inv.live_track(&mut repo, &s).is_err() {
                        inc(&dc.db_errors);
                    }
                }
                // T-403: then the reviews. They create nothing, so their order against the
                // offers does not matter; they run before the closes for the same reason the
                // offers do — a close must be able to supersede a live decision, never the
                // reverse.
                for s in self.live_reviews.drain(..) {
                    // T-403: whether a chain holds this emission as the review runs. The live
                    // continuous route yields to one — see `ConfirmPolicy::decide`.
                    let measuring = shared
                        .claims
                        .measuring(s.track.f_center_hz, s.track.bandwidth_hz.max(0.0) / 2.0);
                    if inv.live_trust(&mut repo, &s, measuring).is_err() {
                        inc(&dc.db_errors);
                    }
                }
                for e in self.closed.drain(..) {
                    if inv.track_event(&mut repo, &e).is_err() {
                        inc(&dc.db_errors);
                    }
                }
            }
            for e in self.floor.drain(..) {
                inc(&dc.floor_events);
                let Some(life) = self.anomalies.as_mut() else {
                    continue;
                };
                match life.on_floor_event(&mut repo, &e) {
                    Ok(report) => {
                        add(&dc.anomalies_opened, report.opened.len() as u64);
                        add(&dc.anomalies_closed, report.closed.len() as u64);
                        opened.extend(report.opened);
                    }
                    Err(_) => inc(&dc.db_errors),
                }
            }
            if !verdicts.is_empty() {
                let rows: Vec<TrustVerdict> = verdicts
                    .iter()
                    .cloned()
                    .map(|v| v.into_verdict(shared.survey_id))
                    .collect();
                match repo.insert_trust_verdicts(&rows) {
                    Ok(n) => add(&dc.verdicts_written, n as u64),
                    Err(_) => {
                        inc(&dc.db_errors);
                        shared.counters.verdicts.requeue(verdicts);
                    }
                }
            }
        }
        if opened.is_empty() {
            return;
        }
        let Some((correlator, cache)) = &self.correlator else {
            return;
        };
        // Feed-cache file I/O first, without the repository lock.
        match correlator.feed_states(Some(cache)) {
            Ok(states) => {
                let mut repo = shared.repo();
                for id in opened {
                    match correlator.correlate_with_states(
                        &mut repo,
                        &states,
                        id,
                        self.site.as_ref(),
                        now,
                    ) {
                        Ok(out) => add(&dc.explanations, out.written.len() as u64),
                        Err(_) => inc(&dc.db_errors),
                    }
                }
            }
            Err(_) => inc(&dc.db_errors),
        }
    }

    fn take(&mut self, msg: WriterMsg, acks: &mut Vec<SyncSender<()>>) {
        match msg {
            WriterMsg::Batch(b) => self.absorb(*b),
            WriterMsg::Sync(ack) => acks.push(ack),
        }
    }

    /// Runs until the reader drops its sender, then makes a few last attempts.
    fn run(mut self, rx: Receiver<WriterMsg>) {
        loop {
            match rx.recv_timeout(RETRY_INTERVAL) {
                Ok(msg) => {
                    let mut acks = Vec::new();
                    self.take(msg, &mut acks);
                    while let Ok(msg) = rx.try_recv() {
                        self.take(msg, &mut acks);
                    }
                    self.write();
                    self.publish_presence();
                    for ack in acks {
                        let _ = ack.send(());
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.write();
                    self.publish_presence();
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        for _ in 0..3 {
            self.write();
            if !self.has_work() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

/// A detection's occupied band as a member box (T-102): its measured OBW about its centre,
/// clamped inside its pixel box. The pixel box is every bin that crossed threshold in any frame
/// of the record (up to the detector's 1 s max duration), so a wideband modulated emission's
/// flickering skirts inflate it well past the band that holds its power (a WFM station: 408 kHz
/// box vs 333 kHz OBW); the track estimate uses the OBW, so its candidate does too.
fn occupied_box(r: &DetectionRecord) -> (f64, f64) {
    let (lo, hi) = (r.f_lo_hz.min(r.f_hi_hz), r.f_lo_hz.max(r.f_hi_hz));
    let half = 0.5 * r.detection.obw_hz;
    if half.is_nan() || half <= 0.0 || !r.detection.f_center_hz.is_finite() {
        return (lo, hi);
    }
    let fc = r.detection.f_center_hz.clamp(lo, hi);
    ((fc - half).max(lo), (fc + half).min(hi))
}

/// Short-burst detector settings (T-075): the hk-detect defaults, with the longest burst held
/// below what the STFT path can detect (`min_frames − 1` frame periods, at most 5 ms), so a
/// signal is never detected by both paths.
fn burst_config(shared: &Shared) -> BurstConfig {
    let period_s = (shared.fft_len * shared.averages) as f64 / shared.fs;
    let mut min_frames = DetectorConfig::new(shared.survey_id).profile.min_frames;
    if !shared.cfg.settings.short_burst_bands_hz.is_empty() {
        min_frames = min_frames.min(DetectionProfile::short_burst().min_frames);
    }
    let mut cfg = BurstConfig::default();
    cfg.max_duration_s = cfg
        .max_duration_s
        .min(f64::from(min_frames.saturating_sub(1).max(1)) * period_s);
    cfg
}

/// Runs reader 1 until the ring closes.
pub(crate) fn run(shared: Arc<Shared>, tx: Sender<ControlEvent>) -> anyhow::Result<()> {
    let result = run_inner(shared, tx.clone());
    let _ = tx.send(ControlEvent::DetectFinished);
    result
}

fn run_inner(shared: Arc<Shared>, tx: Sender<ControlEvent>) -> anyhow::Result<()> {
    let welch = WelchConfig {
        fft_len: shared.fft_len,
        overlap: 0,
        window: WindowKind::Hann,
        holds: false,
        spectral_kurtosis: true,
    };
    let mut stft = crate::compute::stft(
        &shared.compute,
        &shared.counters.compute,
        crate::compute::Reader::Detect,
        StftConfig::new(welch, shared.averages),
    )?;
    let writer = Writer::new(Arc::clone(&shared))?;
    let (writer_tx, writer_rx) = mpsc::sync_channel(WRITER_QUEUE);
    let writer_join = thread::Builder::new()
        .name("hk-detect-writer".into())
        .spawn(move || writer.run(writer_rx))?;
    let mut node = DetectNode::new(Arc::clone(&shared), tx, writer_tx)?;
    let mut burst = BurstDetector::new(shared.survey_id, burst_config(&shared));
    let mut reader = shared.ring.reader_at(0);
    let cursor = shared.gate.register(0);
    let mut buf = vec![Complex::<i8>::default(); 1 << 16];
    let rc = &shared.counters.detect_reader;
    loop {
        match reader.read_timeout(&mut buf, Duration::from_millis(50)) {
            ReadOutcome::Data(chunk) => {
                let s = &buf[..chunk.len];
                let first = chunk.first_sample();
                for (i, &x) in s.iter().enumerate() {
                    if is_clipped_ci8(x) {
                        node.clips.push_back(first + i as u64);
                    }
                }
                if !node.namer.named() {
                    node.namer
                        .push(first, chunk.time.host_time.as_unix_nanos(), s);
                    if node.namer.complete() {
                        node.namer.finalize(shared.fs);
                    }
                }
                burst.push(
                    chunk.time,
                    chunk.discontinuity,
                    &chunk.provenance,
                    s,
                    &mut |r| node.push_burst(r),
                );
                stft.push(InputInfo::from(&chunk), s, |frame| {
                    node.process_frame(frame)
                });
                cursor.set(chunk.end_sample());
                add(&rc.samples, chunk.len as u64);
            }
            // Nothing new for a read timeout: deliver an asynchronous provider's rows (T-056).
            ReadOutcome::Empty if stft.in_flight() > 0 => {
                stft.flush(|frame| node.process_frame(frame));
            }
            ReadOutcome::Overrun { .. } | ReadOutcome::Empty => {}
            ReadOutcome::Closed => break,
        }
        set(&rc.lost_samples, reader.lost_samples());
        set(&rc.overruns, reader.overruns());
        set(&rc.gap_samples, reader.gap_samples());
        let st = stft.stats();
        set(&rc.frames, st.frames);
        set(&rc.stft_resets, st.resets);
    }
    // Stream end or detach (T-056): frames still in flight are detected and tracked before the
    // burst detector, the tracker and the writer finish.
    stft.flush(|frame| node.process_frame(frame));
    let st = stft.stats();
    set(&rc.frames, st.frames);
    set(&rc.stft_resets, st.resets);
    burst.finish(&mut |r| node.push_burst(r));
    node.finish();
    // Closing the queue ends the writer after its last attempts.
    drop(node);
    let joined = writer_join.join();
    drop(cursor);
    joined.map_err(|_| anyhow::anyhow!("the detection writer thread panicked"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize, seed: i8) -> Vec<Complex<i8>> {
        (0..n)
            .map(|i| Complex::new((i as i8).wrapping_mul(3) ^ seed, seed))
            .collect()
    }

    #[test]
    fn capture_names_are_content_derived_and_independent_of_chunking() {
        let iq = tone(CAPTURE_NAME_SAMPLES + 100, 5);
        let name = |chunks: &[usize], index: u64, fs: f64| {
            let mut n = CaptureNamer::default();
            let mut at = 0;
            for &len in chunks {
                n.push(index + at as u64, 1_000, &iq[at..at + len]);
                at += len;
            }
            assert!(n.complete());
            n.finalize(fs);
            assert!(n.named());
            n.take().unwrap()
        };
        let whole = name(&[CAPTURE_NAME_SAMPLES + 100], 0, 2.4e6);
        assert_eq!(
            whole,
            name(&[1000, 5000, CAPTURE_NAME_SAMPLES - 5900], 0, 2.4e6)
        );
        assert_ne!(
            whole,
            name(&[CAPTURE_NAME_SAMPLES], 7, 2.4e6),
            "stream index"
        );
        assert_ne!(whole, name(&[CAPTURE_NAME_SAMPLES], 0, 2.0e6), "rate");
        let mut other = CaptureNamer::default();
        other.push(0, 1_000, &tone(CAPTURE_NAME_SAMPLES, 6));
        other.finalize(2.4e6);
        assert_ne!(Some(whole.clone()), other.take(), "content");
        assert!(whole.starts_with("hk-iq:fnv1a64:"));
        let mut empty = CaptureNamer::default();
        empty.finalize(2.4e6);
        assert!(!empty.named() && empty.take().is_none());
        let mut short = CaptureNamer::default();
        short.push(0, 0, &iq[..10]);
        assert!(!short.complete());
        short.finalize(2.4e6);
        assert!(short.take().is_some(), "a short stream is named at its end");
    }
}
