//! Analog auto-mode chain (C19, SIGNAL-062): collect a contiguous window from the track's
//! pre-trigger start, run `AnalogReceiver` (estimate → mode selection → WFM + RDS when the mode
//! is WFM), write the Demodulation, Decode rows and the Emitter identity/label
//! (`hk_demod::write_session`). Attached only under a class that permits content.
//!
//! **Probe.** Mode selection first runs on the leading `probe_s` of the window. The chain
//! continues to the full window only when the selected mode is in `accept_modes` (and a pilot
//! was found when `require_pilot`); otherwise it stops without writing anything
//! (`mode_rejected`). Mode selection, not the spec, decides what is demodulated.
//!
//! **Refinement (T-070).** When the probe accepted a mode with an objective
//! ([`crate::refine`]), the collected window refines the channel from the demodulated output,
//! starting from the channel the chain was attached to. The refined tuning is demodulated instead
//! of the attach box and stored on the written emitter before its explanations are ranked.
//! A probe that finds the mode outside this channel refines first and decides by the refined
//! centre ([`owns`]): the chain takes an emission inside its channel, or one off every raster
//! channel less than a raster step away, so a station between two channels is demodulated where it
//! is instead of by neither neighbour.

use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::thread;

use hk_core::{Discontinuity, ProvenanceHandle};
use hk_demod::refine::{IqWindow, RefineStart, RefinementOutcome, Tuning};
use hk_demod::{
    AnalogMode, AnalogReceiver, AnalogSession, ReceiverConfig, RecordContext, write_session,
};
use hk_dsp::InputInfo;
use hk_estimate::SnippetRequest;
use hk_model::SampleTime;
use num_complex::Complex;

use hk_model::RecordingTrigger;

use super::{ChainMsg, ChainReader, Next};
use crate::events::Candidate;
use crate::gate::GateCursor;
use crate::refine::{RefineSettings, SOURCE_ANALOG_CHAIN};
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
    /// The chain's id: its claims on emissions ([`super::EmissionClaims`], T-071).
    pub owner: u64,
}

/// Releases the chain's emission claim when it finishes (T-071).
struct ClaimRelease<'a> {
    shared: &'a Shared,
    owner: u64,
}

impl Drop for ClaimRelease<'_> {
    fn drop(&mut self) {
        self.shared.claims.release(
            self.owner,
            self.shared.ring.next_sample().unwrap_or(0),
            (super::CHANNEL_COOLDOWN_S * self.shared.fs) as u64,
        );
    }
}

