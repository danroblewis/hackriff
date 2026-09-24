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
//!   (DDC + subprocess decoder), [`record`] (pre-trigger SigMF, C25), [`trunk`] (C23
//!   control-channel hunt, T-287; metadata only), [`sweep`] (sweep characterisation of a candidate
//!   region, T-297; metadata only), [`classify`] (the C15 classifier over a candidate region,
//!   T-878; metadata only).
//!
//! **Selection versus measurement (T-297).** [`Trigger::ConfirmedTrack`] *selects*: the first
//! matching spec wins and the rest never run. That is right for decoding and wrong for measuring,
//! so [`Trigger::EveryTrack`] chains attach **beside** the selected one, capped by their own node
//! spec and counted apart from it (`sweep_attached`, not `attached`).
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
pub mod budget;
pub(crate) mod classify;
pub(crate) mod fsk;
pub mod iq;
pub mod listen;
pub mod outputs;
pub(crate) mod plugin;
pub(crate) mod record;
pub mod spec;
pub(crate) mod sweep;
pub mod taps;
pub(crate) mod trunk;

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use hk_core::{ReadChunk, ReadOutcome, ResyncPolicy, RingReader};
use hk_model::{DetectionId, RecordingTrigger, TrackId};
use num_complex::Complex;

use crate::events::{Candidate, MemberBox};
use crate::gate::GateCursor;
use crate::run::Shared;
use crate::stats::{ChainStat, CpuClock, add, inc};
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
    /// The chain's own counters (T-071), from [`set_thread_stat`] or [`Self::with_stat`].
    stat: Option<Arc<ChainStat>>,
    clock: CpuClock,
}

thread_local! {
    static THREAD_STAT: RefCell<Option<Arc<ChainStat>>> = const { RefCell::new(None) };
}

