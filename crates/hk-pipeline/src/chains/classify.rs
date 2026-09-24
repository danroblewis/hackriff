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
//! attach is refused and counted, `classify_admission_refused`), each counted against the run-wide
//! [`MAX_RUNTIME_CHAINS`](super::MAX_RUNTIME_CHAINS). Memory is the rolling `retain_s` buffer
//! (capped at the fsk chain's sample ceiling) plus one owned copy of the chosen box, which is at
//! most `window_s` plus its pads. Once a box spans the whole window nothing later can beat it, so
//! the chain stops buffering and only reads and releases, which keeps the lossless flow gate moving.
//! Per classification the cost is [`crate::classify::classify_and_record`]'s documented bound.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use hk_classify::{Classifier, SymbolEstimator};
use hk_core::{Discontinuity, ProvenanceHandle};
use hk_dsp::InputInfo;
use hk_estimate::SnippetRequest;
use hk_model::classify::Classification;
use hk_model::{EmitterId, SampleTime, Timestamp, TrackId};
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
}

/// The box chosen so far: an owned copy of its padded samples and what classifying it needs.
struct Chosen {
    iq: Vec<Complex<i8>>,
    time: SampleTime,
    prov: ProvenanceHandle,
    request: SnippetRequest,
    /// Analysed extent, samples.
    span: u64,
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
    let retain = ((node.retain_s * fs) as usize).clamp(1, super::fsk::MAX_RETAIN_SAMPLES);
    let mut cr = ChainReader::new(
        Arc::clone(&shared),
        cand.first_sample.saturating_sub(pad),
        cursor,
    );
    let mut buf: Vec<Complex<i8>> = Vec::new();
    let mut base = 0u64;
    let mut base_time = Timestamp::UNIX_EPOCH;
    let mut prov: Option<ProvenanceHandle> = None;
    let mut groups: Vec<Group> = Vec::new();
    let mut chosen: Option<Chosen> = None;
    // Classified once the chosen box spans the whole window (a continuous emission), else at the
    // end; held until the inventory has an entry to write it against.
    let mut classified: Option<Option<Classification>> = None;
    let mut last_poll: Option<Instant> = None;
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
                            && ch.first_sample() == base + buf.len() as u64
                            && prov.as_ref().is_some_and(|p| p.id() == ch.provenance.id());
                        if !contiguous {
                            buf.clear();
                            base = ch.first_sample();
                            base_time = ch.time.host_time;
                            prov = Some(ch.provenance.clone());
                        }
                        buf.extend_from_slice(&cr.buf[..ch.len]);
                        if buf.len() > 2 * retain {
                            let cut = buf.len() - retain;
                            buf.drain(..cut);
                            base += cut as u64;
                            base_time =
                                base_time.saturating_add_nanos((cut as f64 * 1e9 / fs) as i64);
                        }
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
        let buf_end = base + buf.len() as u64;
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
                    g.start.max(base + pad)
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
                if end <= start || s < base || e <= end || buf.is_empty() {
                    inc(&c.classify_missed);
                    continue;
                }
                let span = end - start;
                if chosen.as_ref().is_some_and(|b| b.span >= span) {
                    continue;
                }
                chosen = Some(Chosen {
                    iq: buf[(s - base) as usize..(e - base) as usize].to_vec(),
                    time: SampleTime {
                        sample_index: s,
                        host_time: base_time
                            .saturating_add_nanos(((s - base) as f64 * 1e9 / fs) as i64),
                    },
                    prov: p.clone(),
                    request: SnippetRequest {
                        start_index: start,
                        end_index: end,
                        center_offset_hz: 0.5 * (g.f_lo + g.f_hi) - p.tune.center_hz,
                        bandwidth_hz: (g.f_hi - g.f_lo).max(1.0),
                    },
                    span,
                });
            }
            groups.drain(..done);
        }
        if chosen.as_ref().is_some_and(|b| b.span >= window) && !full {
            // Nothing later can beat it: the buffer and the pending groups are no longer needed.
            buf = Vec::new();
            groups.clear();
        }

        // A continuous emission is classified as soon as it spans the window, and written as soon
        // as its track has an entry.
        if classified.is_none() && chosen.as_ref().is_some_and(|b| b.span >= window) {
            classified = Some(classify(&shared, chosen.as_ref().expect("chosen")));
        }
        match &classified {
            // The cascade abstained on the best box there will be: nothing to write.
            Some(None) => return,
            Some(Some(result)) => {
                let due = last_poll.is_none_or(|t| t.elapsed() >= EMITTER_POLL);
                if due && !detach {
                    last_poll = Some(Instant::now());
                    if let Some(emitter) = emitter_of(&shared, track) {
                        write(&shared, emitter, result);
                        return;
                    }
                }
            }
            None => {}
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

    let result = match classified {
        Some(r) => r,
        // No box of this track ever reached the chain's buffer: nothing was measured.
        None => chosen.as_ref().and_then(|b| classify(&shared, b)),
    };
    // Abstained upstream (counted in `classify`), or nothing measured: nothing to write.
    let Some(result) = result else {
        return;
    };
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
            write(&shared, emitter, &result);
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
fn classify(shared: &Shared, b: &Chosen) -> Option<Classification> {
    let info = InputInfo {
        time: b.time,
        discontinuity: Discontinuity::NONE,
        dropped_before: 0,
        provenance: &b.prov,
    };
    let mut c14 = SymbolEstimator::new();
    let out = crate::classify::classify_box(
        &Classifier::new(),
        &mut c14,
        // T-399: the receiver-line survey measured by the `hk-survey` reader, once for this
        // capture state. Read here, never measured here.
        shared.receiver.as_ref(),
        info,
        &b.iq,
        &b.request,
        b.time.host_time,
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
            out.as_ref().map_or("abstained".into(), |c| format!(
                "{} open-set {:.3} reasons {:?}",
                c.family, c.open_set_score, c.reasons
            ))
        );
    }
    out
}

/// The entry the inventory recorded for `track`, if any. Takes the inventory lock alone.
fn emitter_of(shared: &Shared, track: TrackId) -> Option<EmitterId> {
    shared
        .inventory
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .recorded_emitter_of_track(track)
}

/// Appends the classification to `emitter` ([`crate::classify::record`], which always writes).
fn write(shared: &Shared, emitter: EmitterId, classification: &Classification) {
    let c = &shared.counters.chains;
    let mut repo = shared.repo();
    match crate::classify::record(&mut repo, emitter, classification) {
        Ok(_) => inc(&c.classifications),
        Err(e) => {
            inc(&c.errors);
            eprintln!("hk-pipeline: classify write: {e}");
        }
    }
}