/// Claims the emission at `center_hz` for this chain (T-071 dedupe): ranked by its distance from
/// the chain's channel, so the nearer channel's chain owns a station both neighbours refined to.
fn claim_emission(shared: &Shared, node: &AnalogNode, channel_center: f64, center_hz: f64) -> bool {
    let half = (0.5 * node.bandwidth_hz).min(node.channel_tolerance_hz);
    let ok = shared.claims.claim(
        node.owner,
        center_hz,
        half,
        (center_hz - channel_center).abs(),
        shared.ring.next_sample().unwrap_or(0),
    );
    if !ok {
        inc(&shared.counters.chains.duplicate_emission);
        if crate::debug_enabled() {
            eprintln!(
                "hk-pipeline: analog chain {} (channel {:.4} MHz): {:.4} MHz is owned by a \
                 neighbouring chain",
                node.owner,
                channel_center / 1e6,
                center_hz / 1e6
            );
        }
    }
    ok
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

/// Demodulates the window's first `len` samples on the attach box, or on a `refined` tuning (its
/// centre and WFM channel bandwidth, no further CFO correction).
fn demodulate(
    w: &Window,
    len: usize,
    cand: &Candidate,
    bandwidth_hz: f64,
    refined: Option<&Tuning>,
) -> Option<Result<AnalogSession, hk_demod::DemodError>> {
    let (time, prov) = w.head.as_ref()?;
    let info = InputInfo {
        time: *time,
        discontinuity: Discontinuity::NONE,
        dropped_before: 0,
        provenance: prov,
    };
    let center = refined.map_or(0.5 * (cand.f_lo_hz + cand.f_hi_hz), |t| t.center_hz);
    let request = SnippetRequest {
        start_index: time.sample_index,
        end_index: time.sample_index + len as u64,
        center_offset_hz: center - prov.tune.center_hz,
        bandwidth_hz,
    };
    let mut receiver = match refined {
        Some(t) => AnalogReceiver::new(ReceiverConfig {
            wfm_channel_bandwidth_hz: t.bandwidth_hz,
            max_cfo_correction_hz: 0.0,
            ..ReceiverConfig::default()
        }),
        None => AnalogReceiver::default(),
    };
    Some(receiver.run(info, &w.iq[..len], &request))
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
    let _claim = ClaimRelease {
        shared: &shared,
        owner: node.owner,
    };
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
    let mut probe_mode = None;
    let mut probe_refined = None;
    if probe > 0 && probe < want {
        collect(&mut cr, &rx, &mut w, probe);
        if w.iq.len() < probe {
            return;
        }
        let accepted = match demodulate(&w, probe, &cand, node.bandwidth_hz, None) {
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
                let mut accepted = mode_ok && pilot_ok && in_channel;
                // T-070: an emission outside this channel may sit off every raster channel, where
                // no chain's channel holds it. Refine from where the probe found it and decide by
                // the refined centre ([`owns`]).
                if mode_ok && pilot_ok && !in_channel {
                    probe_refined =
                        refine_window(&w, probe, s.rf_center_hz, node.bandwidth_hz, s.mode.mode)
                            .filter(|o| owns(&node, channel_center, o.tuning.center_hz));
                    accepted = probe_refined.as_ref().is_some_and(|o| {
                        claim_emission(&shared, &node, channel_center, o.tuning.center_hz)
                    });
                }
                if crate::debug_enabled() {
                    eprintln!(
                        "hk-pipeline: analog probe {:.4} MHz (channel {:.4}): mode {} ({:.2}), pilot {:?} → {} (OBW99 {:?} Hz, {:?})",
                        s.rf_center_hz / 1e6,
                        channel_center / 1e6,
                        mode_name(&s),
                        s.mode.confidence,
                        s.mode.features.pilot.as_ref().map(|p| p.found),
                        if accepted { "continue" } else { "stop" },
                        s.mode.features.obw99_hz,
                        s.mode.reason
                    );
                }
                if accepted {
                    probe_mode = Some(s.mode.mode);
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
    collect_and_write(
        &shared,
        &rx,
        cr,
        w,
        &cand,
        &node,
        want,
        probe_mode,
        probe_refined,
    );
    if let Some(join) = recorder {
        let _ = join.join();
    }
}

/// Largest distance from the nearest raster channel still read as on that channel, as a fraction
/// of the raster step (the explanations' US FM raster tolerance is also 10 %).
const ON_RASTER_FRACTION: f64 = 0.1;

/// Whether the emission refined to `refined_hz` belongs to this chain (T-070): inside its channel
/// (half a raster step and half the node bandwidth), or off every raster channel and less than one
/// raster step away. A station between two channels would otherwise belong to neither neighbour's
/// chain and never be demodulated; either neighbour may take it (a second demodulation of the same
/// station is a known duplicate, not a loss).
fn owns(node: &AnalogNode, channel_center: f64, refined_hz: f64) -> bool {
    let d = refined_hz - channel_center;
    if d.abs() <= (0.5 * node.bandwidth_hz).min(node.channel_tolerance_hz) {
        return true;
    }
    if !node.channel_tolerance_hz.is_finite() {
        return false;
    }
    let raster = 2.0 * node.channel_tolerance_hz;
    let nearest = channel_center + (d / raster).round() * raster;
    (refined_hz - nearest).abs() > ON_RASTER_FRACTION * raster && d.abs() < raster
}

/// Refines `mode` on the window's first `len` samples from a box at `center_hz` / `bandwidth_hz`
/// (T-070); the locked result, else `None`.
fn refine_window(
    w: &Window,
    len: usize,
    center_hz: f64,
    bandwidth_hz: f64,
    mode: AnalogMode,
) -> Option<RefinementOutcome> {
    let (time, prov) = w.head.as_ref()?;
    let info = InputInfo {
        time: *time,
        discontinuity: Discontinuity::NONE,
        dropped_before: 0,
        provenance: prov,
    };
    let start = RefineStart {
        center_hz,
        bandwidth_hz: bandwidth_hz.max(0.0),
        warm: false,
    };
    let o = crate::refine::refine(
        &RefineSettings::default(),
        mode,
        IqWindow::new(info, &w.iq[..len.min(w.iq.len())]),
        &start,
    )?;
    if crate::debug_enabled() {
        eprintln!(
            "hk-pipeline: analog refine {:.4} MHz -> {:.4} MHz / {:.1} kHz, locked {}, quality \
             {:.1}, {} iterations, {} evaluations, {:.2} s",
            center_hz / 1e6,
            o.tuning.center_hz / 1e6,
            o.tuning.bandwidth_hz / 1e3,
            o.locked,
            o.quality,
            o.iterations,
            o.evaluations,
            o.elapsed_s
        );
    }
    o.locked.then_some(o)
}

/// Collects the rest of the window, refines, demodulates and writes the session. The refinement
/// starts from the probe's refined result when the probe needed one, else from the attach channel,
/// and must stay owned by this chain.
#[allow(clippy::too_many_arguments)]
fn collect_and_write(
    shared: &Arc<Shared>,
    rx: &Receiver<ChainMsg>,
    mut cr: ChainReader,
    mut w: Window,
    cand: &Candidate,
    node: &AnalogNode,
    want: usize,
    probe_mode: Option<AnalogMode>,
    probe_refined: Option<RefinementOutcome>,
) {
    let fs = shared.fs;
    let c = &shared.counters.chains;
    collect(&mut cr, rx, &mut w, want);
    drop(cr);
    if (w.iq.len() as f64) < 0.25 * fs {
        return;
    }
    let channel_center = 0.5 * (cand.f_lo_hz + cand.f_hi_hz);
    let refined = probe_mode
        .and_then(|m| {
            let (start, width) = probe_refined
                .as_ref()
                .map_or((channel_center, cand.f_hi_hz - cand.f_lo_hz), |p| {
                    (p.tuning.center_hz, 0.0)
                });
            refine_window(&w, w.iq.len(), start, width, m).or_else(|| probe_refined.clone())
        })
        .filter(|o| owns(node, channel_center, o.tuning.center_hz));
    // T-071: one chain per emission, even when a neighbour's chain refined to it too.
    let emission = refined
        .as_ref()
        .map_or(channel_center, |o| o.tuning.center_hz);
    if !claim_emission(shared, node, channel_center, emission) {
        return;
    }
    let session = match demodulate(
        &w,
        w.iq.len(),
        cand,
        node.bandwidth_hz,
        refined.as_ref().map(|o| &o.tuning),
    ) {
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
        // Only a stored detection: the writer thread may lag (see `stored_detection`).
        detection_ref: super::stored_detection(shared, cand.detection),
        emitter_hint: None,
    };
    if !shared.claims.commit(node.owner) {
        // Preempted by the chain whose channel is nearer the emission.
        inc(&c.duplicate_emission);
        return;
    }
    let mut repo = shared.repo();
    match write_session(&mut repo, &session, &ctx) {
        Ok(written) => {
            inc(&c.demodulations);
            add(&c.decodes, written.decode_ids.len() as u64);
            shared
                .track_decodes
                .add(cand.track, written.decode_ids.len() as u64);
            add(&c.emitters_created, u64::from(written.emitter_created));
            add(&c.labels, u64::from(written.label.is_some()));
            if let Some(e) = written.emitter_id {
                if let Some(o) = &refined
                    && let Err(err) = crate::refine::persist(
                        &mut repo,
                        e,
                        o,
                        SOURCE_ANALOG_CHAIN,
                        session.time_range().start,
                    )
                {
                    inc(&c.errors);
                    eprintln!("hk-pipeline: analog chain refined tuning write: {err}");
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn node(raster_hz: f64) -> AnalogNode {
        AnalogNode {
            pre_s: 0.0,
            window_s: 1.0,
            bandwidth_hz: 200e3,
            probe_s: 0.5,
            accept_modes: Vec::new(),
            require_pilot: false,
            channel_tolerance_hz: 0.5 * raster_hz,
            record: None,
            owner: 0,
        }
    }

    #[test]
    fn a_chain_owns_its_channel_and_off_raster_stations_beside_it_not_on_raster_neighbours() {
        let n = node(200e3);
        let ch = 101.3e6;
        assert!(owns(&n, ch, 101.302e6), "in channel");
        assert!(
            owns(&n, ch, 101.4495e6),
            "150 kHz off: off raster, beside it"
        );
        assert!(owns(&n, ch, 101.1505e6), "off raster below");
        assert!(!owns(&n, ch, 101.5e6), "on the neighbour's raster channel");
        assert!(
            !owns(&n, ch, 101.49e6),
            "within the neighbour's raster tolerance"
        );
        assert!(!owns(&n, ch, 101.55e6), "a raster step or more away");
        let unbounded = node(f64::INFINITY);
        assert!(owns(&unbounded, ch, 101.39e6));
        assert!(
            !owns(&unbounded, ch, 101.45e6),
            "beyond half the bandwidth off a raster"
        );
    }
}
