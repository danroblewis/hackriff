//! Runtime chains (ADR-0001 "Spike S1 outcome"): a chain is a ring reader attached at runtime
//! with a data-built node list, running on its own thread, detached by dropping it. Attaching or
//! detaching never touches the capture thread or the always-on readers; in lossless replay the
//! chain's [`GateCursor`](crate::gate::GateCursor) joins the flow gate for as long as it lives.
//!
//! - [`ChainManager`] (control thread): selects a spec for a confirmed track or a covered band
//!   ([`spec`]), refuses content chains under a class that forbids content, spawns the chain and
//!   its recorder, forwards member boxes (with a backlog for boxes that arrived before the
//!   confirmation), detaches on track close / merge / coverage loss, and reaps finished threads.
//! - Chain bodies: [`analog`] (C19 auto mode + RDS), [`fsk`] (C13/C14/C20/C21), [`plugin`]
//!   (DDC + subprocess decoder), [`record`] (pre-trigger SigMF, C25).
//!
//! **Claims at attach (T-037b).** Every chain and recorder registers its gate cursor on the
//! control thread when it is attached, at the first sample it will read, and hands it to its
//! thread. Capture cannot run past those samples in lossless replay while the thread starts
//! (loads a manifest, spawns a plugin). Coverage polls acknowledge the tune they evaluated
//! (`Counters::coverage_seq`), which the capture thread waits for ([`crate::capture`]).
//!
//! **Channel cooldown.** A raster channel whose chain finished is left alone for
//! [`CHANNEL_COOLDOWN_S`] of stream time (`channel_cooldown`): a WFM station's many fragment
//! tracks, or noise on an empty channel whose probe was rejected, do not start a new probe (and a
//! new demodulation) for every fragment.

pub(crate) mod analog;
pub(crate) mod fsk;
pub mod listen;
pub(crate) mod plugin;
pub(crate) mod record;
pub mod spec;
pub mod taps;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use hk_core::{ReadChunk, ReadOutcome, ResyncPolicy, RingReader};
use hk_model::{DetectionId, RecordingTrigger, TrackId};
use num_complex::Complex;

use crate::events::{Candidate, MemberBox};
use crate::gate::GateCursor;
use crate::run::Shared;
use crate::stats::{add, inc};
use spec::{ChainShape, ChainSpec, Trigger, select_for_track};

/// Messages to a running chain.
#[derive(Debug)]
pub(crate) enum ChainMsg {
    /// A member box of the chain's track.
    Member(MemberBox),
    /// Finish: flush what the chain has and exit.
    Detach,
}

/// A chain's ring reader.
pub(crate) struct ChainReader {
    pub shared: Arc<Shared>,
    reader: RingReader<Complex<i8>>,
    cursor: GateCursor,
    pub buf: Vec<Complex<i8>>,
}

/// One read.
pub(crate) enum Next {
    Data(ReadChunk),
    Lost,
    Idle,
    Closed,
}

impl ChainReader {
    /// Attaches at `start` (clamped to the oldest retained sample) with the gate `cursor` the
    /// chain claimed at attach.
    pub fn new(shared: Arc<Shared>, start: u64, cursor: GateCursor) -> Self {
        let start = start.max(shared.ring.oldest_sample().unwrap_or(0));
        cursor.set(start);
        let reader = shared
            .ring
            .reader_at(start)
            .with_resync_policy(ResyncPolicy::Oldest);
        Self {
            shared,
            reader,
            cursor,
            buf: vec![Complex::default(); 1 << 16],
        }
    }

    /// Reads the next chunk into `buf` (waits up to 20 ms).
    pub fn next(&mut self) -> Next {
        let c = &self.shared.counters.chains;
        match self
            .reader
            .read_timeout(&mut self.buf, Duration::from_millis(20))
        {
            ReadOutcome::Data(chunk) => {
                add(&c.samples, chunk.len as u64);
                Next::Data(chunk)
            }
            ReadOutcome::Overrun { lost_samples, .. } => {
                add(&c.lost_samples, lost_samples);
                Next::Lost
            }
            ReadOutcome::Empty => Next::Idle,
            ReadOutcome::Closed => Next::Closed,
        }
    }

