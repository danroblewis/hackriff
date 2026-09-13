//! Always-on reader 1: STFT → noise floor → detection → tracking → batched repository writes, and
//! floor events → anomaly lifecycle (+ correlation with the offline feed cache).
//!
//! - **STFT:** Hann, 0 % overlap, `K` averages (see [`crate::config`] for why), SK on.
//! - **Clip counts:** the reader scans each raw ci8 chunk for clipped samples and keeps their
//!   stream indices until the frame covering them is processed, so every frame gets its exact
//!   clip count (`hk_detect::count_clipped_ci8` semantics).
//! - **Batching (ADR-0006):** detections are buffered and written with tracks, track links,
//!   closed-track inventory calls and floor-event anomalies in one repository lock every
//!   `flush_interval_s` of stream time (0.5 s) or `detection_batch` detections; detections are
//!   always written before the track links that name them.
//! - **Control events:** member boxes of each track, a track's first trustworthy confirmation
//!   (not a spur, image or impulsive candidate), closes, merges and (with the scheduler)
//!   segment capture results.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Duration;

use hk_context::{Correlator, FeedCache, FloorAnomalies, FloorAnomalyConfig, Site};
use hk_core::ReadOutcome;
use hk_detect::clip::is_clipped_ci8;
use hk_detect::{
    BandProfile, ClipCount, Confirmation, DetectionProfile, DetectionRecord, DetectionWriter,
    Detector, DetectorConfig, DetectorEvent, TrackBatch, TrackEvent, Tracker, TrackerConfig,
};
use hk_dsp::floor::{FloorConfig, FloorEvent, NoiseFloorTracker};
use hk_dsp::window::WindowKind;
use hk_dsp::{InputInfo, SpectrumFrame, StftConfig, StftProcessor, WelchConfig};
use hk_model::{DetectionId, FreqRange, Timestamp, TrackId};
use num_complex::Complex;

use crate::events::{Candidate, ControlEvent, MemberBox};
use crate::run::Shared;
use crate::stats::{add, inc, set};

/// Boxes and links older than this (stream time) are forgotten.
const MEMORY_NS: i64 = 120_000_000_000;

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

struct DetectNode {
    shared: Arc<Shared>,
    tx: Sender<ControlEvent>,
    floor: NoiseFloorTracker,
    det: Detector,
    tracker: Tracker,
    writer: DetectionWriter,
    batch: TrackBatch,
    anomalies: Option<FloorAnomalies>,
    correlator: Option<(Correlator, FeedCache)>,
    site: Option<Site>,
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
}

impl DetectNode {
    fn new(shared: Arc<Shared>, tx: Sender<ControlEvent>) -> anyhow::Result<Self> {
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
            writer: DetectionWriter::new(shared.cfg.settings.detection_batch),
            shared,
            tx,
            floor,
            det,
            tracker: Tracker::new(TrackerConfig::default()),
            batch: TrackBatch::new(),
            anomalies,
            correlator,
            site,
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
        })
    }

    fn send(&self, ev: ControlEvent) {
        let _ = self.tx.send(ev);
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
        if stats.segments != self.segments {
            if self.scheduler_captures && self.segments > 0 {
                if let Some(c) = self.det.last_capture() {
                    inc(&self.shared.counters.detect.captures);
                    self.send(ControlEvent::Capture {
                        t_start: self.segment_start,
                        capture: Box::new(c.clone()),
                    });
                }
            }
            self.segments = stats.segments;
            self.segment_start = frame.t.host_time;
        }
        let dc = &self.shared.counters.detect;
        set(&dc.dense_frames, stats.dense_frames);
        set(&dc.invalid_floor_frames, stats.invalid_floor_frames);
        set(&dc.guarded_frames, stats.guarded_frames);

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

    fn flush_ns(&self) -> i64 {
        (self.shared.cfg.settings.flush_interval_s * 1e9) as i64
    }

    fn handle_events(&mut self, evs: Vec<Owned>, tev: &mut Vec<TrackEvent>) {
        let shared = Arc::clone(&self.shared);
        let dc = &shared.counters.detect;
        for ev in evs {
            match ev {
                Owned::Det(r) => {
                    inc(&dc.detections);
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

    fn flush(&mut self) {
        self.last_flush_ns = Some(self.now_ns);
        if self.pending.is_empty()
            && self.batch.is_empty()
            && self.closed.is_empty()
            && self.pending_floor.is_empty()
        {
            return;
        }
        let shared = Arc::clone(&self.shared);
        let dc = &shared.counters.detect;
        let mut repo = shared.repo();
        inc(&dc.db_batches);
        for r in self.pending.drain(..) {
            if self.writer.push(&mut repo, &r).is_err() {
                inc(&dc.db_errors);
            }
        }
        if self.writer.flush(&mut repo).is_err() {
            inc(&dc.db_errors);
        }
        set(&dc.detections_written, self.writer.written());
        match self.batch.write(&mut repo) {
            Ok((tracks, links)) => {
                add(&dc.track_rows, tracks as u64);
                add(&dc.track_links, links as u64);
            }
            Err(_) => inc(&dc.db_errors),
        }
        self.links_seen = 0;
        {
            let mut inv = shared
                .inventory
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for e in self.closed.drain(..) {
                if inv.track_event(&mut repo, &e).is_err() {
                    inc(&dc.db_errors);
                }
            }
        }
        let now = Timestamp::from_unix_nanos(self.now_ns);
        for e in self.pending_floor.drain(..) {
            inc(&dc.floor_events);
            let Some(life) = self.anomalies.as_mut() else {
                continue;
            };
            match life.on_floor_event(&mut repo, &e) {
                Ok(report) => {
                    add(&dc.anomalies_opened, report.opened.len() as u64);
                    add(&dc.anomalies_closed, report.closed.len() as u64);
                    if let Some((correlator, cache)) = &self.correlator {
                        for id in report.opened {
                            match correlator.correlate(
                                &mut repo,
                                Some(cache),
                                id,
                                self.site.as_ref(),
                                now,
                            ) {
                                Ok(out) => add(&dc.explanations, out.written.len() as u64),
                                Err(_) => inc(&dc.db_errors),
                            }
                        }
                    }
                }
                Err(_) => inc(&dc.db_errors),
            }
        }
    }

    fn finish(&mut self) {
        let mut evs = Vec::new();
        self.det.finish(&mut |ev: DetectorEvent<'_>| match ev {
            DetectorEvent::Detection(r) => evs.push(Owned::Det(r)),
            DetectorEvent::Confirmed(c) => evs.push(Owned::Conf(c)),
            DetectorEvent::Integrated(_) => {}
        });
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
        self.handle_track_events(closes);
        self.drain_links();
        self.flush();
        let st = self.det.stats();
        let dc = &self.shared.counters.detect;
        set(&dc.dense_frames, st.dense_frames);
        set(&dc.invalid_floor_frames, st.invalid_floor_frames);
        set(&dc.guarded_frames, st.guarded_frames);
    }
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
    let mut node = DetectNode::new(Arc::clone(&shared), tx)?;
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
    node.finish();
    drop(cursor);
    Ok(())
}
