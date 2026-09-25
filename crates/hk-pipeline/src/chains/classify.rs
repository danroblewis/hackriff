//! The C15 classifier as a measuring chain on a running pipeline (T-878).
//!
//! # Why this exists
//!
//! Until T-878 the classifier's only caller was the fsk chain, after it had demodulated at least
//! `min_bursts` bursts and written a framed result. Classification therefore depended on a decode
//! chain **matching** the track, **attaching** to it and **succeeding** on it — and measured
//! through the mock SDR, most scenes met none of the three: POCSAG and ACARS tracks closed
//! `unmatched`, a WFM station got only the FM chain (which never classifies), a LoRa burst got the
//! fsk chain but too few FSK bursts for it to reach the classifier, and a multipath scene was
//! classified three times and persisted nothing (the rank-3 tie, now resolved in
//! [`crate::classify::record`]). The emissions left unclassified were precisely the unknown ones
//! the classifier is for.
//!
//! ADR-0016 §4 ("Placement") puts classification per event, on the CPU, off the ring and DSP
//! threads, where the pipeline already works over an emission. This chain is that place for every
//! confirmed track: it attaches on [`Trigger::EveryTrack`](super::spec::Trigger::EveryTrack),
//! **beside** whatever decode chain was selected and whether or not one was, exactly as the sweep
//! characteriser ([`super::sweep`]) does. It demodulates nothing and decides nothing about which
//! decoder runs; it reads the region's own IQ and asks [`crate::classify`] about it.
//!
//! # What it classifies
//!
//! The track's member boxes, grouped as the fsk chain groups them (boxes overlapping in time are one
//! burst). The **longest** group is the most evidence the track offers of its emission (ties keep
//! the earlier, so the choice is deterministic and blind), capped at the node's `window_s`:
//!
//! - a **burst** shorter than the window is classified whole, once, when the track ends — the
//!   choice the fsk chain made, so an FSK scene is classified from the same burst as before;
//! - a **continuous** emission (a WFM station, a carrier) is classified over its first `window_s`
//!   as soon as that much of it has arrived, not when its track eventually idles out, and the row is
//!   written the moment the inventory has an entry for the track.
//!
//! # And whether it is DMR (T-989)
//!
//! Over the same box, the chain also asks [`crate::dmr`] whether the region is **conventional DMR
//! (Tier II)** — a question the trunking hunt cannot reach, because a conventional repeater has no
//! control channel. It is deliberately **not** gated on the classification: the emissions it
//! exists for are the ones the classifier abstained on (an explorer window's DMR repeater arrived
//! with `classification: null`), so a region with no classification is still asked, and a region
//! with one gets both rows. What gates it is the region's own measured bandwidth, because a DMR
//! channel is 12.5 kHz wide; everything past that gate has to produce DMR sync words at a ~1e-11
//! false-alarm rate per position before anything is written.
//!
//! # Which emitter the row is written against
//!
//! The one the inventory recorded **for this track**
//! ([`crate::Inventory::recorded_emitter_of_track`]) — never an emitter found by looking a
//! frequency up. A track's closing sighting races this chain's detach, so the lookup is retried
//! for a bounded [`EMITTER_WAIT`]; with no entry the classification is dropped and counted
//! (`classify_no_emitter`), never attached to a guess.
//!
//! # Order of writes, and why it is fixed
//!
//! The track's decode chain (if one attached) writes its own label for the same emission, and an
//! emitter's classification history is in insertion order. Before T-878 the fsk chain wrote label,
//! lock promotion and posterior in that order on one thread; with two threads, identical runs
//! recorded them in either order (measured: `m3_scene`'s two identical runs disagreed). So at the
//! end of a track this chain waits — bounded by [`DECODE_WAIT`], and not at all once the run is
//! stopping — for the decode chain's thread to finish before writing
//! ([`DecodeSlot`](super::DecodeSlot)). A continuous emission written early (above) does not wait:
//! its decode chain may run for as long as the track does.
//!
//! # Cost
//!
//! One chain thread per classified track, at most `max_chains` alive at once (above that the
//! attach is refused and counted, `classify_admission_refused`). They are **not** counted against
//! the run-wide [`MAX_RUNTIME_CHAINS`](super::MAX_RUNTIME_CHAINS): a track's classifying chain
//! attaches before its decode chain, and must never take the slot the decode chain needs. A track
//! refused at the cap is **remembered and retried** when a slot frees (T-886,
//! [`super::Chains::attach_measuring`]), so the cap bounds concurrency and never decides which
//! regions are classified. Memory is the rolling buffer — [`retain_samples`], what the classifier
//! reads plus the measured member-box arrival lag, held exactly and not at twice it (T-886) —
//! plus one owned copy of the chosen box, which is at most `window_s` plus its pads. Once a box spans the whole window
//! nothing later can beat it, so the chain stops reading and releases its cursor before it
//! classifies, which keeps the lossless flow gate moving.
//! Per classification the cost is [`crate::classify::classify_and_record`]'s documented bound.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use hk_classify::{Classifier, SymbolEstimator};
use hk_core::{Discontinuity, ProvenanceHandle};
use hk_dsp::InputInfo;
use hk_estimate::SnippetRequest;
use hk_model::classify::Classification;
use hk_model::{DetectionId, EmitterId, SampleTime, Timestamp, TrackId};
use num_complex::Complex;

