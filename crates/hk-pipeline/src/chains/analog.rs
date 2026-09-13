//! Analog auto-mode chain (C19, SIGNAL-062): collect a contiguous window from the track's
//! pre-trigger start, run `AnalogReceiver` (estimate → mode selection → WFM + RDS when the mode
//! is WFM), write the Demodulation, Decode rows and the Emitter identity/label
//! (`hk_demod::write_session`). Attached only under a class that permits content.
//!
//! **Probe.** Mode selection first runs on the leading `probe_s` of the window. The chain
//! continues to the full window only when the selected mode is in `accept_modes` (and a pilot
//! was found when `require_pilot`); otherwise it stops without writing anything
//! (`mode_rejected`). Mode selection, not the spec, decides what is demodulated.

use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::thread;

use hk_core::{Discontinuity, ProvenanceHandle};
use hk_demod::{AnalogReceiver, AnalogSession, RecordContext, write_session};
use hk_dsp::InputInfo;
use hk_estimate::SnippetRequest;
use hk_model::SampleTime;
use num_complex::Complex;

use hk_model::RecordingTrigger;

use super::{ChainMsg, ChainReader, Next};
use crate::events::Candidate;
use crate::gate::GateCursor;
use crate::run::Shared;
use crate::stats::{add, inc};

/// The analog node's settings.
pub(crate) struct AnalogNode {
    pub pre_s: f64,
    pub window_s: f64,
    pub bandwidth_hz: f64,
    pub probe_s: f64,
    pub accept_modes: Vec<String>,
    pub require_pilot: bool,
    /// Largest distance of the locked emission from the channel centre, Hz: half the spec's
    /// raster step (so a neighbour's chain owns a station between two raster channels), or
    /// unbounded off a raster. Half the bandwidth also applies.
    pub channel_tolerance_hz: f64,
    /// The spec's record node, run once the probe accepts the channel.
    pub record: Option<RecordAfterProbe>,
}

/// A deferred pre-trigger recording.
pub(crate) struct RecordAfterProbe {
    pub pre_s: f64,
    pub post_s: f64,
    pub trigger: RecordingTrigger,
    pub label: String,
}

struct Window {
    iq: Vec<Complex<i8>>,
    head: Option<(SampleTime, ProvenanceHandle)>,
    ended: bool,
    /// A `Detach` arrived; it ends collection as soon as the window holds samples (kept pending
    /// while it is still empty, never dropped).
    detached: bool,
}

/// Collects until `iq` holds `target` samples, the stream ends, a discontinuity arrives, or the
/// chain is detached.
fn collect(cr: &mut ChainReader, rx: &Receiver<ChainMsg>, w: &mut Window, target: usize) {
    while w.iq.len() < target && !w.ended {
        while !w.detached {
            match rx.try_recv() {
                Ok(ChainMsg::Detach) => w.detached = true,
                Ok(ChainMsg::Member(_)) => {}
                Err(_) => break,
            }
        }
        if w.detached && !w.iq.is_empty() {
            w.ended = true;
            break;
        }
        match cr.next() {
            Next::Data(c) => {
                if let Some((t, p)) = &w.head {
                    if c.first_sample() != t.sample_index + w.iq.len() as u64
                        || c.provenance.id() != p.id()
                    {
                        w.ended = true;
                        break;
                    }
                } else {
                    w.head = Some((c.time, c.provenance.clone()));
                }
                let take = (target - w.iq.len()).min(c.len);
                w.iq.extend_from_slice(&cr.buf[..take]);
                cr.release_to(c.end_sample());
            }
            Next::Lost => {
                if !w.iq.is_empty() {
                    w.ended = true;
                }
            }
            Next::Idle => {}
            Next::Closed => w.ended = true,
        }
    }
}

fn demodulate(
    w: &Window,
    len: usize,
    cand: &Candidate,
    bandwidth_hz: f64,
) -> Option<Result<AnalogSession, hk_demod::DemodError>> {
    let (time, prov) = w.head.as_ref()?;
    let info = InputInfo {
        time: *time,
        discontinuity: Discontinuity::NONE,
        dropped_before: 0,
        provenance: prov,
    };
    let center = 0.5 * (cand.f_lo_hz + cand.f_hi_hz);
    let request = SnippetRequest {
        start_index: time.sample_index,
        end_index: time.sample_index + len as u64,
        center_offset_hz: center - prov.tune.center_hz,
        bandwidth_hz,
    };
    Some(AnalogReceiver::default().run(info, &w.iq[..len], &request))
}

fn mode_name(s: &AnalogSession) -> String {
    format!("{:?}", s.mode.mode).to_lowercase()
}

