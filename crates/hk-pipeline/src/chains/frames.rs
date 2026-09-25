//! The narrowband-FSK **frame hunt** as a measuring chain on a running pipeline (T-950,
//! `SIGNAL-088`).
//!
//! # Why this exists
//!
//! The explorer found FLEX — the dominant 929–932 MHz paging protocol — on three channels in San
//! Francisco and the app decoded none of it: the only pager path was the POCSAG recipe, which a
//! user has to start, and the `fsk-bursts` decode chain wants short bursts (`bursty`, four
//! detections) and frames them blind. A FLEX transmitter keys up for a header and a few 160 ms
//! blocks, or for whole 1.875 s frames back to back, so its tracks closed `unmatched`.
//!
//! This chain attaches on [`Trigger::EveryTrack`](super::spec::Trigger::EveryTrack) to every
//! narrowband track, **beside** whatever decode chain was selected, as the classifier and the
//! sweep characteriser do. It channelises the track, and hands each transmission to the framing
//! catalogue — [`hk_demod::flex`] today — which answers with frames only when a sync-1 is found
//! **and** its frame information word passes BCH(31,21). So which decoder a signal gets is chosen
//! by the signal's own symbols, never by the band it sits in: the chain reads no band plan and no
//! frequency beyond the track's own measured edges and where the device says it is tuned.
//!
//! # What it writes, against which emitter
//!
//! Against the emitter the inventory recorded **for this track**
//! ([`crate::Inventory::recorded_emitter_of_track`]), never one found by frequency — the rule the
//! classifier follows, for the same reason. Per batch of decoded frames:
//!
//! - a **Demodulation** whose `estimated_params` are what the frames measured: the data symbol
//!   rate and level count the codewords chose (`hk_demod::flex::demod`), the header's outer
//!   deviation and the carrier offset;
//! - one **Decode** per frame (`frame_model: flex-frame`), metadata only: cycle, frame, the
//!   declared and the measured mode, codeword counts;
//! - one **Decode** per page (`flex-page`): capcode, type and phase as metadata; message text as
//!   content **only** under a class that permits it — the paging bands derive `restricted-paging`
//!   ([`crate::class`]), so on the air it is withheld;
//! - **decoder evidence** `flex` ([`crate::family::record_decoder_evidence`]), which the family
//!   vocabulary maps to the `paging` service, then a re-rank of the emitter's explanations.
//!
//! Writes happen as batches are decoded, so a long transmission is decoded incrementally and its
//! earlier frames are never re-decoded (the region extends; its decoded part does not repeat).
//!
//! # Cost
//!
//! One thread per hunted track, at most `max_chains` alive (refused above, counted, retried when a
//! slot frees — the classifier's rule). A continuous emitter that stays on the air for a whole
//! segment without one sync-1 is abandoned, so a carrier cannot hold a slot for its lifetime
//! while the bursty channels that do carry frames wait (measured on `flex-pagers-930p8`: four
//! continuous non-FLEX carriers held all four slots for the whole 12 s). Memory is the channelised buffer — `retain_s` of a
//! ~80 kSps complex stream, about 4 MB at 6.5 s whatever the device rate — plus one decode's
//! working copies. The DDC runs at the input rate for the track's lifetime; a narrowband track is
//! short-lived or one of few.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use hk_core::ProvenanceHandle;
use hk_demod::flex::{self, FLEX_DECODER_ID, FLEX_DECODER_VERSION, FlexFrame};
use hk_dsp::{Ddc, DdcSpec, InputInfo, IqSample};
use hk_model::{
    ContentClass, CrcStatus, Decode, DecodeId, Demodulation, DemodulationId, EmitterId,
    EstimatedParams, TimeRange, Timestamp, TrackId,
};
use num_complex::Complex32;
use serde_json::json;

use super::{ChainMsg, ChainReader, Next};
use crate::class::classify_emitter;
use crate::events::{Candidate, MemberBox};
use crate::gate::GateCursor;
use crate::run::Shared;
use crate::stats::{add, inc};

