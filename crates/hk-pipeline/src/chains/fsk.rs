//! FSK burst chain (C13/C14/C20/C21, AWARE-036).
//!
//! The chain keeps a rolling copy of the ring from the track's start and receives the track's
//! member boxes. Boxes that overlap in time are one burst (the detector emits one box per 2-FSK
//! tone lobe). A burst is demodulated (`FskReceiver`: snippet → C13 → C14, prior-led trials below
//! the trust floor with the standard-rate table) once a later burst has started or the chain is
//! detached, and once its samples plus pads are in the buffer. At detach (track closed, stream
//! end) framing is inferred across the bursts and `write_framed_bursts` stores Emitter,
//! Demodulations, Decodes and the Bitstream descriptor. **Content fails closed**: a Decode keeps
//! its payload only when a user classification rule covers the emitter and the source class is
//! not restricted ([`crate::class::classify_emitter`]). The bursts' hard bits are then published
//! on a gated bits stream ([`publish_bits`]).

use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use hk_classify::{Classifier, SymbolEstimator};
use hk_core::Discontinuity;
use hk_core::ProvenanceHandle;
use hk_demod::fsk::{
    DemodPriors, EmitterClassification, FramedRecordContext, FskBurst, FskReceiver,
    FskReceiverConfig, WrittenFraming, write_framed_bursts,
};
use hk_dsp::InputInfo;
use hk_estimate::SnippetRequest;
use hk_estimate::framing::{FramingConfig, infer_framing};
use hk_model::{DetectionId, SampleTime, Timestamp};
use hk_stream::{
    BinaryRecord, Publisher, PublisherConfig, RecordFlags, StreamError, StreamHeader, StreamKind,
};
use num_complex::Complex;

use super::taps::STREAM_INFER_INTERVAL;
use super::{ChainMsg, ChainReader, Next};
use crate::class::classify_emitter;
use crate::events::{Candidate, MemberBox};
use crate::gate::GateCursor;
use crate::run::Shared;
use crate::stats::{add, inc};
use hk_demod::fsk::record::effective_content_class;

#[derive(Clone, Debug)]
struct Group {
    start: u64,
    end: u64,
    f_lo: f64,
    f_hi: f64,
    detection: DetectionId,
}

fn add_member(groups: &mut Vec<Group>, m: MemberBox, missed: &std::sync::atomic::AtomicU64) {
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
    // T-558: bounded. Dropping the oldest is counted as the missed box it is.
    if groups.len() > MAX_PENDING_GROUPS {
        let over = groups.len() - MAX_PENDING_GROUPS;
        groups.drain(..over);
        add(missed, over as u64);
    }
}

/// Most samples one chain's retain buffer may hold, whatever the rate (T-558).
///
/// `retain_s` is a duration, so the buffer it asks for is `2 x retain_s x fs` samples — a figure
/// that grows with the device. The built-in `fsk-bursts` spec's 3 s is 24 MB at the 2 Msps a
/// replay runs at and **240 MB at the HackRF's 20 Msps**, per chain; a survey holding several at
/// once is then gigabytes of buffer for bursts that are milliseconds long. Sixteen mega-samples
/// (32 MB) is 8 s at 2 Msps and 0.8 s at 20 Msps, both far longer than a burst, and it is the
/// same shortfall the chain already handles when a box's samples have aged out of the ring
/// (`fsk_boxes_missed`) rather than a new failure mode.
const MAX_RETAIN_SAMPLES: usize = 16 << 20;

/// Most pending burst groups one chain queues (T-558). A group is only drained once its samples
/// have arrived, so a chain whose member boxes outrun its reader would otherwise grow one entry
/// per box for as long as it lives. The oldest go first: their samples are the ones the ring is
/// about to lose anyway.
const MAX_PENDING_GROUPS: usize = 4096;

