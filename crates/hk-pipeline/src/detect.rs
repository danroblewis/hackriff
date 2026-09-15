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

use hk_context::{Correlator, FeedCache, FloorAnomalies, FloorAnomalyConfig, Site};
use hk_core::ReadOutcome;
use hk_detect::clip::is_clipped_ci8;
use hk_detect::{
    BandProfile, BurstConfig, BurstDetector, ClipCount, Confirmation, DetectionProfile,
    DetectionRecord, DetectionWriter, Detector, DetectorConfig, DetectorEvent, TrackBatch,
    TrackEvent, Tracker, TrackerConfig,
};
use hk_dsp::floor::{FloorConfig, FloorEvent, NoiseFloorTracker};
use hk_dsp::window::WindowKind;
use hk_dsp::{InputInfo, SpectrumFrame, StftConfig, StftProcessor, WelchConfig};
use hk_model::{DetectionId, FreqRange, Timestamp, TrackId, TrustVerdict};
use num_complex::Complex;

use crate::events::{Candidate, ControlEvent, MemberBox};
use crate::run::Shared;
use crate::stats::{add, inc, set};

/// Boxes and links older than this (stream time) are forgotten.
const MEMORY_NS: i64 = 120_000_000_000;
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
            && self.floor.is_empty()
            && self.capture_name.is_none()
    }

    fn merge(&mut self, other: WriteBatch) {
        let WriteBatch {
            mut detections,
            tracks,
            mut closed,
            mut floor,
            now_ns,
            capture_name,
        } = other;
        self.detections.append(&mut detections);
        merge_tracks(&mut self.tracks, tracks);
        self.closed.append(&mut closed);
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
    now_ns: i64,
    segments: u64,
    segment_start: Timestamp,
    scheduler_captures: bool,
    dense_seen: (u64, u64),
    dense_frames: VecDeque<u64>,
    namer: CaptureNamer,
    finishing: bool,
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
        Ok(Self {
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
                    if !(f.spur_candidate || f.image_candidate || f.impulsive) {
                        self.good.insert(id);
                    }
                    if r.candidate.is_confirmed() {
                        self.confirmed.insert(id);
                    }
                    let t = r.detection.time.start;
                    self.boxes.insert(
                        id,
                        MemberBox {
                            detection: id,
                            samples: r.samples.clone(),
                            f_lo_hz: r.f_lo_hz,
                            f_hi_hz: r.f_hi_hz,
                            t_start: t,
                            continues: r.continues,
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
        while self
            .order
            .front()
            .is_some_and(|&(t, _)| self.now_ns - t > MEMORY_NS)
        {
            let (_, id) = self.order.pop_front().expect("front");
            self.boxes.remove(&id);
            self.good.remove(&id);
            self.confirmed.remove(&id);
            self.det_track.remove(&id);
        }
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
                TrackEvent::Merged { from, into, .. } => {
                    inc(&dc.track_merges);
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
        let batch = WriteBatch {
            detections: std::mem::take(&mut self.pending),
            tracks: std::mem::take(&mut self.batch),
            closed,
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
        Ok(Self {
            detections: DetectionWriter::new(shared.cfg.settings.detection_batch),
            shared,
            pending: Vec::new(),
            tracks: TrackBatch::new(),
            closed: Vec::new(),
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
        self.floor.extend(floor);
        self.now_ns = self.now_ns.max(now_ns);
    }

    fn has_work(&self) -> bool {
        !self.pending.is_empty()
            || self.detections.pending() > 0
            || !self.tracks.is_empty()
            || !self.closed.is_empty()
            || !self.floor.is_empty()
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
                        true
                    }
                    Err(_) => {
                        inc(&dc.db_errors);
                        false
                    }
                };
            if tracks_stored && !self.closed.is_empty() {
                let mut inv = shared
                    .inventory
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
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
                    for ack in acks {
                        let _ = ack.send(());
                    }
                }
                Err(RecvTimeoutError::Timeout) => self.write(),
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
    let mut stft = StftProcessor::new(StftConfig::new(welch, shared.averages))
        .map_err(|e| anyhow::anyhow!("detection STFT: {e:?}"))?;
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