use super::{ChainMsg, ChainReader, Next};
use crate::events::{Candidate, MemberBox};
use crate::gate::GateCursor;
use crate::run::Shared;
use crate::stats::{add, inc};

/// Longest wait for the track's inventory emitter once there is a classification to write.
///
/// A track's entry is written by the detection writer when the track closes, which races this
/// chain's detach. Bounded so a run can never hang on it, and paid only when there is a row to
/// write. The same bound [`super::sweep`] uses for the same race.
const EMITTER_WAIT: Duration = Duration::from_secs(2);

/// Longest wait for the track's decode chain to finish writing before this chain writes (see
/// [`super::DecodeSlot`]). The run already waits for every chain before it ends, so this adds no
/// latency to a run whose chains finish; it only bounds a classifier behind a stuck one. A plugin
/// chain may take its `settle_s` (up to 30 s in a lossless replay), hence the margin.
const DECODE_WAIT: Duration = Duration::from_secs(60);

/// How often a classified-but-unwritten continuous emission asks the inventory for its entry.
const EMITTER_POLL: Duration = Duration::from_millis(100);

/// Most pending member groups one chain queues, as the fsk chain bounds its own (T-558).
const MAX_PENDING_GROUPS: usize = 4096;

/// Longest **member-box arrival lag** the rolling buffer is sized to cover (T-886).
///
/// The chain cannot classify a group whose samples it no longer holds, and a member box arrives
/// well after the samples it describes: the detector emits a box when the burst ends or its
/// continuing box reaches its period. So the buffer must span what the classifier *reads*
/// (`window_s` plus its two pads) **plus** that lag — and nothing beyond it, which is memory paid
/// for samples no group will ever ask for again.
///
/// **Measured, not assumed** (T-453): instrumented over the five scenes of
/// `device_classify_every_track` at 2.4 Msps, the gap between a group's end and the newest
/// buffered sample at the moment the group was processed was, over 84 groups, p50 **0.70 s**,
/// p90 **1.40 s**, max **1.59 s**. Two seconds is that worst case with ~25 % margin.
const MEMBER_LAG_S: f64 = 2.0;

/// Samples the rolling buffer holds: the node's `retain_s`, **capped at what the chain can still
/// use** ([`MEMBER_LAG_S`] plus the analysed extent and its pads), then at the fsk chain's sample
/// ceiling whatever the rate.
///
/// T-886: the built-in spec asks for 3.0 s, of which 0.29 s is read and at most ~1.6 s more was
/// ever needed to still be holding a group when it arrived. Sizing from what is read halves the
/// buffer at the rates a replay runs at; at 20 Msps the sample ceiling binds first, and it is the
/// exact-capacity buffer above that halves the residency there.
pub(crate) fn retain_samples(node: &ClassifyNode, fs: f64) -> usize {
    let usable = node.window_s + 2.0 * node.pad_s + MEMBER_LAG_S;
    ((node.retain_s.min(usable) * fs) as usize).clamp(1, super::fsk::MAX_RETAIN_SAMPLES)
}