#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    shared: Arc<Shared>,
    rx: Receiver<ChainMsg>,
    cand: Candidate,
    pad_s: f64,
    retain_s: f64,
    min_bursts: usize,
    max_bursts: usize,
    cursor: GateCursor,
) {
    let fs = shared.fs;
    let c = &shared.counters.chains;
    let pad = (pad_s * fs) as u64;
    let retain = ((retain_s * fs) as usize).clamp(1, MAX_RETAIN_SAMPLES);
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
    let mut bursts: Vec<FskBurst> = Vec::new();
    let mut first_det = cand.detection;
    let (mut f_lo, mut f_hi) = (cand.f_lo_hz, cand.f_hi_hz);
    let mut receiver = FskReceiver::new(FskReceiverConfig::default());
    let priors = DemodPriors {
        standard_rates: true,
        ..Default::default()
    };
    let (mut detach, mut closed) = (false, false);
    // T-247: the one burst the C15 cascade runs on at detach — the longest the chain demodulated,
    // which is the most evidence it saw of this emission (ties keep the earlier one, so the choice
    // is deterministic and blind). Kept as an owned copy of that burst's samples alone, bounded by
    // one burst, because `buf` is drained as the chain advances.
    // T-844: with the CFAR detection that burst came from, which the C38 gate needs.
    let mut best: Option<(
        Vec<Complex<i8>>,
        SampleTime,
        ProvenanceHandle,
        SnippetRequest,
        DetectionId,
    )> = None;
    // Bursts already offered to burst taps (T-060) and when framing was last inferred for them.
    let mut streamed = 0usize;
    let mut last_stream: Option<Instant> = None;
    loop {
        loop {
            match rx.try_recv() {
                Ok(ChainMsg::Member(m)) => add_member(&mut groups, m, &c.fsk_boxes_missed),
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
                    cr.release_to(ch.end_sample());
                    if buf.len() > 2 * retain {
                        let cut = buf.len() - retain;
                        buf.drain(..cut);
                        base += cut as u64;
                        base_time = base_time.saturating_add_nanos((cut as f64 * 1e9 / fs) as i64);
                    }
                }
                Next::Lost => buf.clear(),
                Next::Idle => {}
                Next::Closed => closed = true,
            }
        } else if !detach {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(ChainMsg::Member(m)) => add_member(&mut groups, m, &c.fsk_boxes_missed),
                Ok(ChainMsg::Detach) | Err(RecvTimeoutError::Disconnected) => detach = true,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }

        let buf_end = base + buf.len() as u64;
        let complete = if detach {
            groups.len()
        } else {
            groups.len().saturating_sub(1)
        };
        let mut done = 0;
        for g in groups.iter().take(complete) {
            if g.end + pad > buf_end && !closed {
                break;
            }
            done += 1;
            let s = g.start.saturating_sub(pad);
            let e = (g.end + pad).min(buf_end);
            let Some(p) = prov.as_ref() else {
                inc(&c.fsk_boxes_missed);
                continue;
            };
            if s < base || e <= g.end || buf.is_empty() {
                inc(&c.fsk_boxes_missed);
                continue;
            }
            let slice = &buf[(s - base) as usize..(e - base) as usize];
            let info = InputInfo {
                time: SampleTime {
                    sample_index: s,
                    host_time: base_time
                        .saturating_add_nanos(((s - base) as f64 * 1e9 / fs) as i64),
                },
                discontinuity: Discontinuity::NONE,
                dropped_before: 0,
                provenance: p,
            };
            let request = SnippetRequest {
                start_index: g.start,
                end_index: g.end,
                center_offset_hz: 0.5 * (g.f_lo + g.f_hi) - p.tune.center_hz,
                bandwidth_hz: (g.f_hi - g.f_lo).max(1.0),
            };
            match receiver.run(info, slice, &request, &priors) {
                Ok(b) => {
                    inc(&c.fsk_bursts);
                    f_lo = f_lo.min(g.f_lo);
                    f_hi = f_hi.max(g.f_hi);
                    first_det.get_or_insert(g.detection);
                    if best.as_ref().is_none_or(|(s, ..)| s.len() < slice.len()) {
                        best = Some((slice.to_vec(), info.time, p.clone(), request, g.detection));
                    }
                    bursts.push(b);
                }
                Err(_) => inc(&c.errors),
            }
        }
        groups.drain(..done);
        if !detach
            && streamed < bursts.len()
            && bursts.len() >= min_bursts.max(1)
            && shared.bursts.has_taps()
            && last_stream.is_none_or(|t| t.elapsed() >= STREAM_INFER_INTERVAL)
        {
            stream_live(&shared, &bursts, streamed, f_lo, f_hi);
            streamed = bursts.len();
            last_stream = Some(Instant::now());
        }
        if bursts.len() >= max_bursts || (detach && groups.is_empty()) {
            break;
        }
        if detach && closed && !groups.is_empty() && done == 0 {
            // Samples for the remaining boxes will never arrive.
            add(&c.fsk_boxes_missed, groups.len() as u64);
            break;
        }
    }
    drop(cr);
    if bursts.len() < min_bursts.max(1) {
        return;
    }
    let bits: Vec<&[u8]> = bursts.iter().map(FskBurst::bits).collect();
    let result = infer_framing(&bits, &FramingConfig::default());
    let crc_valid = result
        .frames
        .iter()
        .filter(|f| f.crc_valid == Some(true))
        .count() as u64;
    add(&c.crc_valid, crc_valid);
    let classification = classify_emitter(
        &shared.cfg.settings.classify,
        shared.cfg.source_class,
        f_lo,
        f_hi,
    )
    .map(|(content_class, by)| EmitterClassification { content_class, by });
    let ctx = FramedRecordContext {
        // Only a stored detection: the writer thread may lag (see `stored_detection`).
        detection_ref: super::stored_detection(&shared, first_det),
        classification,
        ..Default::default()
    };
    // T-844: the C38 shadow stage, when an operator put a model in a non-`off` mode. The best
    // burst's detection is waited for like the chain's own (`stored_detection`), but only then —
    // an idle stage adds no wait to the chain.
    let ml = shared.ml.as_deref().filter(|m| !m.is_idle());
    let ml_detection = ml
        .and_then(|_| best.as_ref())
        .and_then(|b| super::stored_detection(&shared, Some(b.4)));
    let mut shadow: Option<(hk_model::classify::Classification, Option<Vec<f32>>)> = None;
    let written = {
        let mut repo = shared.repo();
        match write_framed_bursts(&mut repo, &bursts, &result, &ctx) {
            Ok(w) => {
                add(&c.demodulations, w.demodulation_ids.len() as u64);
                add(&c.decodes, w.decode_ids.len() as u64);
                shared
                    .track_decodes
                    .add(cand.track, w.decode_ids.len() as u64);
                add(&c.content_withheld, w.content_withheld as u64);
                add(&c.emitters_created, u64::from(w.emitter_created));
                // ADR-0016 §2 (T-247): this chain recovered a clock (C14) and framed the bursts
                // with a CRC that checked, so its label is **lock-verified**, not a pre-sync
                // guess. Its row went in through the legacy path, which records no lock and so
                // derives rank 3 — the classifier's own rank, where "latest among equals" would
                // let the C15 row below take `2fsk` off the emitter. Stating the rank keeps the
                // chain's label and leaves rank 3 free for the posterior.
                if crc_valid > 0
                    && crate::classify::record_locked_chain_label(&mut repo, w.emitter_id).is_err()
                {
                    inc(&c.errors);
                }
                // T-247: the C15 cascade, once per chain write, on this chain's own thread and its
                // own copy of the samples (`crate::classify::classify_and_record` states the
                // bound). Without this call site a run wrote no posterior, no open-set score and
                // no `unknown`, and ADR-0016 §7's classification floors could not be measured
                // through the device at all.
                if let Some((iq, time, prov, request, _)) = &best {
                    let info = InputInfo {
                        time: *time,
                        discontinuity: Discontinuity::NONE,
                        dropped_before: 0,
                        provenance: prov,
                    };
                    let mut c14 = SymbolEstimator::new();
                    match crate::classify::classify_and_record_observed(
                        &mut repo,
                        w.emitter_id,
                        &Classifier::new(),
                        &mut c14,
                        // T-399: the receiver-line survey measured by the `hk-survey` reader, once
                        // for this capture state. Read here, never measured here.
                        shared.receiver.as_ref(),
                        info,
                        iq,
                        request,
                        time.host_time,
                        ml,
                    ) {
                        Ok(Some((classification, written, input))) => {
                            if written {
                                inc(&c.classifications);
                            }
                            shadow = Some((classification, input));
                        }
                        Ok(None) => {}
                        Err(_) => inc(&c.errors),
                    }
                }
                let mut inv = shared
                    .inventory
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // Named, not just counted (T-293): this was the only chain write path that
                // incremented the error counter silently, so a run summary reported "2 errors"
                // with nothing to read. An error nobody can name trains everyone to ignore the
                // count.
                if let Err(e) = inv.chain_emitter(&mut repo, cand.track, w.emitter_id) {
                    inc(&c.errors);
                    eprintln!("hk-pipeline: fsk chain emitter: {e}");
                }
                Some(w)
            }
            Err(e) => {
                inc(&c.errors);
                eprintln!("hk-pipeline: fsk chain write: {e}");
                None
            }
        }
    };
    // T-844: observed after the repository lock is released — the host batches, and a batch must
    // never stall the other writers. The published row above is final; this only records what a
    // model in shadow said about it (`crate::ml`).
    if let (Some(ml), Some((classification, input))) = (ml, &shadow) {
        let detection = ml_detection.and_then(|d| shared.repo().detection(d).ok());
        ml.observe(detection.as_ref(), classification, input.as_deref());
    }
    if let Some(w) = written {
        publish_bits(&shared, &bursts, &w, f_lo, f_hi);
        // Burst taps get the bursts not offered live, under the stored class and emitter.
        shared.bursts.offer(
            &shared.counters,
            &bursts,
            streamed,
            &result,
            w.content_class,
            Some(w.emitter_id),
        );
    }
}