/// Channel passband the DDC keeps around the track's centre, Hz: FLEX occupies about ±8 kHz, and
/// the rest is room for a first box that saw only one tone lobe of the emission.
const CHANNEL_BANDWIDTH_HZ: f64 = 40_000.0;
/// Gap between member boxes still treated as one transmission, s. A continuous transmitter's
/// boxes abut; a separate key-up is at least a frame away.
const GAP_S: f64 = 0.25;
/// Overlap between consecutive segments of one long transmission, s: one frame plus its header,
/// so a frame cut by a segment edge is whole in the next.
const OVERLAP_S: f64 = flex::frame::FRAME_S + 0.2;
/// How long a transmission may stay on the air before the chain looks for a sync-1 in it, s. A
/// keyed FLEX transmitter sends a frame every 1.875 s, so any stretch this long holds a whole
/// sync-1 and FIW; a stretch without one is not FLEX, and the chain gives its slot back.
const PROBE_S: f64 = flex::frame::FRAME_S + 0.3;
/// How far the stream must run past a transmission's last box before it is taken as ended, s:
/// member boxes trail their samples by up to ~1.6 s (measured for the classifier, T-886), so a
/// box extending it could still arrive until then. Without this a one-off key-up would wait for
/// the *next* transmission — seconds later — and find its samples gone from the buffer (measured
/// on `flex-pagers-930p8`: the 929.608 MHz frame at 2.13 s, next key-up at 11.5 s).
const SETTLE_S: f64 = 2.0;
/// Longest wait for the track's inventory emitter once there is something to write.
const EMITTER_WAIT: Duration = Duration::from_secs(2);
/// Most pending transmissions one chain queues (the fsk chain's bound, T-558).
const MAX_PENDING_GROUPS: usize = 4096;

/// What one hunt may spend. Built from
/// [`ChainShape::FskFrames`](super::spec::ChainShape::FskFrames).
#[derive(Clone, Copy, Debug)]
pub(crate) struct FramesNode {
    /// Pad either side of a transmission, s.
    pub pad_s: f64,
    /// Channelised buffer, s.
    pub retain_s: f64,
    /// Longest stretch decoded at once, s.
    pub segment_s: f64,
}

/// One transmission: member boxes merged across gaps shorter than [`GAP_S`].
#[derive(Clone, Debug)]
struct Group {
    /// Source sample range.
    start: u64,
    end: u64,
    f_lo: f64,
    f_hi: f64,
    /// Next source sample to decode from (a long transmission is decoded in segments).
    from: u64,
}

fn add_member(groups: &mut Vec<Group>, m: &MemberBox, gap: u64, missed: &std::sync::atomic::AtomicU64) {
    let (s, e) = (m.samples.start, m.samples.end);
    if let Some(g) = groups
        .iter_mut()
        .find(|g| s < g.end.saturating_add(gap) && e.saturating_add(gap) > g.start)
    {
        g.start = g.start.min(s);
        g.from = g.from.min(s);
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
        from: s,
    });
    groups.sort_by_key(|g| g.start);
    if groups.len() > MAX_PENDING_GROUPS {
        let over = groups.len() - MAX_PENDING_GROUPS;
        groups.drain(..over);
        add(missed, over as u64);
    }
}

/// The channelised stream, one contiguous span of it.
struct Channel {
    ddc: Ddc,
    /// RF centre of the channel, Hz.
    center_hz: f64,
    iq: VecDeque<Complex32>,
    /// Source index of `iq[0]`.
    first_source: f64,
    /// Source samples per output sample.
    spo: f64,
    /// Output rate, Hz.
    rate: f64,
    /// A source index and its capture time, to convert source indices to time.
    anchor: (u64, Timestamp),
    retain: usize,
    /// The channel sits at the band edge and is taken in the Nyquist-wrapped frame: the input is
    /// multiplied by `(−1)^n` (by absolute sample index), which moves it by `fs / 2` to where the
    /// DDC can reach it — `hk_estimate`'s snippet rule. A FLEX channel of the explorer's capture
    /// is 8 kHz inside the 2.4 Msps band edge.
    wrapped: bool,
    scratch: Vec<Complex32>,
}