/// The chain running on this thread: readers created on it account to `stat` (T-071).
pub(crate) fn set_thread_stat(stat: Option<Arc<ChainStat>>) {
    THREAD_STAT.with(|t| *t.borrow_mut() = stat);
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
            stat: THREAD_STAT.with(|t| t.borrow().clone()),
            clock: CpuClock::new(),
        }
    }

    /// Accounts this reader's samples, CPU time and backlog to `stat`.
    #[must_use]
    pub fn with_stat(mut self, stat: Arc<ChainStat>) -> Self {
        self.stat = Some(stat);
        self
    }

    /// Reads the next chunk into `buf` (waits up to 20 ms).
    ///
    /// **A stopped segment reads as closed** (T-542). `shared.stop` is how a segment ends — a
    /// retune's re-plumb, a capture failure, a user stop — and once it is set no further sample of
    /// *this* segment will ever be shown to anyone. A chain that keeps pulling from the ring past
    /// it is doing work for a window that has gone, and it holds up far more than itself: the
    /// segment's `hk-control` worker only returns once every chain it started has finished
    /// ([`ChainManager::reap`]), and the supervisor's re-plumb waits on that worker.
    ///
    /// Measured on the live HackRF under a 1 MHz–6 GHz sweep at dwell 1 s with the canvas at its
    /// finest level: at the step that crosses into `restricted-paging` the re-plumb took **88
    /// seconds**, with ten `hk-chain-fsk-bursts-*` threads still reading and appending, the
    /// supervisor parked in `pthread_join`, no capture at all, and `/ws/spectrum/live` answering
    /// `410`/`404` for 85 s of it. Nothing had panicked and nothing was deadlocked — the segment
    /// simply would not let go.
    ///
    /// Returning `Closed` (rather than adding a flag each chain must remember to test) is the
    /// narrow version of the fix: every chain shape already has a wind-down for the ring ending,
    /// because the ring does end, and this is that same ending arriving a few milliseconds
    /// earlier.
    pub fn next(&mut self) -> Next {
        if self.shared.stop.load(Ordering::SeqCst) {
            return Next::Closed;
        }
        let c = &self.shared.counters.chains;
        match self
            .reader
            .read_timeout(&mut self.buf, Duration::from_millis(20))
        {
            ReadOutcome::Data(chunk) => {
                add(&c.samples, chunk.len as u64);
                if let Some(s) = &self.stat {
                    add(&s.samples, chunk.len as u64);
                    let head = self.shared.ring.next_sample().unwrap_or(0);
                    let behind = head.saturating_sub(chunk.end_sample()) as f64;
                    s.backlog_us.store(
                        (behind / self.shared.fs * 1e6) as u64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    s.account_cpu(&mut self.clock);
                }
                Next::Data(chunk)
            }
            ReadOutcome::Overrun { lost_samples, .. } => {
                add(&c.lost_samples, lost_samples);
                if let Some(s) = &self.stat {
                    add(&s.lost_samples, lost_samples);
                }
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
    /// Set when the chain's thread has finished its body — including its writes — or unwound
    /// (T-878: what a classifying chain waits on, see [`DecodeSlot`]).
    finished: Arc<AtomicBool>,
    /// A [`Trigger::EveryTrack`] measuring chain (T-297, T-878) and which kind, counted apart
    /// from the decode chains; `None` for everything else.
    measuring: Option<Measure>,
}

/// T-878: the decode chain a classifying chain must let finish before it writes, filled by the
/// manager when (if ever) a decode chain attaches to the same track.
///
/// Both write classification rows for the same emission, and an emitter's classification history
/// is in insertion order; left to thread timing, identical runs recorded the chain's label and the
/// classifier's posterior in either order. Before T-878 the one fsk chain wrote label, promotion
/// and posterior in that order on one thread; waiting here keeps that order. `None` (no decode
/// chain matched) means nothing to wait for.
pub(crate) type DecodeSlot = Arc<Mutex<Option<Arc<AtomicBool>>>>;

/// Sets its flag when dropped: at the end of a chain's thread, including an unwinding one, so a
/// chain that panicked never leaves a classifier waiting on it for the whole bound.
struct SetOnDrop(Arc<AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// The kinds of [`Trigger::EveryTrack`] measuring chain. At most one of each runs per track, each
/// kind under its own node spec's `max_chains`, and each is counted apart from the decode chains
/// and from the other kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Measure {
    /// Sweep characterisation ([`sweep`], T-297).
    Sweep,
    /// The C15 classifier ([`classify`], T-878). Unlike the characteriser it needs the track's
    /// member boxes, so it is sent them.
    Classify,
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
        // T-531: the row is written by the detect writer, and on a stop that writer is already
        // finishing — so once the run is stopping the wait can only ever time out, and waiting it
        // out is pure shutdown latency on the thread the control thread is joining. Give up now
        // and count it exactly as the timeout does.
        if std::time::Instant::now() >= deadline || shared.stop.load(Ordering::SeqCst) {
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

/// Which analog chain owns an emission (T-071). T-070 lets a raster chain take an off-raster
/// station beside its channel, so two neighbouring chains can refine to the same station; the
/// emission is demodulated and written by exactly one of them.
///
/// - A chain **claims** the centre it refined to, with a half-width and a rank (its distance from
///   its own channel centre; lower is better) before demodulating the full window.
/// - An overlapping claim held by another chain refuses the new one when that claim is committed
///   or ranks no worse; otherwise the better-ranked new claim **preempts** it.
/// - The chain **commits** just before writing; a preempted chain finds its claim gone and stops.
/// - A released committed claim is kept for [`CHANNEL_COOLDOWN_S`] of stream time, so a chain
///   that finishes later on the same window does not write the station again.
///
/// Per segment ([`Shared::claims`]); only the claim table is shared, never chain state.
/// CRC-valid-or-not decodes written per track (T-127 review): the scheduler credits a bandit
/// dwell with the decodes of the tracks it saw, never the run-wide counter. Keeps the most recent
/// [`TRACK_DECODES_MAX`] tracks; an evicted track reads 0 (its later count restarts, so a dwell
/// holding an older base saturates to 0 rather than over-crediting). Decodes a chain writes for
/// a track merged into another stay on the chain's own track id.
#[derive(Debug, Default)]
pub(crate) struct TrackDecodes {
    inner: Mutex<(HashMap<TrackId, u64>, VecDeque<TrackId>)>,
}

/// Tracks [`TrackDecodes`] remembers.
const TRACK_DECODES_MAX: usize = 4096;

impl TrackDecodes {
    /// Adds `n` decodes written for `track` (none for a track-less chain).
    pub fn add(&self, track: Option<TrackId>, n: u64) {
        let (Some(track), true) = (track, n > 0) else {
            return;
        };
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let (counts, order) = &mut *g;
        match counts.get_mut(&track) {
            Some(c) => *c += n,
            None => {
                if order.len() >= TRACK_DECODES_MAX {
                    if let Some(old) = order.pop_front() {
                        counts.remove(&old);
                    }
                }
                counts.insert(track, n);
                order.push_back(track);
            }
        }
    }

    /// Decodes written for `track` so far.
    pub fn get(&self, track: TrackId) -> u64 {
        let g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        g.0.get(&track).copied().unwrap_or(0)
    }
}

#[derive(Debug, Default)]
pub(crate) struct EmissionClaims {
    claims: Mutex<Vec<Claim>>,
}

#[derive(Debug, Clone)]
struct Claim {
    owner: u64,
    center_hz: f64,
    half_hz: f64,
    rank: f64,
    committed: bool,
    /// Released: kept until this stream sample.
    until: Option<u64>,
}

impl EmissionClaims {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Claim>> {
        self.claims.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Claims the emission at `center_hz` (± `half_hz`) for `owner` ranked `rank`; `false` when
    /// another chain owns it.
    pub fn claim(&self, owner: u64, center_hz: f64, half_hz: f64, rank: f64, now: u64) -> bool {
        let mut claims = self.lock();
        claims.retain(|c| c.until.is_none_or(|u| now < u));
        let overlaps = |c: &Claim| {
            c.owner != owner && (c.center_hz - center_hz).abs() < c.half_hz.max(half_hz)
        };
        if claims
            .iter()
            .any(|c| overlaps(c) && (c.committed || c.until.is_some() || c.rank <= rank))
        {
            return false;
        }
        claims.retain(|c| !overlaps(c) && c.owner != owner);
        claims.push(Claim {
            owner,
            center_hz,
            half_hz,
            rank,
            committed: false,
            until: None,
        });
        true
    }

    /// Commits `owner`'s claim before it writes; `false` when it was preempted.
    pub fn commit(&self, owner: u64) -> bool {
        let mut claims = self.lock();
        match claims
            .iter_mut()
            .find(|c| c.owner == owner && c.until.is_none())
        {
            Some(c) => {
                c.committed = true;
                true
            }
            None => false,
        }
    }

    /// `owner` finished at `now`: a committed claim is kept for `hold` samples, others dropped.
    /// T-403: whether a chain is **measuring** the emission at `center_hz` (± `half_hz`) right
    /// now — it claimed it and has not released it.
    ///
    /// This is the fact the live continuous confirmation route yields to. Route B's evidence
    /// (continuity, duty cycle, width) is real but weaker *in kind* than a demodulator's: it says
    /// "something modulated has been on air steadily", not "this is an FM broadcast station, and
    /// here is its pilot". While a chain holds the emission the stronger answer is being measured,
    /// and a fast route that confirmed anyway would record the weaker reason for a station the
    /// system was about to identify positively — and *which* reason got recorded would depend on
    /// how loaded the host was, because a chain under load writes its session later in the capture.
    ///
    /// A released claim (`until` set) does not count: the chain is finished with it, whatever it
    /// concluded.
    pub fn measuring(&self, center_hz: f64, half_hz: f64) -> bool {
        self.lock()
            .iter()
            .any(|c| c.until.is_none() && (c.center_hz - center_hz).abs() < c.half_hz.max(half_hz))
    }

    pub fn release(&self, owner: u64, now: u64, hold: u64) {
        let mut claims = self.lock();
        claims.retain(|c| c.owner != owner || c.committed);
        for c in claims
            .iter_mut()
            .filter(|c| c.owner == owner && c.until.is_none())
        {
            c.until = Some(now.saturating_add(hold));
        }
    }
}

/// The first sample a chain of `shape` reads for `cand` (claimed at attach).
fn chain_start(shape: &ChainShape, cand: &Candidate, fs: f64) -> u64 {
    let pre_s = match shape {
        ChainShape::Analog { pre_s, .. } => *pre_s,
        ChainShape::Fsk { pad_s, .. } | ChainShape::Classify { pad_s, .. } => *pad_s,
        // The hunt and the characteriser read forward from where they attached: there is no
        // trigger box to precede.
        ChainShape::Plugin { .. } | ChainShape::TrunkCc { .. } | ChainShape::Sweep { .. } => 0.0,
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
    /// T-297: the measuring chains ([`Trigger::EveryTrack`]) running for each track, kept apart
    /// from `by_track` because they attach *beside* the decode chain rather than instead of it.
    measuring: HashMap<TrackId, Vec<(Measure, u64)>>,
    /// T-878: each open track's classifying-chain [`DecodeSlot`], filled when a decode chain
    /// attaches to the track.
    slots: HashMap<TrackId, DecodeSlot>,
}

const BACKLOG_PER_TRACK: usize = 512;

/// Most runtime chains and recorders alive at once, across the whole run (T-558).
///
/// **Why there has to be a number.** A chain is an OS thread plus that chain's retain buffer,
/// and until this cap existed nothing bounded how many of them a run could hold: `attach_track`
/// refused a duplicate *channel* (and only for a spec with a raster — `fsk-bursts` has none) and
/// `attach_measuring` capped its own kind at the node spec's `max_chains`, but the decode chains
/// were limited only by how many tracks got confirmed. A 1 MHz–6 GHz survey confirms a great
/// many: T-542 measured the live process at **~450 threads and multi-GB RSS** under one.
///
/// Measured here (T-558, `tests/sweep_residency.rs`, mock device at 2 Msps): threads rose
/// **one per attach** — 43 → 219 across 175 attaches — and RSS 64 → 1210 MiB, about 6.5 MiB a
/// chain. The retain buffer scales with the sample rate, so at the HackRF's 20 Msps an
/// `fsk-bursts` chain's `2 × retain_s` of `Complex<i8>` is ~240 MB on its own.
///
/// Above the cap an attach is **refused and counted** (`chains.admission_refused`), never
/// queued: the candidate that could not be decoded now is still in the inventory, and a survey
/// that keeps moving is worth more than one that stalls holding every region it ever saw. This
/// counts recorders too, because a recorder is also a thread this run has to carry.
pub(crate) const MAX_RUNTIME_CHAINS: usize = 16;

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
            measuring: HashMap::new(),
            slots: HashMap::new(),
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
        self.attach_with(spec, cand, None)
    }

    /// [`Self::attach`], handing a classifying chain the [`DecodeSlot`] it waits on (a fresh,
    /// empty one when `slot` is `None`; other shapes ignore it).
    fn attach_with(
        &mut self,
        spec: &ChainSpec,
        cand: Candidate,
        slot: Option<DecodeSlot>,
    ) -> Option<u64> {
        // T-558: the run-wide bound. Reap first so the cap counts chains that are still running
        // rather than slots a finished thread has not been joined out of yet (`is_finished` is an
        // atomic load, so asking is cheap enough to ask on every attach).
        self.reap();
        let c = &self.shared.counters.chains;
        if self.running.len() >= MAX_RUNTIME_CHAINS {
            inc(&c.admission_refused);
            return None;
        }
        let shape = match spec.shape() {
            Ok(s) => s,
            Err(e) => {
                inc(&c.attach_errors);
                eprintln!("hk-pipeline: chain {}: {e}", spec.id);
                return None;
            }
        };
        let class = self.shared.cfg.source_class;
        // T-297: a measuring chain runs beside the decode chain rather than instead of it, and is
        // counted apart from it (see `Running::measuring`).
        let measuring = match (&shape, spec.trigger) {
            (ChainShape::Classify { .. }, Trigger::EveryTrack) => Some(Measure::Classify),
            (_, Trigger::EveryTrack) => Some(Measure::Sweep),
            _ => None,
        };
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
                    self.running.push(Running {
                        id,
                        tx: None,
                        join,
                        // A recorder writes no classification: nothing waits on it.
                        finished: Arc::new(AtomicBool::new(false)),
                        measuring: None,
                    });
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
        // T-071: the chain's own counters, registered while its thread runs.
        let stat = self.shared.counters.chain_stats.register(&spec.id);
        stat.set_channel(
            0.5 * (cand.f_lo_hz + cand.f_hi_hz),
            cand.f_hi_hz - cand.f_lo_hz,
        );
        let cursor = self
            .shared
            .gate
            .register(chain_start(&shape, &cand, self.shared.fs));
        // On a raster the receiver must lock inside this channel, not a neighbour's.
        let channel_tolerance_hz = spec.raster_hz.map_or(f64::INFINITY, |r| 0.5 * r);
        // T-287: the hunt's grid. `validate` refuses an occupancy spec without one.
        let raster_hz = spec.raster_hz.unwrap_or(0.0);
        let finished = Arc::new(AtomicBool::new(false));
        let done = SetOnDrop(Arc::clone(&finished));
        let slot = slot.unwrap_or_default();
        let spawned = thread::Builder::new()
            .name(format!("hk-chain-{}-{id}", spec.id))
            .spawn(move || {
                let _done = done;
                let mut clock = CpuClock::new();
                stat.account_cpu(&mut clock);
                set_thread_stat(Some(stat.stat()));
                match shape {
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
                            owner: id,
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
                    ChainShape::TrunkCc {
                        window_s,
                        max_channels,
                        max_demods,
                        period_s,
                        max_follows,
                    } => trunk::run(
                        shared,
                        rx,
                        cand,
                        trunk::TrunkCcNode {
                            window_s,
                            max_channels,
                            max_demods,
                            period_s,
                            max_follows,
                            raster_hz,
                        },
                        cursor,
                    ),
                    ChainShape::Classify {
                        pad_s,
                        retain_s,
                        window_s,
                        ..
                    } => classify::run(
                        shared,
                        rx,
                        cand,
                        classify::ClassifyNode {
                            pad_s,
                            retain_s,
                            window_s,
                        },
                        cursor,
                        slot,
                    ),
                    ChainShape::Sweep {
                        window_s,
                        frame_s,
                        max_passes,
                        ..
                    } => sweep::run(
                        shared,
                        rx,
                        cand,
                        sweep::SweepNode {
                            window_s,
                            frame_s,
                            max_passes,
                        },
                        cursor,
                    ),
                }
                stat.account_cpu(&mut clock);
                set_thread_stat(None);
            });
        match spawned {
            Ok(join) => {
                inc(match measuring {
                    Some(Measure::Sweep) => &c.sweep_attached,
                    Some(Measure::Classify) => &c.classify_attached,
                    None => &c.attached,
                });
                self.running.push(Running {
                    id,
                    tx: Some(tx),
                    join,
                    finished,
                    measuring,
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
        // Measuring chains first, and unconditionally: they must not depend on whether a decode
        // spec matched, because the region that most needs measuring is the one none claimed.
        self.attach_measuring(track);
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

    /// Measuring chains of `kind` alive now, across every track.
    fn live_measuring(&self, kind: Measure) -> usize {
        self.running
            .iter()
            .filter(|r| r.measuring == Some(kind))
            .count()
    }

    /// Attaches every [`Trigger::EveryTrack`] spec matching this track, **in addition to** the
    /// decode chain [`select_for_track`] chooses (T-297, T-878).
    ///
    /// At most one measuring chain **of each kind** per track, and at most the node spec's
    /// `max_chains` of that kind alive at once across the run — the bound that matters, since the
    /// trigger is per confirmed track and a busy band has many. Above the cap the attach is refused
    /// and counted, never queued. A classifying chain is handed the track's member boxes so far
    /// (the backlog stays for the decode chain).
    fn attach_measuring(&mut self, track: TrackId) {
        let Some(cand) = self.pending.get(&track).cloned() else {
            return;
        };
        // A confirmed track has at least one detection by definition — the confirming one — which
        // `members` has not necessarily counted yet.
        let count = self.members.get(&track).copied().unwrap_or(0).max(1);
        let specs: Vec<ChainSpec> = self
            .shared
            .specs
            .iter()
            .filter(|s| {
                s.trigger == Trigger::EveryTrack
                    && count >= s.min_detections
                    && s.matches(cand.f_lo_hz, cand.f_hi_hz, cand.bursty)
            })
            .cloned()
            .collect();
        for spec in specs {
            let (kind, cap) = match spec.shape() {
                Ok(ChainShape::Sweep { max_chains, .. }) => (Measure::Sweep, max_chains),
                Ok(ChainShape::Classify { max_chains, .. }) => (Measure::Classify, max_chains),
                _ => continue,
            };
            // One measuring chain of a kind per track: a second would measure the same region
            // twice.
            if self
                .measuring
                .get(&track)
                .is_some_and(|v| v.iter().any(|(k, _)| *k == kind))
            {
                continue;
            }
            if self.live_measuring(kind) >= cap {
                let c = &self.shared.counters.chains;
                inc(match kind {
                    Measure::Sweep => &c.sweep_admission_refused,
                    Measure::Classify => &c.classify_admission_refused,
                });
                continue;
            }
            let slot = (kind == Measure::Classify).then(|| {
                let slot = DecodeSlot::default();
                // Normally the decode chain attaches after this one (measuring chains attach
                // first), but a slot is filled from whatever is already there.
                *slot.lock().unwrap_or_else(PoisonError::into_inner) = self.decode_finished(track);
                slot
            });
            if let Some(id) = self.attach_with(&spec, cand.clone(), slot.clone()) {
                self.measuring.entry(track).or_default().push((kind, id));
                if let Some(slot) = slot {
                    self.slots.insert(track, slot);
                    for m in self.backlog.get(&track).cloned().unwrap_or_default() {
                        self.send(id, ChainMsg::Member(m));
                    }
                }
            }
        }
    }

    /// The finished flag of `track`'s decode chain, if one is attached.
    fn decode_finished(&self, track: TrackId) -> Option<Arc<AtomicBool>> {
        let id = self.by_track.get(&track)?;
        self.running
            .iter()
            .find(|r| r.id == *id)
            .map(|r| Arc::clone(&r.finished))
    }

    /// Hands `track`'s classifying chain (if any) its decode chain's finished flag.
    fn fill_slot(&self, track: TrackId) {
        if let (Some(slot), Some(flag)) = (self.slots.get(&track), self.decode_finished(track)) {
            *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(flag);
        }
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
            self.fill_slot(track);
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
        // T-878: the classifier chooses its box from the track's members, whatever decode chain
        // (if any) receives them below.
        for &(kind, id) in self.measuring.get(&track).into_iter().flatten() {
            if kind == Measure::Classify {
                self.send(id, ChainMsg::Member(member.clone()));
            }
        }
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
        // The measuring chain writes what it measured at detach, against a settled inventory.
        for (_, id) in self.measuring.remove(&track).unwrap_or_default() {
            self.send(id, ChainMsg::Detach);
        }
        self.slots.remove(&track);
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
        // The survivor keeps its own measuring chain; the absorbed track's finishes and writes
        // whatever it had already measured (the repository re-points a merged emitter).
        for (_, id) in self.measuring.remove(&from).unwrap_or_default() {
            self.send(id, ChainMsg::Detach);
        }
        self.slots.remove(&from);
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
                // The survivor's classifier now waits on the decode chain it inherited.
                self.fill_slot(into);
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

    /// Attaches tune-driven chains — [`Trigger::Coverage`] whose band the window covers, and
    /// [`Trigger::Occupancy`] whose band it overlaps — and detaches those it no longer does.
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
            .filter(|s| matches!(s.trigger, Trigger::Coverage | Trigger::Occupancy))
            .cloned()
            .collect();
        for spec in specs {
            // A coverage chain wants its band *inside* the window (it decodes a known channel); an
            // occupancy chain wants only an *overlap*, because its band is a prior about where to
            // hunt and is wider than any window. The band it is handed is therefore what the radio
            // can actually see, not the allocation — the hunt sweeps the window's own raster and is
            // never told a frequency.
            let (active, band) = match spec.trigger {
                Trigger::Occupancy => {
                    let usable = 0.8 * rate;
                    (
                        spec.overlaps_window(center, usable),
                        [center - usable / 2.0, center + usable / 2.0],
                    )
                }
                _ => (
                    spec.covered_by(center, rate),
                    spec.freq_hz.first().copied().unwrap_or([center, center]),
                ),
            };
            match (active, self.coverage.get(&spec.id).copied()) {
                (true, None) => {
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
        self.measuring.clear();
        self.slots.clear();
    }

    /// Joins finished chains.
    pub fn reap(&mut self) {
        let mut i = 0;
        while i < self.running.len() {
            if self.running[i].join.is_finished() {
                let r = self.running.swap_remove(i);
                let _ = r.join.join();
                if r.tx.is_some() {
                    let c = &self.shared.counters.chains;
                    inc(match r.measuring {
                        Some(Measure::Sweep) => &c.sweep_detached,
                        Some(Measure::Classify) => &c.classify_detached,
                        None => &c.detached,
                    });
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
    fn exactly_one_chain_owns_an_emission_and_the_nearer_channel_preempts() {
        let c = EmissionClaims::default();
        let station = 101.4495e6;
        // The beside chain (101.3 MHz channel, 149.5 kHz away) claims first.
        assert!(c.claim(1, station, 100e3, 149.5e3, 0));
        // The in-channel chain (101.5 MHz, 50.5 kHz away) preempts it before it wrote.
        assert!(c.claim(2, station + 300.0, 100e3, 50.5e3, 10));
        assert!(!c.commit(1), "preempted");
        assert!(c.commit(2));
        // A third claimant, even a nearer one, is refused once the owner committed.
        assert!(!c.claim(3, station, 100e3, 0.0, 20));
        // Other stations are unaffected.
        assert!(c.claim(4, 101.7e6, 100e3, 0.0, 20));
        // Released: held for the cooldown, then free.
        c.release(2, 1_000, 500);
        c.release(1, 1_000, 500);
        assert!(!c.claim(5, station, 100e3, 0.0, 1_499));
        assert!(c.claim(5, station, 100e3, 0.0, 1_500));
        // A worse-ranked late claimant is refused by an uncommitted better claim.
        assert!(!c.claim(6, station, 100e3, 1.0, 1_600));
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
        let trunk = ChainShape::TrunkCc {
            window_s: 0.5,
            max_channels: 64,
            max_demods: 8,
            period_s: 10.0,
            max_follows: 8,
        };
        assert_eq!(chain_start(&analog, &cand, fs), 500_000);
        assert_eq!(chain_start(&fsk, &cand, fs), 980_000);
        assert_eq!(chain_start(&plugin, &cand, fs), 1_000_000);
        assert_eq!(chain_start(&trunk, &cand, fs), 1_000_000);
    }
}
