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
//! not restricted ([`crate::class::classify_emitter`]).

use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::Duration;

use hk_core::Discontinuity;
use hk_core::ProvenanceHandle;
use hk_demod::fsk::{
    DemodPriors, EmitterClassification, FramedRecordContext, FskBurst, FskReceiver,
    FskReceiverConfig, write_framed_bursts,
};
use hk_dsp::InputInfo;
use hk_estimate::SnippetRequest;
use hk_estimate::framing::{FramingConfig, infer_framing};
use hk_model::{DetectionId, SampleTime, Timestamp};
use num_complex::Complex;

use super::{ChainMsg, ChainReader, Next};
use crate::class::classify_emitter;
use crate::events::{Candidate, MemberBox};
use crate::run::Shared;
use crate::stats::{add, inc};

#[derive(Clone, Debug)]
struct Group {
    start: u64,
    end: u64,
    f_lo: f64,
    f_hi: f64,
    detection: DetectionId,
}

fn add_member(groups: &mut Vec<Group>, m: MemberBox) {
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
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    shared: Arc<Shared>,
    rx: Receiver<ChainMsg>,
    cand: Candidate,
    pad_s: f64,
    retain_s: f64,
    min_bursts: usize,
    max_bursts: usize,
) {
    let fs = shared.fs;
    let c = &shared.counters.chains;
    let pad = (pad_s * fs) as u64;
    let retain = ((retain_s * fs) as usize).max(1);
    let mut cr = ChainReader::new(Arc::clone(&shared), cand.first_sample.saturating_sub(pad));
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
    loop {
        loop {
            match rx.try_recv() {
                Ok(ChainMsg::Member(m)) => add_member(&mut groups, m),
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
                Ok(ChainMsg::Member(m)) => add_member(&mut groups, m),
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
                    bursts.push(b);
                }
                Err(_) => inc(&c.errors),
            }
        }
        groups.drain(..done);
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
    add(
        &c.crc_valid,
        result
            .frames
            .iter()
            .filter(|f| f.crc_valid == Some(true))
            .count() as u64,
    );
    let classification = classify_emitter(
        &shared.cfg.settings.classify,
        shared.cfg.source_class,
        f_lo,
        f_hi,
    )
    .map(|(content_class, by)| EmitterClassification { content_class, by });
    let ctx = FramedRecordContext {
        detection_ref: first_det,
        classification,
        ..Default::default()
    };
    let mut repo = shared.repo();
    match write_framed_bursts(&mut repo, &bursts, &result, &ctx) {
        Ok(w) => {
            add(&c.demodulations, w.demodulation_ids.len() as u64);
            add(&c.decodes, w.decode_ids.len() as u64);
            add(&c.content_withheld, w.content_withheld as u64);
            add(&c.emitters_created, u64::from(w.emitter_created));
            let mut inv = shared
                .inventory
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if inv
                .chain_emitter(&mut repo, cand.track, w.emitter_id)
                .is_err()
            {
                inc(&c.errors);
            }
        }
        Err(e) => {
            inc(&c.errors);
            eprintln!("hk-pipeline: fsk chain write: {e}");
        }
    }
}