pub(crate) fn run(
    shared: Arc<Shared>,
    rx: Receiver<ChainMsg>,
    cand: Candidate,
    mut node: AnalogNode,
    cursor: GateCursor,
) {
    let fs = shared.fs;
    let c = &shared.counters.chains;
    let start = cand.first_sample.saturating_sub((node.pre_s * fs) as u64);
    let want = (node.window_s * fs) as usize;
    let probe = ((node.probe_s * fs) as usize).min(want);
    let mut cr = ChainReader::new(Arc::clone(&shared), start, cursor);
    let mut w = Window {
        iq: Vec::with_capacity(want.min(1 << 27)),
        head: None,
        ended: false,
        detached: false,
    };
    if probe > 0 && probe < want {
        collect(&mut cr, &rx, &mut w, probe);
        if w.iq.len() < probe {
            return;
        }
        let accepted = match demodulate(&w, probe, &cand, node.bandwidth_hz) {
            Some(Ok(s)) => {
                let mode_ok =
                    node.accept_modes.is_empty() || node.accept_modes.contains(&mode_name(&s));
                let pilot_ok =
                    !node.require_pilot || s.mode.features.pilot.as_ref().is_some_and(|p| p.found);
                // The receiver's CFO estimate can slide onto a strong neighbour: the emission
                // it locked to must lie in this channel, or the neighbour's own chain owns it.
                let channel_center = 0.5 * (cand.f_lo_hz + cand.f_hi_hz);
                let in_channel = (s.rf_center_hz - channel_center).abs()
                    <= (0.5 * node.bandwidth_hz).min(node.channel_tolerance_hz);
                let accepted = mode_ok && pilot_ok && in_channel;
                if crate::debug_enabled() {
                    eprintln!(
                        "hk-pipeline: analog probe {:.4} MHz (channel {:.4}): mode {} ({:.2}), pilot {:?} → {}",
                        s.rf_center_hz / 1e6,
                        channel_center / 1e6,
                        mode_name(&s),
                        s.mode.confidence,
                        s.mode.features.pilot.as_ref().map(|p| p.found),
                        if accepted { "continue" } else { "stop" }
                    );
                }
                accepted
            }
            _ => false,
        };
        if !accepted {
            inc(&c.mode_rejected);
            return;
        }
    }
    // The recording runs beside the window collection, never inline: in lossless replay this
    // chain's gate cursor would otherwise stay parked at the probe end while the recorder waits
    // for samples up to trigger + post, and the capture thread could not advance past the slack.
    // Its cursor is claimed here, before this chain releases more history.
    let recorder = node.record.take().and_then(|r| {
        let cursor = super::record::claim(&shared, cand.trigger_sample, r.pre_s);
        let s = Arc::clone(&shared);
        let at = cand.trigger_sample;
        let spawned = thread::Builder::new()
            .name("hk-rec-analog".into())
            .spawn(move || {
                super::record::run_claimed(
                    s,
                    Some(cursor),
                    at,
                    r.pre_s,
                    r.post_s,
                    r.trigger,
                    r.label,
                )
            });
        match spawned {
            Ok(join) => Some(join),
            Err(_) => {
                inc(&c.errors);
                None
            }
        }
    });
    collect_and_write(&shared, &rx, cr, w, &cand, &node, want);
    if let Some(join) = recorder {
        let _ = join.join();
    }
}

/// Collects the rest of the window, demodulates and writes the session.
fn collect_and_write(
    shared: &Arc<Shared>,
    rx: &Receiver<ChainMsg>,
    mut cr: ChainReader,
    mut w: Window,
    cand: &Candidate,
    node: &AnalogNode,
    want: usize,
) {
    let fs = shared.fs;
    let c = &shared.counters.chains;
    collect(&mut cr, rx, &mut w, want);
    drop(cr);
    if (w.iq.len() as f64) < 0.25 * fs {
        return;
    }
    let session = match demodulate(&w, w.iq.len(), cand, node.bandwidth_hz) {
        Some(Ok(s)) => s,
        Some(Err(e)) => {
            inc(&c.errors);
            eprintln!("hk-pipeline: analog chain: {e}");
            return;
        }
        None => return,
    };
    if crate::debug_enabled() {
        eprintln!(
            "hk-pipeline: analog chain {:.4} MHz: {} samples, mode {} ({:.2}), pilot {:?}, RDS PI {:?}",
            session.rf_center_hz / 1e6,
            w.iq.len(),
            mode_name(&session),
            session.mode.confidence,
            session
                .wfm
                .as_ref()
                .map(|x| (x.pilot.present, x.pilot.frequency_hz)),
            session.rds().and_then(|r| r.pi.as_ref().map(|p| p.hex())),
        );
    }
    let ctx = RecordContext {
        recording_ref: None,
        detection_ref: cand.detection,
        emitter_hint: None,
    };
    let mut repo = shared.repo();
    match write_session(&mut repo, &session, &ctx) {
        Ok(written) => {
            inc(&c.demodulations);
            add(&c.decodes, written.decode_ids.len() as u64);
            add(&c.emitters_created, u64::from(written.emitter_created));
            add(&c.labels, u64::from(written.label.is_some()));
            if let Some(e) = written.emitter_id {
                let mut inv = shared
                    .inventory
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if inv.chain_emitter(&mut repo, cand.track, e).is_err() {
                    inc(&c.errors);
                }
            }
        }
        Err(e) => {
            inc(&c.errors);
            eprintln!("hk-pipeline: analog chain write: {e}");
        }
    }
}