/// Offers `bursts[from..]` to burst taps while the chain runs (T-060): framing inferred over
/// every burst so far, class as the chain will store it ([`effective_content_class`]).
fn stream_live(shared: &Shared, bursts: &[FskBurst], from: usize, f_lo: f64, f_hi: f64) {
    let bits: Vec<&[u8]> = bursts.iter().map(FskBurst::bits).collect();
    let result = infer_framing(&bits, &FramingConfig::default());
    inc(&shared.counters.taps.inferences);
    let ctx = FramedRecordContext {
        classification: classify_emitter(
            &shared.cfg.settings.classify,
            shared.cfg.source_class,
            f_lo,
            f_hi,
        )
        .map(|(content_class, by)| EmitterClassification { content_class, by }),
        ..Default::default()
    };
    let class = effective_content_class(&ctx, &result);
    shared
        .bursts
        .offer(&shared.counters, bursts, from, &result, class, None);
}

/// The FSK bits stream (T-037b): one record per demodulated burst on
/// `bits/fsk-bursts/<emitter>`, hard bits as one `u8` (0 or 1) per bit (`datatype` `ru8`), timed
/// at the burst's first sample, with the Bitstream descriptor's framing. The header class is the
/// class the Decode rows were stored under (`classify_emitter` already applied the source class:
/// a restricted source stays restricted, and only a user rule lifts `metadata-only`), so the
/// stream never carries more than the repository kept. The publisher gates like every other
/// content stream (T-016): under a class that forbids content each record goes out header-only
/// (`GATED`: timing, seq and length only) and is counted in `bits_gated`. Written after the rows
/// are stored and outside the repository lock.
fn publish_bits(shared: &Shared, bursts: &[FskBurst], w: &WrittenFraming, f_lo: f64, f_hi: f64) {
    let c = &shared.counters.chains;
    let class = w.content_class;
    let mut header = StreamHeader::new(
        format!("bits/fsk-bursts/{}", w.emitter_id),
        StreamKind::Bits,
        class,
        "hk-pipeline:fsk-bursts",
    );
    header.datatype = Some("ru8".into());
    header.center_hz = Some(0.5 * (f_lo + f_hi));
    header.bandwidth_hz = Some((f_hi - f_lo).max(0.0));
    header.emitter_id = Some(w.emitter_id);
    if let Some(b) = &w.bitstream {
        header.bitstream_id = Some(b.id);
        header.framing = Some(b.framing.clone());
    }
    let longest = bursts.iter().map(|b| b.bits().len()).max().unwrap_or(0);
    header.max_frame_len = header.max_frame_len.max((longest + 256) as u32);
    let mut publisher = match Publisher::new(header.clone(), PublisherConfig::default()) {
        Ok(p) => p,
        Err(e) => {
            inc(&c.errors);
            eprintln!("hk-pipeline: fsk bits stream: {e}");
            return;
        }
    };
    if let Some(sink) = &shared.cfg.stream_sink {
        sink(&header, publisher.handle());
    }
    for b in bursts {
        let bits = b.bits();
        if bits.is_empty() {
            continue;
        }
        let record = BinaryRecord {
            t: b.timestamp_of_source(b.request.start_index as f64),
            sample_index: b.request.start_index,
            flags: RecordFlags::empty(),
            payload: bits,
        };
        match publisher.publish_binary(record) {
            Ok(_) => inc(&c.bits_records),
            Err(StreamError::ContentGated { .. }) => inc(&c.bits_gated),
            Err(_) => inc(&c.errors),
        }
    }
    publisher.finish();
}