/// The chain's rolling window of one contiguous span of samples, holding **exactly**
/// [`retain_samples`] of it (T-886).
///
/// The buffer this replaced was a `Vec` trimmed back to `retain` only once it had grown to *twice*
/// it, so its residency peaked at `2 x retain` — 64 MB per chain at 20 Msps, 256 MB for the
/// spec's four. Trimming a `Vec` to a fixed length on every append instead would copy the whole
/// buffer per chunk, so the doubling was the price of the amortisation; a `VecDeque` drops from
/// the front in `O(dropped)` and pays neither. Capacity grows geometrically (a chain on a
/// millisecond burst never allocates the whole window) but is **never allowed past `retain`**, so
/// the peak is the bound and not twice it.
struct Rolling {
    iq: VecDeque<Complex<i8>>,
    /// Samples held at most.
    retain: usize,
    /// Sample index of `iq[0]`.
    base: u64,
    /// Capture time of `iq[0]`.
    base_time: Timestamp,
}

impl Rolling {
    fn new(retain: usize) -> Self {
        Self {
            iq: VecDeque::new(),
            retain,
            base: 0,
            base_time: Timestamp::UNIX_EPOCH,
        }
    }

    fn is_empty(&self) -> bool {
        self.iq.is_empty()
    }

    fn clear(&mut self) {
        self.iq.clear();
    }

    /// Sample index one past the newest sample held.
    fn end(&self) -> u64 {
        self.base + self.iq.len() as u64
    }

    /// Bytes of sample buffer this chain holds — its residency, as a test measures it.
    #[cfg_attr(not(test), allow(dead_code))]
    fn bytes(&self) -> usize {
        self.iq.capacity() * std::mem::size_of::<Complex<i8>>()
    }

    /// Starts a new span at `first` (a discontinuity, a new provenance, or the first chunk).
    fn restart(&mut self, first: u64, time: Timestamp) {
        self.iq.clear();
        self.base = first;
        self.base_time = time;
    }

    /// Appends `samples`, dropping whatever the retention no longer covers.
    fn extend(&mut self, samples: &[Complex<i8>], fs: f64) {
        let cut = (self.iq.len() + samples.len())
            .saturating_sub(self.retain)
            .min(self.iq.len());
        if cut > 0 {
            self.iq.drain(..cut);
            self.base += cut as u64;
            self.base_time = self
                .base_time
                .saturating_add_nanos((cut as f64 * 1e9 / fs) as i64);
        }
        let need = self.iq.len() + samples.len();
        if need > self.iq.capacity() {
            // Geometric, but never past the retention: the peak is `retain`, not twice it.
            let want = need.max(self.iq.capacity() * 2).min(self.retain.max(need));
            self.iq.reserve_exact(want - self.iq.len());
        }
        self.iq.extend(samples.iter().copied());
    }

    /// Capture time of sample index `i`, which must be inside the span.
    fn time_at(&self, i: u64, fs: f64) -> Timestamp {
        self.base_time
            .saturating_add_nanos(((i - self.base) as f64 * 1e9 / fs) as i64)
    }

    /// An owned copy of `[s, e)`, which must be inside the span.
    fn copy(&self, s: u64, e: u64) -> Vec<Complex<i8>> {
        self.iq
            .range((s - self.base) as usize..(e - self.base) as usize)
            .copied()
            .collect()
    }
}

/// What one classification may spend. Built from
/// [`ChainShape::Classify`](super::spec::ChainShape::Classify); the module docs state each bound.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ClassifyNode {
    /// Pad either side of the analysed box, s.
    pub pad_s: f64,
    /// Rolling sample buffer, s.
    pub retain_s: f64,
    /// Longest extent analysed, s.
    pub window_s: f64,
}

#[derive(Clone, Debug)]
struct Group {
    start: u64,
    end: u64,
    f_lo: f64,
    f_hi: f64,
    /// The CFAR detection of the group's first box, the subject the C38 gate admits (T-844).
    detection: DetectionId,
}

/// The box chosen so far: an owned copy of its padded samples and what classifying it needs.
struct Chosen {
    iq: Vec<Complex<i8>>,
    time: SampleTime,
    prov: ProvenanceHandle,
    request: SnippetRequest,
    /// Analysed extent, samples.
    span: u64,
    /// The group's CFAR detection (T-844: the shadow record's subject).
    detection: DetectionId,
}