impl Channel {
    /// Source index one past the newest output sample.
    fn end_source(&self) -> f64 {
        self.first_source + self.iq.len() as f64 * self.spo
    }

    fn time_of(&self, source: f64, fs: f64) -> Timestamp {
        self.anchor
            .1
            .saturating_add_nanos(((source - self.anchor.0 as f64) * 1e9 / fs) as i64)
    }

    fn push(&mut self, info: InputInfo<'_>, samples: &[num_complex::Complex<i8>]) {
        let end = self.end_source();
        let held = !self.iq.is_empty();
        let processed = if self.wrapped {
            let odd = info.time.sample_index & 1;
            self.scratch.clear();
            self.scratch
                .extend(samples.iter().enumerate().map(|(k, v)| {
                    let v = v.to_complex32();
                    if (k as u64 + odd) & 1 == 1 { -v } else { v }
                }));
            self.ddc.process(info, &self.scratch)
        } else {
            self.ddc.process(info, samples)
        };
        let Ok(block) = processed else {
            self.iq.clear();
            return;
        };
        if block.samples.is_empty() {
            return;
        }
        let h = &block.header;
        let first = h.time.source_index;
        let contiguous = held
            && h.discontinuity.is_empty()
            && h.dropped_before == 0
            && (first - end).abs() <= 2.0 * h.time.source_per_output;
        let (spo, rate) = (h.time.source_per_output, h.sample_rate_hz);
        let anchor = (h.time.time.sample_index, h.time.time.host_time);
        let out: Vec<Complex32> = block.samples.to_vec();
        if !contiguous {
            self.iq.clear();
            self.first_source = first;
        }
        self.spo = spo;
        self.rate = rate;
        self.anchor = anchor;
        self.iq.extend(out);
        let over = self.iq.len().saturating_sub(self.retain);
        if over > 0 {
            self.iq.drain(..over);
            self.first_source += over as f64 * self.spo;
        }
    }

    /// A copy of the source span `[s, e)`, mixed from the channel centre to `center_hz`, with the
    /// source index of its first sample; `None` when the channel no longer (or never) held it.
    fn span(&self, s: u64, e: u64, center_hz: f64) -> Option<(Vec<Complex32>, f64)> {
        if self.iq.is_empty() || self.spo <= 0.0 {
            return None;
        }
        let a = ((s as f64 - self.first_source) / self.spo).floor();
        let b = ((e as f64 - self.first_source) / self.spo).ceil();
        let a = a.max(0.0) as usize;
        let b = (b.max(0.0) as usize).min(self.iq.len());
        if b <= a + 1 {
            return None;
        }
        let shift = center_hz - self.center_hz;
        let w = -std::f64::consts::TAU * shift / self.rate;
        let out = self
            .iq
            .range(a..b)
            .enumerate()
            .map(|(k, v)| {
                let (sn, cs) = (w * k as f64).sin_cos();
                v * Complex32::new(cs as f32, sn as f32)
            })
            .collect();
        Some((out, self.first_source + a as f64 * self.spo))
    }
}

/// One decoded frame, placed.
struct Found {
    frame: FlexFrame,
    /// Source index of the sync marker.
    source: f64,
    t: Timestamp,
}