    /// The chain no longer needs samples before `sample`.
    pub fn release_to(&self, sample: u64) {
        self.cursor.set(sample);
    }
}

struct Running {
    id: u64,
    tx: Option<Sender<ChainMsg>>,
    join: JoinHandle<()>,
}

/// Longest wait for a chain row's parent detection ([`stored_detection`]).
pub(crate) const PARENT_WAIT: Duration = Duration::from_secs(5);

/// The triggering detection a chain's rows may reference (T-037b). The detection writer thread
/// stores detections asynchronously, and under load (a detect reader overrunning at 8–10 Msps,
/// T-055) or a failing store the row can lag or never arrive; writing a Demodulation, Decode or
/// Recording that names it then fails the foreign key. Waits up to [`PARENT_WAIT`] for the row;
/// a detection still missing is dropped from the reference and counted
/// (`detection_ref_missing`): the chain's rows are neither refused nor orphaned.
pub(crate) fn stored_detection(
    shared: &Shared,
    detection: Option<DetectionId>,
) -> Option<DetectionId> {
    let d = detection?;
    let deadline = std::time::Instant::now() + PARENT_WAIT;
    loop {
        if shared.repo().detection(d).is_ok() {
            return Some(d);
        }
        if std::time::Instant::now() >= deadline {
            inc(&shared.counters.chains.detection_ref_missing);
            return None;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Stream time a raster channel is left alone after its chain finished, s.
pub(crate) const CHANNEL_COOLDOWN_S: f64 = 30.0;

type ChannelKey = (String, i64);

/// When each raster channel may attach again (stream sample index).
#[derive(Debug, Default)]
pub(crate) struct ChannelMemory {
    until: HashMap<ChannelKey, u64>,
}

impl ChannelMemory {
    /// The channel's chain finished less than the cooldown ago.
    pub fn cooling(&self, key: &ChannelKey, now: u64) -> bool {
        self.until.get(key).is_some_and(|&u| now < u)
    }

    /// The channel's chain finished at `now`.
    pub fn finished(&mut self, key: ChannelKey, now: u64, cooldown: u64) {
        self.until.insert(key, now.saturating_add(cooldown));
        if self.until.len() > 4096 {
            self.until.retain(|_, u| *u > now);
        }
    }
}

/// The first sample a chain of `shape` reads for `cand` (claimed at attach).
fn chain_start(shape: &ChainShape, cand: &Candidate, fs: f64) -> u64 {
    let pre_s = match shape {
        ChainShape::Analog { pre_s, .. } => *pre_s,
        ChainShape::Fsk { pad_s, .. } => *pad_s,
        ChainShape::Plugin { .. } => 0.0,
    };
    cand.first_sample.saturating_sub((pre_s * fs) as u64)
}

/// Attaches, feeds, detaches and reaps runtime chains (control thread).
pub(crate) struct ChainManager {
    shared: Arc<Shared>,
    running: Vec<Running>,
    next_id: u64,
    by_track: HashMap<TrackId, u64>,
    coverage: HashMap<String, u64>,
    manual: Vec<u64>,
    backlog: HashMap<TrackId, Vec<MemberBox>>,
    members: HashMap<TrackId, u32>,
    pending: HashMap<TrackId, Candidate>,
    by_channel: HashMap<ChannelKey, u64>,
    cooldown: ChannelMemory,
}

const BACKLOG_PER_TRACK: usize = 512;

impl ChainManager {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            running: Vec::new(),
            next_id: 0,
            by_track: HashMap::new(),
            coverage: HashMap::new(),
            manual: Vec::new(),
            backlog: HashMap::new(),
            members: HashMap::new(),
            pending: HashMap::new(),
            by_channel: HashMap::new(),
            cooldown: ChannelMemory::default(),
        }
    }

    /// Running chains and recorders.
    pub fn running(&self) -> usize {
        self.running.len()
    }

    fn send(&self, id: u64, msg: ChainMsg) {
        if let Some(tx) = self
            .running
            .iter()
            .find(|r| r.id == id)
            .and_then(|r| r.tx.as_ref())
        {
            let _ = tx.send(msg);
        }
    }

    /// Spawns `spec` for `cand`; returns the chain id.
    pub fn attach(&mut self, spec: &ChainSpec, cand: Candidate) -> Option<u64> {
        let c = &self.shared.counters.chains;
        let shape = match spec.shape() {
            Ok(s) => s,
            Err(e) => {
                inc(&c.attach_errors);
                eprintln!("hk-pipeline: chain {}: {e}", spec.id);
                return None;
            }
        };
        let class = self.shared.cfg.source_class;
        if crate::debug_enabled() {
            eprintln!(
                "hk-pipeline: attach {} for {:.4}..{:.4} MHz (bursty {:?}) from sample {} trigger {}",
                spec.id,
                cand.f_lo_hz / 1e6,
                cand.f_hi_hz / 1e6,
                cand.bursty,
                cand.first_sample,
                cand.trigger_sample
            );
        }
        if spec.requires_content && !class.permits_content() {
            inc(&c.refused_class);
            return None;
        }
        // An analog chain with a probe records only once mode selection accepted the channel.
        let deferred = matches!(shape, ChainShape::Analog { probe_s, .. } if probe_s > 0.0);
        let mut analog_record = None;
        if let Some((pre_s, post_s)) = spec.record() {
            let trigger = match cand.detection {
                Some(d) => RecordingTrigger::Detection(d),
                None => RecordingTrigger::Scheduler,
            };
            if class.permits_content() && deferred {
                analog_record = Some(analog::RecordAfterProbe {
                    pre_s,
                    post_s,
                    trigger,
                    label: spec.id.clone(),
                });
            } else if class.permits_content() {
                let shared = Arc::clone(&self.shared);
                let label = spec.id.clone();
                let at = cand.trigger_sample;
                let cursor = record::claim(&self.shared, at, pre_s);
                let id = self.next_id;
                self.next_id += 1;
                if let Ok(join) =
                    thread::Builder::new()
                        .name(format!("hk-rec-{id}"))
                        .spawn(move || {
                            record::run_claimed(
                                shared,
                                Some(cursor),
                                at,
                                pre_s,
                                post_s,
                                trigger,
                                label,
                            )
                        })
                {
                    self.running.push(Running { id, tx: None, join });
                }
            } else {
                inc(&c.recordings_refused_class);
            }
        }
        let (tx, rx) = mpsc::channel();
        let shared = Arc::clone(&self.shared);
        let id = self.next_id;
        self.next_id += 1;
        let spec_id = spec.id.clone();
        let cursor = self
            .shared
            .gate
            .register(chain_start(&shape, &cand, self.shared.fs));
        // On a raster the receiver must lock inside this channel, not a neighbour's.
        let channel_tolerance_hz = spec.raster_hz.map_or(f64::INFINITY, |r| 0.5 * r);
        let spawned = thread::Builder::new()
            .name(format!("hk-chain-{}-{id}", spec.id))
            .spawn(move || match shape {
                ChainShape::Analog {
                    pre_s,
                    window_s,
                    bandwidth_hz,
                    probe_s,
                    accept_modes,
                    require_pilot,
                } => analog::run(
                    shared,
                    rx,
                    cand,
                    analog::AnalogNode {
                        pre_s,
                        window_s,
                        bandwidth_hz,
                        probe_s,
                        accept_modes,
                        require_pilot,
                        channel_tolerance_hz,
                        record: analog_record,
                    },
                    cursor,
                ),
                ChainShape::Fsk {
                    pad_s,
                    retain_s,
                    min_bursts,
                    max_bursts,
                } => fsk::run(
                    shared, rx, cand, pad_s, retain_s, min_bursts, max_bursts, cursor,
                ),
                ChainShape::Plugin {
                    ddc,
                    manifest,
                    tail_pad_samples,
                    settle_s,
                } => plugin::run(
                    shared,
                    rx,
                    cand,
                    &spec_id,
                    ddc,
                    &manifest,
                    tail_pad_samples,
                    settle_s,
                    cursor,
                ),
            });
        match spawned {
            Ok(join) => {
                inc(&c.attached);
                self.running.push(Running {
                    id,
                    tx: Some(tx),
                    join,
                });
                Some(id)
            }
            Err(_) => {
                inc(&c.attach_errors);
                None
            }
        }
    }

    /// A track confirmed: it becomes a candidate. Selection runs now and again whenever a member
    /// box widens the candidate, until a spec matches with its `min_detections` met; a candidate
    /// still without a chain when its track closes counts as `unmatched`. (The first confirmation
    /// often sees a single tone lobe or fragment, narrower than the emission.)
    pub fn on_confirmed(&mut self, cand: Candidate) {
        let Some(track) = cand.track else { return };
        if self.by_track.contains_key(&track) || self.pending.contains_key(&track) {
            return;
        }
        self.pending.insert(track, cand);
        self.try_attach(track);
    }

    fn try_attach(&mut self, track: TrackId) {
        let Some(cand) = self.pending.get(&track) else {
            return;
        };
        let count = self.members.get(&track).copied().unwrap_or(0);
        let Some(spec) =
            select_for_track(&self.shared.specs, cand.f_lo_hz, cand.f_hi_hz, cand.bursty).cloned()
        else {
            return;
        };
        if count < spec.min_detections {
            return;
        }
        let cand = self.pending.remove(&track).expect("pending candidate");
        if crate::debug_enabled() {
            eprintln!(
                "hk-pipeline: {} selected for {:.4}..{:.4} MHz ({count} detections, bursty {:?})",
                spec.id,
                cand.f_lo_hz / 1e6,
                cand.f_hi_hz / 1e6,
                cand.bursty
            );
        }
        self.attach_track(track, &spec, cand);
    }

    fn attach_track(&mut self, track: TrackId, spec: &ChainSpec, mut cand: Candidate) {
        let c = &self.shared.counters.chains;
        let (lo, hi) = spec.channel(cand.f_lo_hz, cand.f_hi_hz);
        cand.f_lo_hz = lo;
        cand.f_hi_hz = hi;
        let (center, rate) = self.shared.counters.tune();
        let fc = 0.5 * (lo + hi);
        if rate > 0.0 && (fc - center).abs() + 0.5 * (hi - lo) > 0.48 * rate {
            inc(&c.outside_window);
            return;
        }
        let key = spec.raster_hz.map(|_| (spec.id.clone(), fc.round() as i64));
        if let Some(k) = &key {
            let now = self.shared.ring.next_sample().unwrap_or(0);
            if self.cooldown.cooling(k, now) {
                inc(&c.channel_cooldown);
                return;
            }
            if let Some(id) = self.by_channel.get(k) {
                if self.running.iter().any(|r| r.id == *id) {
                    inc(&c.duplicate_channel);
                    return;
                }
            }
        }
        if let Some(id) = self.attach(spec, cand) {
            self.by_track.insert(track, id);
            if let Some(k) = key {
                self.by_channel.insert(k, id);
            }
            for m in self.backlog.remove(&track).unwrap_or_default() {
                self.send(id, ChainMsg::Member(m));
            }
        }
    }

    /// Attaches a spec on request (API / tests).
    pub fn attach_manual(&mut self, spec: &ChainSpec, cand: Candidate) {
        if let Some(id) = self.attach(spec, cand) {
            self.manual.push(id);
        }
    }

    /// Detaches every manual chain.
    pub fn detach_manual(&mut self) {
        for id in std::mem::take(&mut self.manual) {
            self.send(id, ChainMsg::Detach);
        }
    }

    pub fn on_member(&mut self, track: TrackId, member: MemberBox) {
        *self.members.entry(track).or_insert(0) += 1;
        let fs = self.shared.fs;
        if let Some(c) = self.pending.get_mut(&track) {
            c.f_lo_hz = c.f_lo_hz.min(member.f_lo_hz);
            c.f_hi_hz = c.f_hi_hz.max(member.f_hi_hz);
            c.first_sample = c.first_sample.min(member.samples.start);
            let dur_s = (member.samples.end - member.samples.start) as f64 / fs;
            if member.continues {
                c.bursty = Some(false);
            } else if c.bursty.is_none() && dur_s < 0.5 {
                c.bursty = Some(true);
            }
        }
        match self.by_track.get(&track) {
            Some(&id) => self.send(id, ChainMsg::Member(member)),
            None => {
                let b = self.backlog.entry(track).or_default();
                if b.len() < BACKLOG_PER_TRACK {
                    b.push(member);
                }
            }
        }
        if self.pending.contains_key(&track) {
            self.try_attach(track);
        }
    }

    pub fn on_track_closed(&mut self, track: TrackId) {
        self.backlog.remove(&track);
        self.members.remove(&track);
        if let Some(c) = self.pending.remove(&track) {
            if crate::debug_enabled() {
                eprintln!(
                    "hk-pipeline: no chain for {:.4}..{:.4} MHz ({:.1} kHz, bursty {:?})",
                    c.f_lo_hz / 1e6,
                    c.f_hi_hz / 1e6,
                    (c.f_hi_hz - c.f_lo_hz) / 1e3,
                    c.bursty
                );
            }
            inc(&self.shared.counters.chains.unmatched);
        }
        if let Some(id) = self.by_track.remove(&track) {
            self.send(id, ChainMsg::Detach);
        }
    }

    pub fn on_merged(&mut self, from: TrackId, into: TrackId) {
        let moved = self.members.remove(&from).unwrap_or(0);
        *self.members.entry(into).or_insert(0) += moved;
        if let Some(mut b) = self.backlog.remove(&from) {
            let into_b = self.backlog.entry(into).or_default();
            into_b.append(&mut b);
            into_b.truncate(BACKLOG_PER_TRACK);
        }
        if let Some(id) = self.by_track.remove(&from) {
            if let std::collections::hash_map::Entry::Vacant(e) = self.by_track.entry(into) {
                e.insert(id);
            } else {
                self.send(id, ChainMsg::Detach);
            }
        }
        if let Some(mut p) = self.pending.remove(&from) {
            if let Some(q) = self.pending.get_mut(&into) {
                q.f_lo_hz = q.f_lo_hz.min(p.f_lo_hz);
                q.f_hi_hz = q.f_hi_hz.max(p.f_hi_hz);
                q.first_sample = q.first_sample.min(p.first_sample);
            } else if !self.by_track.contains_key(&into) {
                p.track = Some(into);
                self.pending.insert(into, p);
            }
        }
        if self.pending.contains_key(&into) {
            self.try_attach(into);
        }
    }

    /// Attaches coverage chains whose band the window covers; detaches those it no longer does.
    /// Acknowledges the tune it evaluated (`Counters::coverage_seq`).
    pub fn poll_coverage(&mut self) {
        let counters = Arc::clone(&self.shared.counters);
        // Read before the tune: the tune evaluated is at least as new as the acknowledged one.
        let seq = counters.tune_seq.load(Ordering::SeqCst);
        self.evaluate_coverage();
        counters.coverage_seq.fetch_max(seq, Ordering::SeqCst);
    }

    fn evaluate_coverage(&mut self) {
        let (center, rate) = self.shared.counters.tune();
        if rate.is_nan() || rate <= 0.0 || self.shared.ring.is_closed() {
            return;
        }
        let specs: Vec<ChainSpec> = self
            .shared
            .specs
            .iter()
            .filter(|s| s.trigger == Trigger::Coverage)
            .cloned()
            .collect();
        for spec in specs {
            let covered = spec.covered_by(center, rate);
            match (covered, self.coverage.get(&spec.id).copied()) {
                (true, None) => {
                    let band = spec.freq_hz[0];
                    let at = self.shared.ring.oldest_sample().unwrap_or(0);
                    let cand = Candidate {
                        track: None,
                        detection: None,
                        f_lo_hz: band[0],
                        f_hi_hz: band[1],
                        first_sample: at,
                        trigger_sample: at,
                        bursty: None,
                    };
                    if let Some(id) = self.attach(&spec, cand) {
                        self.coverage.insert(spec.id.clone(), id);
                    }
                }
                (false, Some(id)) => {
                    self.coverage.remove(&spec.id);
                    self.send(id, ChainMsg::Detach);
                }
                _ => {}
            }
        }
    }

    /// Detaches everything.
    pub fn detach_all(&mut self) {
        for r in &self.running {
            if let Some(tx) = &r.tx {
                let _ = tx.send(ChainMsg::Detach);
            }
        }
        self.by_track.clear();
        self.manual.clear();
    }

    /// Joins finished chains.
    pub fn reap(&mut self) {
        let mut i = 0;
        while i < self.running.len() {
            if self.running[i].join.is_finished() {
                let r = self.running.swap_remove(i);
                let _ = r.join.join();
                if r.tx.is_some() {
                    inc(&self.shared.counters.chains.detached);
                }
                self.by_track.retain(|_, v| *v != r.id);
                let now = self.shared.ring.next_sample().unwrap_or(0);
                let cooldown = (CHANNEL_COOLDOWN_S * self.shared.fs) as u64;
                let channels: Vec<ChannelKey> = self
                    .by_channel
                    .iter()
                    .filter(|(_, id)| **id == r.id)
                    .map(|(k, _)| k.clone())
                    .collect();
                for k in channels {
                    self.by_channel.remove(&k);
                    self.cooldown.finished(k, now, cooldown);
                }
                // A finished coverage chain is not re-attached while the window still covers
                // its band: keep its entry (removed only on coverage loss).
            } else {
                i += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finished_raster_channels_cool_down_for_the_stream_time_given() {
        let mut m = ChannelMemory::default();
        let key = ("wfm-rds".to_owned(), 101_300_000);
        let other = ("wfm-rds".to_owned(), 101_500_000);
        assert!(!m.cooling(&key, 0));
        m.finished(key.clone(), 1_000, 500);
        assert!(m.cooling(&key, 1_000) && m.cooling(&key, 1_499));
        assert!(!m.cooling(&key, 1_500), "cooled down");
        assert!(!m.cooling(&other, 1_200), "other channels are unaffected");
        // Stream time advances: each channel's cooldown has expired by the next insert.
        for i in 0..5000 {
            m.finished(("x".to_owned(), i), 2_000 + i as u64, 1);
        }
        assert!(m.until.len() <= 4097, "expired channels are pruned");
    }

    #[test]
    fn chains_claim_their_first_sample_by_shape() {
        let cand = Candidate {
            track: None,
            detection: None,
            f_lo_hz: 0.0,
            f_hi_hz: 1.0,
            first_sample: 1_000_000,
            trigger_sample: 1_000_000,
            bursty: None,
        };
        let fs = 1e6;
        let analog = ChainShape::Analog {
            pre_s: 0.5,
            window_s: 1.0,
            bandwidth_hz: 1e5,
            probe_s: 0.0,
            accept_modes: Vec::new(),
            require_pilot: false,
        };
        let fsk = ChainShape::Fsk {
            pad_s: 0.02,
            retain_s: 1.0,
            min_bursts: 1,
            max_bursts: 2,
        };
        let plugin = ChainShape::Plugin {
            ddc: None,
            manifest: String::new(),
            tail_pad_samples: 0,
            settle_s: 0.0,
        };
        assert_eq!(chain_start(&analog, &cand, fs), 500_000);
        assert_eq!(chain_start(&fsk, &cand, fs), 980_000);
        assert_eq!(chain_start(&plugin, &cand, fs), 1_000_000);
    }
}