/// What the chain publishes: the classification, and (T-844) the learned stage's input of the same
/// snippet when a model in a non-`off` mode wanted it ([`crate::classify::classify_box_observed`]).
struct Classified {
    classification: Classification,
    input: Option<Vec<f32>>,
    detection: DetectionId,
}

fn add_member(groups: &mut Vec<Group>, m: &MemberBox, missed: &std::sync::atomic::AtomicU64) {
    let (s, e) = (m.samples.start, m.samples.end);
    if let Some(g) = groups.iter_mut().find(|g| s < g.end && e > g.start) {
        g.start = g.start.min(s);
        g.end = g.end.max(e);
        g.f_lo = g.f_lo.min(m.f_lo_hz);
        g.f_hi = g.f_hi.max(m.f_hi_hz);
        return;
    }
    groups.push(Group {
        start: s,
        end: e,
        f_lo: m.f_lo_hz,
        f_hi: m.f_hi_hz,
        detection: m.detection,
    });
    groups.sort_by_key(|g| g.start);
    if groups.len() > MAX_PENDING_GROUPS {
        let over = groups.len() - MAX_PENDING_GROUPS;
        groups.drain(..over);
        add(missed, over as u64);
    }
}

/// Runs the classifier for `cand`'s track until it has written, the stream ends or it is detached.
pub(crate) fn run(
    shared: Arc<Shared>,
    rx: Receiver<ChainMsg>,
    cand: Candidate,
    node: ClassifyNode,
    cursor: GateCursor,
    decode: super::DecodeSlot,
) {
    let fs = shared.fs;
    let c = &shared.counters.chains;
    let Some(track) = cand.track else {
        // Only a track has an inventory entry to be evidence about.
        return;
    };
    let pad = (node.pad_s * fs) as u64;
    let window = ((node.window_s * fs) as u64).max(1);
    let retain = retain_samples(&node, fs);
    let mut cr = ChainReader::new(
        Arc::clone(&shared),
        cand.first_sample.saturating_sub(pad),
        cursor,
    );
    let mut buf = Rolling::new(retain);
    let mut prov: Option<ProvenanceHandle> = None;
    let mut groups: Vec<Group> = Vec::new();
    let mut chosen: Option<Chosen> = None;
    let (mut detach, mut closed) = (false, false);
    loop {
        let full = chosen.as_ref().is_some_and(|b| b.span >= window);
        loop {
            match rx.try_recv() {
                Ok(ChainMsg::Member(m)) if !full => add_member(&mut groups, &m, &c.classify_missed),
                Ok(ChainMsg::Member(_)) => {}
                Ok(ChainMsg::Detach) => detach = true,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    detach = true;
                    break;
                }
            }
        }
        if !closed {
            match cr.next() {
                Next::Data(ch) => {
                    if full {
                        // Nothing later can beat a box that spans the whole window: keep reading
                        // and releasing so the lossless flow gate never waits on this cursor.
                        buf.clear();
                    } else {
                        let contiguous = !buf.is_empty()
                            && ch.first_sample() == buf.end()
                            && prov.as_ref().is_some_and(|p| p.id() == ch.provenance.id());
                        if !contiguous {
                            buf.restart(ch.first_sample(), ch.time.host_time);
                            prov = Some(ch.provenance.clone());
                        }
                        buf.extend(&cr.buf[..ch.len], fs);
                    }
                    cr.release_to(ch.end_sample());
                }
                Next::Lost => buf.clear(),
                Next::Idle => {}
                Next::Closed => closed = true,
            }
        } else if !detach {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(ChainMsg::Member(m)) if !full => add_member(&mut groups, &m, &c.classify_missed),
                Ok(ChainMsg::Member(_)) => {}
                Ok(ChainMsg::Detach) | Err(RecvTimeoutError::Disconnected) => detach = true,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }

        // Take every group whose samples have arrived. The newest group may still grow, so it is
        // taken early only once it already spans the whole window.
        let buf_end = buf.end();
        let mut done = 0;
        if !full {
            let n = groups.len();
            for (i, g) in groups.iter().enumerate() {
                // Until the chain is detached the newest group may still gain boxes — the
                // other tone lobe of a 2-FSK burst, say — **even after the ring has closed**:
                // member boxes trail the samples, and a group finalised on `closed` alone was
                // split into its lobes by arrival timing, so identical runs chose different boxes.
                // Only a later group (which the detector cannot emit before this one's boxes) or
                // the detach settles it, exactly as the fsk chain settles a burst.
                let growing = i + 1 == n && !detach;
                if growing && g.end - g.start < window {
                    break;
                }
                // The analysed box starts at the group's own start — or, when the buffer has
                // already dropped that, at the oldest sample it still holds behind a pad. The
                // buffer is capped in samples, so at a high rate it holds less than one of the
                // detector's continuing boxes (1 s): without this a continuous emission's box
                // would always have aged out by the time it arrived, and it would never be
                // classified. A burst is analysed from its own start whenever it can be.
                let start = if buf.is_empty() {
                    g.start
                } else {
                    g.start.max(buf.base + pad)
                };
                let end = g.end.min(start + window);
                if end + pad > buf_end && !closed {
                    break;
                }
                done += 1;
                let s = start.saturating_sub(pad);
                let e = (end + pad).min(buf_end);
                let Some(p) = prov.as_ref() else {
                    inc(&c.classify_missed);
                    continue;
                };
                if end <= start || s < buf.base || e <= end || buf.is_empty() {
                    inc(&c.classify_missed);
                    continue;
                }
                let span = end - start;
                if chosen.as_ref().is_some_and(|b| b.span >= span) {
                    continue;
                }
                chosen = Some(Chosen {
                    iq: buf.copy(s, e),
                    time: SampleTime {
                        sample_index: s,
                        host_time: buf.time_at(s, fs),
                    },
                    prov: p.clone(),
                    request: SnippetRequest {
                        start_index: start,
                        end_index: end,
                        center_offset_hz: 0.5 * (g.f_lo + g.f_hi) - p.tune.center_hz,
                        bandwidth_hz: (g.f_hi - g.f_lo).max(1.0),
                    },
                    span,
                    detection: g.detection,
                });
            }
            groups.drain(..done);
        }
        // A continuous emission: nothing later can beat a box that spans the whole window, so
        // stop reading here — dropping the cursor below releases the lossless flow gate before
        // the classification and the repository write, never after them.
        if chosen.as_ref().is_some_and(|b| b.span >= window) {
            break;
        }

        if detach && (groups.is_empty() || full) {
            break;
        }
        if detach && closed && done == 0 {
            // Samples for the remaining boxes will never arrive.
            add(&c.classify_missed, groups.len() as u64);
            break;
        }
    }
    drop(cr);
    drop((buf, groups));
    let continuous = chosen.as_ref().is_some_and(|b| b.span >= window);

    // Abstained upstream (counted in `classify`), or no box of this track ever reached the
    // chain's buffer: nothing was measured.
    let classification = chosen.as_ref().and_then(|b| classify(&shared, b));
    // T-989: and, on the same box, whether this is conventional DMR. Deliberately **not** gated
    // on the classification: the emissions this exists for are the ones the classifier abstained
    // on (an explorer window's DMR repeater arrived with `classification: null`), so a region
    // that could not be classified is still asked.
    let result = Measured {
        dmr: chosen.as_ref().and_then(|b| dmr_scan(&shared, b)),
        region: chosen.as_ref().map(|b| Region {
            center_hz: b.prov.tune.center_hz + b.request.center_offset_hz,
            bandwidth_hz: b.request.bandwidth_hz,
            t: b.time.host_time,
        }),
        classification,
    };
    drop(chosen);
    if result.is_empty() {
        return;
    }
    // A continuous emission is written as soon as its track has an entry, not when the track
    // eventually ends: its decode chain may run for as long as the track does.
    if continuous && !detach {
        loop {
            if let Some(emitter) = emitter_of(&shared, track) {
                publish(&shared, emitter, &result);
                return;
            }
            if shared.stop.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            match rx.recv_timeout(EMITTER_POLL) {
                Ok(ChainMsg::Member(_)) | Err(RecvTimeoutError::Timeout) => {}
                Ok(ChainMsg::Detach) | Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }
    // The track's decode chain writes its label first, as the fsk chain did when it was also the
    // classifier's caller: an emitter's history is in insertion order, and thread timing must not
    // decide it (see `super::DecodeSlot`).
    let deadline = Instant::now() + DECODE_WAIT;
    loop {
        let flag = decode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(flag) = flag else { break };
        if flag.load(std::sync::atomic::Ordering::SeqCst)
            || shared.stop.load(std::sync::atomic::Ordering::SeqCst)
            || Instant::now() >= deadline
        {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let deadline = Instant::now() + EMITTER_WAIT;
    loop {
        // T-531: same rule as `stored_detection`: once the run is stopping, the entry this waits
        // for is written by a thread that is itself stopping, and waiting is shutdown latency.
        let stopping = shared.stop.load(std::sync::atomic::Ordering::SeqCst);
        if let Some(emitter) = emitter_of(&shared, track) {
            publish(&shared, emitter, &result);
            return;
        }
        if stopping || Instant::now() >= deadline {
            inc(&c.classify_no_emitter);
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Classifies the chosen box ([`crate::classify::classify_box`]). `None` when the cascade abstained
/// upstream (the box could not be extracted or normalised) — counted, and nothing is written.
fn classify(shared: &Shared, b: &Chosen) -> Option<Classified> {
    let info = InputInfo {
        time: b.time,
        discontinuity: Discontinuity::NONE,
        dropped_before: 0,
        provenance: &b.prov,
    };
    let mut c14 = SymbolEstimator::new();
    let out = crate::classify::classify_box_observed(
        &Classifier::new(),
        &mut c14,
        // T-399: the receiver-line survey measured by the `hk-survey` reader, once for this
        // capture state. Read here, never measured here.
        shared.receiver.as_ref(),
        info,
        &b.iq,
        &b.request,
        b.time.host_time,
        active_ml(shared),
    );
    if out.is_none() {
        inc(&shared.counters.chains.classify_abstained);
    }
    if crate::debug_enabled() {
        eprintln!(
            "hk-pipeline: classify {:.4} MHz ({:.1} kHz, {} samples): {}",
            (b.prov.tune.center_hz + b.request.center_offset_hz) / 1e6,
            b.request.bandwidth_hz / 1e3,
            b.span,
            out.as_ref().map_or("abstained".into(), |(c, _)| format!(
                "{} open-set {:.3} reasons {:?}",
                c.family, c.open_set_score, c.reasons
            ))
        );
    }
    out.map(|(classification, input)| Classified {
        classification,
        input,
        detection: b.detection,
    })
}

/// The run's C38 stage when an operator put some model in a non-`off` mode (T-844). `None` for an
/// idle stage, so the learned input is never computed and no detection is waited for.
fn active_ml(shared: &Shared) -> Option<&crate::ml::MlStage> {
    shared.ml.as_deref().filter(|m| !m.is_idle())
}

/// Writes the classification ([`write`]), then — only after the repository lock is released, since
/// the host batches and a batch must never stall other writers — offers it to the C38 shadow stage
/// (T-844). The published row is final before the stage sees it: the stage borrows it and has no
/// path that writes a `Classification` (`crate::ml`).
fn publish(shared: &Shared, emitter: EmitterId, result: &Classified) {
    write(shared, emitter, &result.classification);
    let Some(ml) = active_ml(shared) else { return };
    if !ml.wants(&result.classification.family) {
        return;
    }
    // The detection row is written by the detect writer, which may trail this chain; wait for it
    // as the decode chains wait for their parent (`super::stored_detection`), but only here, where
    // a model will run.
    let detection = super::stored_detection(shared, Some(result.detection))
        .and_then(|d| shared.repo().detection(d).ok());
    ml.observe(
        detection.as_ref(),
        &result.classification,
        result.input.as_deref(),
    );
}

/// The entry the inventory recorded for `track`, if any. Takes the inventory lock alone.
fn emitter_of(shared: &Shared, track: TrackId) -> Option<EmitterId> {
    shared
        .inventory
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .recorded_emitter_of_track(track)
}

/// What the chain measured about one region: the C15 classification, and whether the region is
/// conventional DMR (T-989). Either may be absent, and a region with neither is never written.
struct Measured {
    classification: Option<Classification>,
    dmr: Option<hk_detect::dmr_tier2::Tier2Scan>,
    region: Option<Region>,
}

/// Where the measured box sat, for the DMR row and its message stream.
#[derive(Clone, Copy)]
struct Region {
    center_hz: f64,
    bandwidth_hz: f64,
    t: hk_model::Timestamp,
}

impl Measured {
    /// Nothing was measured, so there is nothing to wait for an emitter for.
    fn is_empty(&self) -> bool {
        self.classification.is_none() && self.dmr.is_none()
    }
}

/// Asks [`crate::dmr`] whether the chosen box is conventional DMR, counting what it spent.
///
/// The bandwidth gate is checked here rather than inside so that `dmr_scanned` counts the regions
/// a demodulation was actually spent on, not every region the chain saw.
fn dmr_scan(shared: &Shared, b: &Chosen) -> Option<hk_detect::dmr_tier2::Tier2Scan> {
    let c = &shared.counters.chains;
    if !crate::dmr::bandwidth_admits(b.request.bandwidth_hz) {
        return None;
    }
    inc(&c.dmr_scanned);
    let info = InputInfo {
        time: b.time,
        discontinuity: Discontinuity::NONE,
        dropped_before: 0,
        provenance: &b.prov,
    };
    let scan = crate::dmr::identify(info, &b.iq, &b.request)?;
    inc(&c.dmr_identified);
    add(
        &c.dmr_headers_refused,
        (scan.bptc_failed + scan.check_failed) as u64,
    );
    if crate::debug_enabled() {
        eprintln!(
            "hk-pipeline: dmr-tier2 {:.4} MHz: {}",
            (b.prov.tune.center_hz + b.request.center_offset_hz) / 1e6,
            scan.verdict().unwrap_or_default()
        );
    }
    Some(scan)
}

/// Appends what was measured to `emitter`: the classification ([`crate::classify::record`], which
/// always writes) and, where the region was identified as DMR, its verdict row and its headers.
fn write(shared: &Shared, emitter: EmitterId, measured: &Measured) {
    let c = &shared.counters.chains;
    if let Some(classification) = &measured.classification {
        let mut repo = shared.repo();
        match crate::classify::record(&mut repo, emitter, classification) {
            Ok(_) => inc(&c.classifications),
            Err(e) => {
                inc(&c.errors);
                eprintln!("hk-pipeline: classify write: {e}");
            }
        }
    }
    let (Some(scan), Some(region)) = (&measured.dmr, measured.region) else {
        return;
    };
    {
        let mut repo = shared.repo();
        if let Err(e) = crate::dmr::record(&mut repo, emitter, scan, region.t) {
            inc(&c.errors);
            eprintln!("hk-pipeline: dmr-tier2 write: {e}");
        }
    }
    // Outside the repository borrow: publishing takes its own time and no other writer should
    // queue behind it.
    crate::dmr::publish_headers(
        shared,
        emitter,
        scan,
        region.center_hz,
        region.bandwidth_hz,
        region.t,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The built-in `classify` node (`super::super::spec::BUILTIN_CHAINS`).
    const SHIPPED: ClassifyNode = ClassifyNode {
        pad_s: 0.02,
        retain_s: 3.0,
        window_s: 0.25,
    };

    /// The HackRF's rate, and the rate a replay test runs at.
    const RATES: [(f64, &str); 2] = [(20e6, "HackRF"), (2.4e6, "replay")];

    fn mb(bytes: usize) -> f64 {
        bytes as f64 / (1 << 20) as f64
    }

    /// The buffer policy **before T-886**, for the before number: the node's whole `retain_s` at
    /// the rate, capped at the fsk ceiling, trimmed back to it only once the `Vec` had grown to
    /// twice it — so its residency peaked at `2 x retain`.
    fn before_bytes(node: &ClassifyNode, fs: f64) -> usize {
        let retain =
            ((node.retain_s * fs) as usize).clamp(1, super::super::fsk::MAX_RETAIN_SAMPLES);
        2 * retain * std::mem::size_of::<Complex<i8>>()
    }

    /// Feeds `seconds` of samples through the buffer in ring-sized chunks, as the chain does, and
    /// returns it — its `bytes()` is the residency that run cost.
    fn feed(node: &ClassifyNode, fs: f64, seconds: f64) -> Rolling {
        let retain = retain_samples(node, fs);
        let mut buf = Rolling::new(retain);
        let chunk = vec![Complex::new(1i8, -1i8); 1 << 16];
        buf.restart(0, Timestamp::UNIX_EPOCH);
        let total = (seconds * fs) as usize;
        let mut sent = 0;
        while sent < total {
            let n = chunk.len().min(total - sent);
            buf.extend(&chunk[..n], fs);
            sent += n;
        }
        buf
    }

    /// **T-886: the rolling buffer is sized by what the classifier reads, and never peaks at
    /// twice it.**
    ///
    /// Measured here, per chain, feeding a full `retain_s` and more through the buffer:
    ///
    /// | rate | before (`retain_s` at 2x) | after | 4 chains, before → after |
    /// |---|---|---|---|
    /// | 20 Msps | 64.0 MB | 32.0 MB | 256 MB → 128 MB |
    /// | 2.4 Msps | 27.5 MB | 10.5 MB | 110 MB → 42 MB |
    #[test]
    fn t886_the_classify_buffer_costs_what_it_reads_not_twice_the_node() {
        for (fs, name) in RATES {
            let before = before_bytes(&SHIPPED, fs);
            let after = feed(&SHIPPED, fs, 4.0).bytes();
            eprintln!(
                "[T-886] {name} {:.1} Msps: {:.1} MB -> {:.1} MB per chain ({:.0} MB -> {:.0} MB \
                 for the spec's four)",
                fs / 1e6,
                mb(before),
                mb(after),
                4.0 * mb(before),
                4.0 * mb(after)
            );
            assert!(
                after * 2 <= before,
                "{name}: {:.1} MB is not at most half of {:.1} MB",
                mb(after),
                mb(before)
            );
            // Exactly the retention, never a byte of overshoot: the bound is the peak.
            assert_eq!(
                after,
                retain_samples(&SHIPPED, fs) * std::mem::size_of::<Complex<i8>>()
            );
        }
    }

    /// The sizing keeps what the chain can still use: what it reads, plus the measured member-box
    /// arrival lag — and never more than the node asks for, or the fsk ceiling allows.
    #[test]
    fn t886_the_retention_covers_the_analysed_window_and_the_measured_arrival_lag() {
        let read_s = SHIPPED.window_s + 2.0 * SHIPPED.pad_s;
        for (fs, name) in RATES {
            let held_s = retain_samples(&SHIPPED, fs) as f64 / fs;
            assert!(
                held_s >= read_s,
                "{name}: {held_s} s cannot hold the {read_s} s the classifier reads"
            );
            assert!(
                held_s <= SHIPPED.retain_s,
                "{name}: never more than the node asked for"
            );
            assert!(retain_samples(&SHIPPED, fs) <= super::super::fsk::MAX_RETAIN_SAMPLES);
        }
        // Below the ceiling the retention is exactly the measured worst-case lag plus what is
        // read: 1.59 s of lag was the worst of 84 groups, so a group that arrives late is still
        // in the buffer.
        assert!(
            (retain_samples(&SHIPPED, 2.4e6) as f64 / 2.4e6 - (read_s + MEMBER_LAG_S)).abs() < 1e-6
        );
        // A node asking for less than that keeps its own (smaller) answer.
        let small = ClassifyNode {
            retain_s: 0.5,
            ..SHIPPED
        };
        assert_eq!(retain_samples(&small, 1e6), 500_000);
    }

    /// The buffer keeps the newest samples, with the sample index and capture time of what it
    /// still holds — the arithmetic every chosen box is cut by.
    #[test]
    fn t886_the_rolling_buffer_drops_the_oldest_and_keeps_the_arithmetic() {
        let fs = 1e6;
        let mut buf = Rolling::new(10);
        buf.restart(100, Timestamp::UNIX_EPOCH);
        let s: Vec<Complex<i8>> = (0..8).map(|i| Complex::new(i as i8, 0)).collect();
        buf.extend(&s, fs);
        assert_eq!((buf.base, buf.end(), buf.is_empty()), (100, 108, false));
        assert_eq!(buf.copy(102, 105), s[2..5]);
        assert_eq!(buf.time_at(100, fs), Timestamp::UNIX_EPOCH);
        // Past the retention the oldest go, and `base`/`base_time` follow them.
        buf.extend(&s, fs);
        assert_eq!((buf.base, buf.end()), (106, 116));
        assert_eq!(buf.copy(106, 108), s[6..8]);
        assert_eq!(
            buf.time_at(106, fs),
            Timestamp::UNIX_EPOCH.saturating_add_nanos(6_000)
        );
        assert_eq!(buf.bytes(), 10 * std::mem::size_of::<Complex<i8>>());
    }
}