/// Runs the hunt for `cand`'s track until it is detached or the stream ends.
pub(crate) fn run(
    shared: Arc<Shared>,
    rx: Receiver<ChainMsg>,
    cand: Candidate,
    node: FramesNode,
    cursor: GateCursor,
) {
    let fs = shared.fs;
    let c = &shared.counters.chains;
    let Some(track) = cand.track else {
        return;
    };
    let pad = (node.pad_s * fs) as u64;
    let gap = (GAP_S * fs) as u64;
    let segment = ((node.segment_s * fs) as u64).max(1);
    let overlap = (OVERLAP_S * fs) as u64;
    let probe = (PROBE_S * fs) as u64;
    let settle = (SETTLE_S * fs) as u64;
    let mut cr = ChainReader::new(
        Arc::clone(&shared),
        cand.first_sample.saturating_sub(pad),
        cursor,
    );
    let center_hz = 0.5 * (cand.f_lo_hz + cand.f_hi_hz);
    let mut channel: Option<Channel> = None;
    let mut prov: Option<ProvenanceHandle> = None;
    let mut groups: Vec<Group> = Vec::new();
    let mut found: Vec<Found> = Vec::new();
    let mut written = 0usize;
    let mut emitter: Option<EmitterId> = None;
    let (mut detach, mut closed) = (false, false);
    let mut abandon = false;
    loop {
        loop {
            match rx.try_recv() {
                Ok(ChainMsg::Member(m)) => add_member(&mut groups, &m, gap, &c.frames_missed),
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
                    // A new provenance (a retune) is a new channel plan.
                    if prov.as_ref().is_none_or(|p| p.id() != ch.provenance.id()) {
                        prov = Some(ch.provenance.clone());
                        channel = new_channel(&ch.provenance, center_hz, node.retain_s);
                    }
                    if let Some(chan) = channel.as_mut() {
                        chan.push(InputInfo::from(&ch), &cr.buf[..ch.len]);
                    }
                    cr.release_to(ch.end_sample());
                }
                Next::Lost => {
                    if let Some(chan) = channel.as_mut() {
                        chan.iq.clear();
                    }
                }
                Next::Idle => {}
                Next::Closed => closed = true,
            }
        } else if !detach {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(ChainMsg::Member(m)) => add_member(&mut groups, &m, gap, &c.frames_missed),
                Ok(ChainMsg::Detach) | Err(RecvTimeoutError::Disconnected) => detach = true,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }

        // Decode every transmission (or long stretch of one) whose samples have arrived.
        if let Some(chan) = channel.as_ref() {
            let have = chan.end_source() as u64;
            let n = groups.len();
            let mut done = 0;
            for (i, g) in groups.iter_mut().enumerate() {
                let arrived = g.end + pad <= have || closed;
                // Until the chain is detached the newest transmission may still grow.
                let settled = i + 1 < n || detach || have >= g.end + gap + settle;
                if settled && arrived {
                    let (s, e) = (g.from.saturating_sub(pad), g.end + pad);
                    decode_span(&shared, chan, g, s, e, true, &mut found);
                    done += 1;
                    continue;
                }
                if settled {
                    // Its samples have not all arrived yet.
                    break;
                }
                // A transmission still going: decode it a segment at a time, overlapping by a
                // frame, so it is decoded as it extends rather than when it ends.
                // The first look is a short probe (PROBE_S), then whole segments.
                loop {
                    let len = if found.is_empty() && g.from == g.start {
                        probe
                    } else {
                        segment
                    };
                    // Only a stretch the transmission is known to cover, and whose samples have
                    // all arrived; the rest waits for more boxes or the settle.
                    if have < g.from + len + pad || g.end < g.from + len {
                        break;
                    }
                    let e = g.from + len;
                    let span = (g.from.saturating_sub(pad), e + pad);
                    decode_span(&shared, chan, g, span.0, span.1, false, &mut found);
                    g.from = e.saturating_sub(overlap);
                    // A transmitter on the air for a whole segment without one sync-1 is not
                    // sending FLEX (a FLEX transmitter sends a frame every 1.875 s it is keyed):
                    // give the slot back rather than hold it for a carrier's lifetime.
                    if found.is_empty() {
                        abandon = true;
                        break;
                    }
                }
                break;
            }
            groups.drain(..done);
        }

        // Write what is decoded, once the track has an entry.
        if found.len() > written {
            if emitter.is_none() {
                emitter = emitter_of(&shared, track);
            }
            if let Some(e) = emitter {
                write(&shared, e, track, &cand, &found[written..]);
                written = found.len();
            }
        }

        if detach && (groups.is_empty() || closed) {
            break;
        }
        if abandon {
            inc(&c.frames_abandoned);
            if crate::debug_enabled() {
                eprintln!(
                    "hk-pipeline: fsk-frames {:.4} MHz: a segment on the air without a sync; \
                     abandoned",
                    center_hz / 1e6
                );
            }
            return;
        }
    }
    drop(cr);
    if found.len() > written {
        let deadline = Instant::now() + EMITTER_WAIT;
        loop {
            let stopping = shared.stop.load(std::sync::atomic::Ordering::SeqCst);
            if let Some(e) = emitter.or_else(|| emitter_of(&shared, track)) {
                write(&shared, e, track, &cand, &found[written..]);
                return;
            }
            if stopping || Instant::now() >= deadline {
                add(&c.frames_no_emitter, (found.len() - written) as u64);
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

fn new_channel(prov: &ProvenanceHandle, center_hz: f64, retain_s: f64) -> Option<Channel> {
    let fs = prov.tune.sample_rate_hz;
    let mut offset = center_hz - prov.tune.center_hz;
    // Past this the DDC cannot place the passband inside the band: take the wrapped frame.
    let wrapped = offset.abs() + CHANNEL_BANDWIDTH_HZ / 2.0 > 0.45 * fs;
    if wrapped {
        offset -= offset.signum() * fs / 2.0;
    }
    let spec = DdcSpec::new(offset, CHANNEL_BANDWIDTH_HZ);
    let ddc = Ddc::new(spec, fs).ok()?;
    let rate = ddc.output_rate_hz();
    Some(Channel {
        ddc,
        center_hz,
        iq: VecDeque::new(),
        first_source: 0.0,
        spo: fs / rate,
        rate,
        anchor: (0, Timestamp::UNIX_EPOCH),
        retain: (retain_s * rate) as usize,
        wrapped,
        scratch: Vec::new(),
    })
}

/// Decodes source span `[s, e)` of `g` and appends new frames (a frame already found — an
/// overlap between segments — is not added twice). Unless the span is the transmission's `last`,
/// a frame whose data section runs past its end is left for the next segment, which overlaps by
/// a frame and so holds it whole.
#[allow(clippy::too_many_arguments)]
fn decode_span(
    shared: &Shared,
    chan: &Channel,
    g: &Group,
    s: u64,
    e: u64,
    last: bool,
    found: &mut Vec<Found>,
) {
    let c = &shared.counters.chains;
    let center = 0.5 * (g.f_lo + g.f_hi);
    let Some((x, first)) = chan.span(s, e, center) else {
        inc(&c.frames_missed);
        if crate::debug_enabled() {
            eprintln!(
                "hk-pipeline: fsk-frames {:.4} MHz: samples {s}..{e} not held (channel holds \
                 {:.0}..{:.0})",
                center / 1e6,
                chan.first_source,
                chan.end_source(),
            );
        }
        return;
    };
    let Ok(report) = flex::decode(&x, chan.rate) else {
        inc(&c.frames_missed);
        return;
    };
    if crate::debug_enabled() {
        eprintln!(
            "hk-pipeline: fsk-frames {:.4} MHz: decoded {:.3} s at {:.0} Sps (channel {:.4} MHz): \
             {} frame(s)",
            center / 1e6,
            x.len() as f64 / chan.rate,
            chan.rate,
            chan.center_hz / 1e6,
            report.frames.len(),
        );
    }
    let symbol = shared.fs / flex::frame::HEADER_BAUD;
    for f in report.frames {
        // Only a frame whose FIW passed its check is a frame (sync alone is 32 bits of pattern).
        if f.fiw.is_none_or(|w| !w.checksum_ok) {
            continue;
        }
        let source = first + f.marker_sample * chan.spo;
        if found.iter().any(|x| (x.source - source).abs() < 4.0 * symbol) {
            continue;
        }
        let frame_end = source + (flex::frame::FRAME_S - 0.02) * shared.fs;
        if !last && frame_end > e as f64 {
            continue;
        }
        inc(&c.flex_frames);
        add(
            &c.flex_pages,
            f.phases.iter().map(|p| p.pages.len()).sum::<usize>() as u64,
        );
        if crate::debug_enabled() {
            eprintln!(
                "hk-pipeline: fsk-frames {:.4} MHz: FLEX frame {:?} code {:04X} measured {:?} \
                 ({} clean of {} words)",
                center / 1e6,
                f.fiw.map(|w| (w.cycle, w.frame)),
                f.code,
                f.measured.map(|m| (m.baud, m.levels)),
                f.clean_words(),
                f.words(),
            );
        }
        found.push(Found {
            t: chan.time_of(source, shared.fs),
            source,
            frame: f,
        });
    }
}

/// The entry the inventory recorded for `track`, if any.
fn emitter_of(shared: &Shared, track: TrackId) -> Option<EmitterId> {
    shared
        .inventory
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .recorded_emitter_of_track(track)
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    Some(v[v.len() / 2])
}

/// The Demodulation, Decodes and decoder evidence for a batch of frames (module docs).
fn write(shared: &Shared, emitter: EmitterId, track: TrackId, cand: &Candidate, batch: &[Found]) {
    let c = &shared.counters.chains;
    let measured: Vec<&Found> = batch.iter().filter(|f| f.frame.measured.is_some()).collect();
    let (f_lo, f_hi) = (cand.f_lo_hz, cand.f_hi_hz);
    let class = classify_emitter(
        &shared.cfg.settings.classify,
        shared.cfg.source_class,
        f_lo,
        f_hi,
    )
    .map_or(ContentClass::FAIL_CLOSED, |(c, _)| c);
    // The mode the batch's codewords chose, by majority; an abstaining batch records none.
    let mode = {
        let mut counts: Vec<((u32, u8), usize)> = Vec::new();
        for f in &measured {
            let m = f.frame.measured.unwrap();
            match counts.iter_mut().find(|(k, _)| *k == (m.baud, m.levels)) {
                Some(e) => e.1 += 1,
                None => counts.push(((m.baud, m.levels), 1)),
            }
        }
        counts.into_iter().max_by_key(|(_, n)| *n).map(|(k, _)| k)
    };
    let words: usize = batch.iter().map(|f| f.frame.words()).sum();
    let clean: usize = batch.iter().map(|f| f.frame.clean_words()).sum();
    let t0 = batch.iter().map(|f| f.t).min().unwrap_or(Timestamp::UNIX_EPOCH);
    let t1 = batch
        .iter()
        .map(|f| f.t)
        .max()
        .unwrap_or(t0)
        .saturating_add_nanos((flex::frame::FRAME_S * 1e9) as i64);
    let demod_id = DemodulationId::new();
    let demod = Demodulation {
        id: demod_id,
        emitter_ref: Some(emitter),
        detection_ref: None,
        recording_ref: None,
        mode: match mode {
            Some((_, 4)) => "4fsk".into(),
            Some(_) => "2fsk".into(),
            None => "fsk".into(),
        },
        params: EstimatedParams {
            symbol_rate_hz: mode.map(|(b, _)| f64::from(b)),
            deviation_hz: median(batch.iter().map(|f| f.frame.deviation_hz).collect()),
            cfo_hz: median(batch.iter().map(|f| f.frame.carrier_offset_hz).collect()),
            mod_order: mode.map(|(_, l)| u32::from(l)),
            roll_off: None,
            bandwidth_hz: Some(18_000.0),
            pilot_hz: None,
        },
        lock_quality: (words > 0).then(|| clean as f64 / words as f64),
        evm_db: None,
        time: TimeRange::new(t0, t1),
        demod_version: FLEX_DECODER_VERSION.into(),
    };
    let mut repo = shared.repo();
    let result = (|| -> Result<(), hk_model::RepoError> {
        repo.insert_demodulation(&demod)?;
        for f in batch {
            let fr = &f.frame;
            let fiw = fr.fiw.expect("only FIW-valid frames are kept");
            let m = fr.measured;
            repo.insert_decode(&Decode {
                id: DecodeId::new(),
                demodulation_ref: Some(demod_id),
                recording_ref: None,
                decoder_id: FLEX_DECODER_ID.into(),
                decoder_version: FLEX_DECODER_VERSION.into(),
                frame_model: "flex-frame".into(),
                metadata: json!({
                    "cycle": fiw.cycle,
                    "frame": fiw.frame,
                    "fiw_corrected": fiw.corrected,
                    "sync_errors": fr.sync_errors,
                    "inverted": fr.inverted,
                    "code": format!("{:04X}", fr.code),
                    "declared": fr.declared.map(|d| json!({
                        "baud": d.baud, "levels": d.levels, "bps": d.bps(),
                    })),
                    "measured": m.map(|m| json!({
                        "baud": m.baud,
                        "levels": m.levels,
                        "levels_decisive": fr.levels_decisive,
                        "rate_decisive": fr.rate_decisive,
                    })),
                    "mode_agrees": fr.mode_agrees(),
                    "hypotheses": fr.hypotheses.iter().map(|h| json!({
                        "baud": h.baud, "levels": h.levels, "live_phases": h.live_phases,
                        "dead_phases": h.dead_phases, "clean_words": h.clean_words,
                        "valid_words": h.valid_words, "words": h.words,
                    })).collect::<Vec<_>>(),
                    "blocks_on_air": fr.blocks_on_air,
                    "words": fr.words(),
                    "valid_words": fr.valid_words(),
                    "clean_words": fr.clean_words(),
                    "deviation_hz": fr.deviation_hz,
                    "carrier_offset_hz": fr.carrier_offset_hz,
                    "inner_fraction": fr.inner_fraction,
                    "pages": fr.phases.iter().map(|p| p.pages.len()).sum::<usize>(),
                }),
                content: None,
                // The FIW passed BCH(31,21) and its checksum: the frame's own check.
                crc_status: if fiw.corrected == 0 {
                    CrcStatus::Valid
                } else {
                    CrcStatus::Corrected
                },
                identity: None,
                content_class: class,
                t: f.t,
                provenance: None,
            })?;
            for p in fr.phases.iter().flat_map(|p| &p.pages) {
                repo.insert_decode(&Decode {
                    id: DecodeId::new(),
                    demodulation_ref: Some(demod_id),
                    recording_ref: None,
                    decoder_id: FLEX_DECODER_ID.into(),
                    decoder_version: FLEX_DECODER_VERSION.into(),
                    frame_model: "flex-page".into(),
                    metadata: json!({
                        "capcode": p.capcode.to_string(),
                        "long_address": p.long_address,
                        "type": p.kind.as_str(),
                        "phase": p.phase.to_string(),
                        "message_words": p.message_words,
                        "cycle": fiw.cycle,
                        "frame": fiw.frame,
                    }),
                    content: p
                        .text
                        .as_ref()
                        .filter(|_| class.permits_content())
                        .map(|t| json!({ "text": t })),
                    crc_status: if p.complete {
                        CrcStatus::Valid
                    } else {
                        CrcStatus::Invalid
                    },
                    identity: None,
                    content_class: class,
                    t: f.t,
                    provenance: None,
                })?;
            }
        }
        crate::family::record_decoder_evidence(&mut repo, emitter, FLEX_DECODER_ID, 1.0, t1)?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            add(&c.decodes, batch.len() as u64);
            add(&c.demodulations, 1);
            let mut inv = shared
                .inventory
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Err(e) = inv.chain_emitter(&mut repo, Some(track), emitter) {
                inc(&c.errors);
                eprintln!("hk-pipeline: fsk-frames emitter: {e}");
            }
        }
        Err(e) => {
            inc(&c.errors);
            eprintln!("hk-pipeline: fsk-frames write: {e}");
        }
    }
}
