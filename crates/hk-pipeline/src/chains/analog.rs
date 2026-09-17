//! Analog auto-mode chain (C19, SIGNAL-062): collect a contiguous window from the track's
//! pre-trigger start, run `AnalogReceiver` (estimate → mode selection → WFM + RDS when the mode
//! is WFM), write the Demodulation, Decode rows and the Emitter identity/label
//! (`hk_demod::write_session`). Attached only under a class that permits content.
//!
//! **Probe.** Mode selection first runs on the leading `probe_s` of the window. The chain
//! continues to the full window only when the selected mode is in `accept_modes` (and a pilot
//! was found when `require_pilot`); otherwise it stops (`mode_rejected`). Mode selection, not the
//! spec, decides what is demodulated.
//!
//! **A declined probe still records what it measured (T-416).** It used to stop having written
//! nothing at all, which made "we looked at this window and it is not ours" indistinguishable from
//! "nothing ever looked here" — the distinction `Coverage::Unobserved` draws against
//! observed-and-quiet, and `BiasTee::Unknown` against off. [`log_declined`] writes the
//! Demodulation (`hk_demod::write_declined`) against the emission's inventory entry and nothing
//! else: no sighting, no classification, no identity, no decode, no label. A refusal is evidence,
//! not a claim, and route C of the confirmation rule refuses such a row on its own terms because
//! it carries neither a lock quality nor a pilot.
//!
//! **Refinement (T-070).** When the probe accepted a mode with an objective
//! ([`crate::refine`]), the collected window refines the channel from the demodulated output,
//! starting from the channel the chain was attached to. The refined tuning is demodulated instead
//! of the attach box and stored on the written emitter before its explanations are ranked.
//! A probe that finds the mode outside this channel refines first and decides by the refined
//! centre ([`owns`]): the chain takes an emission inside its channel, or one off every raster
//! channel less than a raster step away, so a station between two channels is demodulated where it
//! is instead of by neither neighbour.
//!
//! **Early identification (T-186).** RDS needs the full window, but the mode, the pilot and the
//! refined tuning do not: the refinement reads only the leading [`RefineSettings::window_s`]. When
//! the probe accepted a refinable mode, the chain first collects that leading window, refines,
//! demodulates it and, when the session is pilot-locked, writes it at once ([`identify`]): the
//! Demodulation, the mode Classification on the emitter, the refined tuning, then
//! `Inventory::chain_emitter` (merge, explanations, review). Its RDS is left out, so no decode is
//! written twice. The full window then reuses that refinement (same leading samples, same claim)
//! and writes as before with the early emitter as its hint, adding the RDS decodes and identity.
//! Without a decoded PI, a pilot-locked session over at least the leading window still places its
//! emitter through a sighting keyed by its Demodulation ([`mode_emitter`]), so a station with
//! weak RDS reaches the inventory too. Nothing is identified on less evidence than before: a
//! channel is only here after a confirmed, trusted track and an accepted probe, an early write
//! needs the whole leading window and a pilot lock, and lifecycle confirmation is unchanged (no
//! identity or decode is written early).
//!
//! **One session, one write of each (T-209).** The early and the full window are one
//! observation: the full window's PI sighting is offered as a re-measurement of the early
//! sighting (`RecordContext::counted_as`), so the emitter's count grows by one per session, and
//! the refined tuning the early write stored is not stored again. The leading window is
//! demodulated without RDS (its decodes would be discarded). **Overload gate:** a window whose
//! provenance is overloaded or whose samples clip beyond the detector's clip rule places no
//! emitter from mode evidence (an intermodulation image can lock a pilot); a decoded identity
//! still places one.

use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::thread;

use hk_core::{Discontinuity, ProvenanceHandle};
use hk_demod::refine::{IqWindow, RefineStart, RefinementOutcome, Tuning};
use hk_demod::{
    AnalogMode, AnalogReceiver, AnalogSession, ReceiverConfig, RecordContext, write_declined,
    write_session,
};
use hk_dsp::InputInfo;
use hk_estimate::SnippetRequest;
use hk_model::{
    Classification, EmitterId, EmitterLink, Fingerprint, LinkTarget, MeasurementKey, RepoError,
    Repository, SampleTime, Sighting,
};
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
/// centre and WFM channel bandwidth, no further CFO correction). `rds: false` skips the RDS
/// decoder (a session that is never written with its decodes).
fn demodulate(
    w: &Window,
    len: usize,
    cand: &Candidate,
    bandwidth_hz: f64,
    refined: Option<&Tuning>,
    rds: bool,
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
    let mut config = match refined {
        Some(t) => ReceiverConfig {
            wfm_channel_bandwidth_hz: t.bandwidth_hz,
            max_cfo_correction_hz: 0.0,
            ..ReceiverConfig::default()
        },
        None => ReceiverConfig::default(),
    };
    if !rds {
        config.wfm.rds = None;
    }
    Some(AnalogReceiver::new(config).run(info, &w.iq[..len], &request))
}

/// T-209: whether the window's front end was in compression over its first `len` samples: its
/// provenance is overloaded, or its clipped-sample fraction exceeds the detector's clip rule
/// (rule 8). A window is one provenance ([`collect`] ends it on a change).
fn front_end_overloaded(w: &Window, len: usize) -> bool {
    let Some((_, prov)) = w.head.as_ref() else {
        return false;
    };
    let iq = &w.iq[..len.min(w.iq.len())];
    prov.get().overload
        || (!iq.is_empty()
            && hk_detect::count_clipped_ci8(iq) as f64
                > hk_detect::Rules::default().clip_fraction * iq.len() as f64)
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
    let mut probe_center = None;
    if probe > 0 && probe < want {
        collect(&mut cr, &rx, &mut w, probe);
        if w.iq.len() < probe {
            return;
        }
        let probed = demodulate(&w, probe, &cand, node.bandwidth_hz, None, false);
        let accepted = match &probed {
            Some(Ok(s)) => {
                let mode_ok =
                    node.accept_modes.is_empty() || node.accept_modes.contains(&mode_name(s));
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
                    let refined =
                        refine_window(&w, probe, s.rf_center_hz, node.bandwidth_hz, s.mode.mode);
                    let emission = off_channel_emission(
                        &node,
                        channel_center,
                        s.rf_center_hz,
                        refined.as_ref(),
                    );
                    accepted =
                        emission.is_some_and(|c| claim_emission(&shared, &node, channel_center, c));
                    if accepted {
                        match refined {
                            Some(o) => probe_refined = Some(o),
                            // T-226: no validated refinement, so the full window refines from
                            // where the probe found the emission rather than from this channel.
                            None => probe_center = Some(s.rf_center_hz),
                        }
                    }
                }
                if crate::debug_enabled() {
                    eprintln!(
                        "hk-pipeline: analog probe {:.4} MHz (channel {:.4}): mode {} ({:.2}), pilot {:?} → {} (OBW99 {:?} Hz, {:?})",
                        s.rf_center_hz / 1e6,
                        channel_center / 1e6,
                        mode_name(s),
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
            // T-416: **the refusal is recorded.** A probe that declines used to leave nothing at
            // all, so "we measured this window and it is not what this chain demodulates" was
            // indistinguishable from "nothing ever looked here" — the same confusion
            // `Coverage::Unobserved` and `BiasTee::Unknown` exist to prevent. What it measured is
            // written as a Demodulation against the emission's inventory entry: mode, estimated
            // parameters, the absent lock, and the window they came from. No promotion follows
            // from it (see `hk_demod::write_declined`).
            if let Some(Ok(s)) = &probed {
                // Release the ring reader **before** writing. The chain is leaving either way and
                // needs no more samples, and its gate cursor back-pressures the capture thread in
                // a lossless replay: holding it across two database transactions parks the whole
                // capture on this refusal's bookkeeping. (The same reason the record node runs on
                // its own thread rather than inline.)
                drop(cr);
                log_declined(&shared, s, &cand);
            }
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
        probe_center,
    );
    if let Some(join) = recorder {
        let _ = join.join();
    }
}

/// Records a probe that declined the window it measured (T-416).
///
/// The row goes in first and is *then* filed against the emission's inventory entry
/// (`Inventory::chain_measurement`), which is what makes the refusal findable — a Demodulation
/// nothing links to is a row in a table nobody reads. The two steps are separate because a chain
/// attaches on the **tracker's** confirmation and probes about a second into an emission, which is
/// routinely before the inventory has offered that track a row at all; the inventory holds the
/// measurement until the track is bound rather than the chain guessing at an entry or dropping it.
fn log_declined(shared: &Arc<Shared>, session: &AnalogSession, cand: &Candidate) {
    let c = &shared.counters.chains;
    // Takes the repository lock itself, so it runs before the guard below: `Shared::repo` is a
    // plain `Mutex` and re-entering it deadlocks the chain.
    let detection_ref = super::stored_detection(shared, cand.detection);
    let ctx = RecordContext {
        recording_ref: None,
        detection_ref,
        emitter_hint: None,
        counted_as: None,
    };
    let mut repo = shared.repo();
    let demod = match write_declined(&mut repo, session, &ctx) {
        Ok(id) => id,
        Err(err) => {
            inc(&c.errors);
            eprintln!("hk-pipeline: analog chain declined-measurement write: {err}");
            return;
        }
    };
    inc(&c.declined_measurements);
    let at = session.time_range().end;
    let mut inv = shared
        .inventory
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Named, not just counted (T-293/T-319): losing this silently is losing the record the whole
    // path exists to leave.
    if let Err(err) = inv.chain_measurement(&mut repo, cand.track, demod, at) {
        inc(&c.errors);
        eprintln!("hk-pipeline: analog chain declined-measurement link: {err}");
    }
    if crate::debug_enabled() {
        eprintln!(
            "hk-pipeline: analog probe {:.4} MHz declined; measurement recorded as {demod:?} \
             (mode {})",
            session.rf_center_hz / 1e6,
            mode_name(session),
        );
    }
}

/// Samples of the leading window the refinement reads (T-186: the early identification's window).
fn identify_len(fs: f64) -> usize {
    (RefineSettings::default().window_s * fs).round() as usize
}

/// The mode evidence an emitter may be placed on without a decoded identity (T-186): a
/// demodulator-selected refinable mode (WFM) whose 19 kHz pilot was found.
fn pilot_locked(session: &AnalogSession) -> bool {
    crate::refine::refinable(session.mode.mode)
        && session
            .mode
            .features
            .pilot
            .as_ref()
            .is_some_and(|p| p.found)
}

/// Outcome of [`identify`].
enum Identified {
    /// Written; the emitter it placed (none without pilot-locked mode evidence) and whether it
    /// stored the refined tuning on it.
    Written(Option<EmitterId>, bool),
    /// Not written (the leading window did not demodulate to pilot-locked evidence, or its front
    /// end was overloaded).
    Skipped,
    /// A neighbouring chain owns the emission.
    Preempted,
}

/// T-186: demodulates the leading window `w` (on `refined` when locked) without RDS (the full
/// window decodes and writes it once) and writes it: Demodulation, mode Classification on the
/// emitter ([`mode_emitter`]), refined tuning, then `Inventory::chain_emitter` (explanations,
/// review). T-209: an overloaded window writes nothing early.
fn identify(
    shared: &Arc<Shared>,
    w: &Window,
    cand: &Candidate,
    node: &AnalogNode,
    refined: Option<&RefinementOutcome>,
) -> Identified {
    let c = &shared.counters.chains;
    let Some(Ok(session)) = demodulate(
        w,
        w.iq.len(),
        cand,
        node.bandwidth_hz,
        refined.map(|o| &o.tuning),
        false,
    ) else {
        return Identified::Skipped;
    };
    let mode_ok = node.accept_modes.is_empty() || node.accept_modes.contains(&mode_name(&session));
    if !mode_ok || !pilot_locked(&session) {
        return Identified::Skipped;
    }
    if front_end_overloaded(w, w.iq.len()) {
        inc(&c.mode_emitters_withheld);
        return Identified::Skipped;
    }
    if !shared.claims.commit(node.owner) {
        inc(&c.duplicate_emission);
        return Identified::Preempted;
    }
    let ctx = RecordContext {
        recording_ref: None,
        detection_ref: super::stored_detection(shared, cand.detection),
        emitter_hint: None,
        counted_as: None,
    };
    let mut repo = shared.repo();
    let emitter = match write_session(&mut repo, &session, &ctx)
        .and_then(|written| mode_emitter(&mut repo, &session, &written, true))
    {
        Ok(e) => e,
        Err(err) => {
            inc(&c.errors);
            eprintln!("hk-pipeline: analog chain early identification write: {err}");
            return Identified::Written(None, false);
        }
    };
    inc(&c.identifications);
    if crate::debug_enabled() {
        eprintln!(
            "hk-pipeline: analog chain {:.4} MHz identified from {} samples: mode {} ({:.2}), emitter {emitter:?}",
            session.rf_center_hz / 1e6,
            w.iq.len(),
            mode_name(&session),
            session.mode.confidence,
        );
    }
    let mut stored = false;
    if let Some(e) = emitter {
        if let Some(o) = refined {
            match crate::refine::persist(
                &mut repo,
                e,
                o,
                SOURCE_ANALOG_CHAIN,
                session.time_range().start,
            ) {
                Ok(row) => stored = row.is_some(),
                Err(err) => {
                    inc(&c.errors);
                    eprintln!("hk-pipeline: analog chain refined tuning write: {err}");
                }
            }
        }
        let mut inv = shared
            .inventory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Named, not just counted (T-293/T-319): a chain_emitter failure here silently loses the
        // fold, the T-078 confirmation review and the T-219 overlap resolution, and a bare count
        // trains everyone to ignore it.
        if let Err(err) = inv.chain_emitter(&mut repo, cand.track, e) {
            inc(&c.errors);
            eprintln!("hk-pipeline: analog chain emitter: {err}");
        }
    }
    Identified::Written(emitter, stored)
}

/// The emitter a written session belongs to: the one `write_session` resolved (decoded PI or the
/// caller's hint), else — T-186 — for pilot-locked mode evidence over at least the leading
/// window (`enough`), a sighting keyed by the Demodulation (no identity) carrying the mode
/// Classification. `None` otherwise, as before. T-209: callers pass `enough: false` for an
/// overloaded window ([`front_end_overloaded`]), which places no emitter from mode evidence.
fn mode_emitter(
    repo: &mut Repository,
    session: &AnalogSession,
    written: &hk_demod::WrittenSession,
    enough: bool,
) -> Result<Option<EmitterId>, RepoError> {
    if written.emitter_id.is_some() || !enough || !pilot_locked(session) {
        return Ok(written.emitter_id);
    }
    let time = session.time_range();
    let family = session.mode.mode.as_str();
    let bandwidth = session.params.obw99_hz.value().unwrap_or(200e3);
    let source = LinkTarget::Demodulation(written.demodulation_id);
    let sighting = Sighting {
        source,
        seen: time,
        count: 1,
        f_center_hz: session.rf_center_hz,
        bandwidth_hz: bandwidth,
        fingerprint: Some(Fingerprint {
            family: Some(family.into()),
            ..Fingerprint::new(session.rf_center_hz, bandwidth)
        }),
        identity: None,
        context: None,
        classification: Some(Classification {
            t: time.end,
            family: family.into(),
            confidence: session.mode.confidence,
            open_set_score: 1.0 - session.mode.confidence,
            model_version: session.mode.rules_version.clone(),
        }),
        tags: Vec::new(),
    };
    let r =
        repo.record_sighting_measured(&sighting, &MeasurementKey::new(SOURCE_ANALOG_CHAIN), None)?;
    repo.link_emitter(&EmitterLink {
        emitter_id: r.emitter_id,
        target: source,
        linked_at: time.end,
    })?;
    Ok(Some(r.emitter_id))
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

/// The emission a chain takes from a probe that found its mode *outside* this channel (T-070):
/// a validated refinement decides, by whether this chain [`owns`] the centre it measured; without
/// one, the probe's own centre stands when this chain owns that.
///
/// T-226: the probe used to drop the chain whenever the refinement did not validate, so an
/// off-raster station the probe had already demodulated was never identified at all (T-186) —
/// the refinement's own budget could decide whether a station exists. The probe centre is measured
/// evidence (the receiver's carrier estimate on the probe window), and the full window refines
/// from it again; only ownership, never a band plan, decides whose chain it is.
fn off_channel_emission(
    node: &AnalogNode,
    channel_center: f64,
    probe_hz: f64,
    refined: Option<&RefinementOutcome>,
) -> Option<f64> {
    match refined {
        Some(o) => owns(node, channel_center, o.tuning.center_hz).then_some(o.tuning.center_hz),
        None => owns(node, channel_center, probe_hz).then_some(probe_hz),
    }
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
/// starts from the probe's refined result when the probe needed one, else from the probe's own
/// centre when the probe found the emission outside this channel without a validated refinement
/// (T-226), else from the attach channel; it must stay owned by this chain.
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
    probe_center: Option<f64>,
) {
    let fs = shared.fs;
    let c = &shared.counters.chains;
    let channel_center = 0.5 * (cand.f_lo_hz + cand.f_hi_hz);
    // The refinement reads only the leading `RefineSettings::window_s` of the window.
    let refine_leading = |w: &Window, m: AnalogMode| {
        let (start, width) = match (probe_refined.as_ref(), probe_center) {
            (Some(p), _) => (p.tuning.center_hz, 0.0),
            (None, Some(c)) => (c, node.bandwidth_hz),
            (None, None) => (channel_center, cand.f_hi_hz - cand.f_lo_hz),
        };
        refine_window(w, w.iq.len(), start, width, m)
            .or_else(|| probe_refined.clone())
            .filter(|o| owns(node, channel_center, o.tuning.center_hz))
    };
    // T-186: identify from the leading window first (see the module docs).
    let identify_samples = identify_len(fs);
    let mut early = None;
    if let Some(m) = probe_mode.filter(|&m| crate::refine::refinable(m))
        && identify_samples < want
    {
        collect(&mut cr, rx, &mut w, identify_samples);
        if w.iq.len() == identify_samples {
            let refined = refine_leading(&w, m);
            // T-071: one chain per emission, even when a neighbour's chain refined to it too.
            let emission = refined
                .as_ref()
                .map_or(probe_center.unwrap_or(channel_center), |o| {
                    o.tuning.center_hz
                });
            if !claim_emission(shared, node, channel_center, emission) {
                return;
            }
            match identify(shared, &w, cand, node, refined.as_ref()) {
                Identified::Preempted => return,
                Identified::Written(emitter, stored) => early = Some((refined, emitter, stored)),
                Identified::Skipped => early = Some((refined, None, false)),
            }
        }
    }
    collect(&mut cr, rx, &mut w, want);
    drop(cr);
    if (w.iq.len() as f64) < 0.25 * fs {
        return;
    }
    let (refined, hint, refined_stored) = match early {
        // Same leading samples, so the same refinement, already claimed at its centre.
        Some(e) => e,
        None => {
            let refined = probe_mode.and_then(|m| refine_leading(&w, m));
            // T-071: one chain per emission, even when a neighbour's chain refined to it too.
            let emission = refined
                .as_ref()
                .map_or(probe_center.unwrap_or(channel_center), |o| {
                    o.tuning.center_hz
                });
            if !claim_emission(shared, node, channel_center, emission) {
                return;
            }
            (refined, None, false)
        }
    };
    let session = match demodulate(
        &w,
        w.iq.len(),
        cand,
        node.bandwidth_hz,
        refined.as_ref().map(|o| &o.tuning),
        true,
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
        // T-186: the emitter the early identification placed.
        emitter_hint: hint,
        // T-209: its sighting already counted this session.
        counted_as: hint.map(|_| SOURCE_ANALOG_CHAIN),
    };
    if !shared.claims.commit(node.owner) {
        // Preempted by the chain whose channel is nearer the emission.
        inc(&c.duplicate_emission);
        return;
    }
    let enough = w.iq.len() >= identify_samples;
    let overloaded = front_end_overloaded(&w, w.iq.len());
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
            // T-209: no emitter from mode evidence alone on an overloaded front end.
            if overloaded && enough && written.emitter_id.is_none() && pilot_locked(&session) {
                inc(&c.mode_emitters_withheld);
            }
            let emitter = mode_emitter(&mut repo, &session, &written, enough && !overloaded)
                .unwrap_or_else(|err| {
                    inc(&c.errors);
                    eprintln!("hk-pipeline: analog chain emitter write: {err}");
                    None
                });
            if let Some(e) = emitter {
                // T-209: the early identification already stored this refinement.
                if let Some(o) = refined.as_ref().filter(|_| !refined_stored)
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
                // Named, not just counted (T-293/T-319): a chain_emitter failure here silently
                // loses the fold, the T-078 confirmation review and the T-219 overlap resolution,
                // and a bare count trains everyone to ignore it.
                if let Err(err) = inv.chain_emitter(&mut repo, cand.track, e) {
                    inc(&c.errors);
                    eprintln!("hk-pipeline: analog chain emitter: {err}");
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

    fn outcome(center_hz: f64) -> RefinementOutcome {
        RefinementOutcome {
            provenance: String::new(),
            objective: "test@1".into(),
            mode: "wfm".into(),
            start: Tuning::default(),
            tuning: Tuning {
                center_hz,
                bandwidth_hz: 200e3,
                mode: Default::default(),
            },
            quality: 50.0,
            locked: true,
            validated: true,
            converged: true,
            stop: hk_demod::refine::StopReason::Completed,
            iterations: 1,
            evaluations: 1,
            elapsed_s: 0.0,
            mode_params: Default::default(),
            labels: Default::default(),
            trace: Vec::new(),
        }
    }

    /// T-226: a probe that found its mode outside this channel keeps the emission when the
    /// refinement does not validate — the chain continues from the probe's own centre and
    /// identifies the station (T-186) instead of stopping. A validated refinement still decides.
    #[test]
    fn an_off_channel_probe_falls_back_to_its_own_centre_without_a_validated_refinement() {
        let n = node(200e3);
        let ch = 101.3e6;
        assert_eq!(
            off_channel_emission(&n, ch, 101.4495e6, None),
            Some(101.4495e6),
            "off raster beside this channel: this chain takes it"
        );
        assert_eq!(
            off_channel_emission(&n, ch, 101.5e6, None),
            None,
            "on the neighbour's raster channel: not ours"
        );
        assert_eq!(
            off_channel_emission(&n, ch, 101.4495e6, Some(&outcome(101.45e6))),
            Some(101.45e6),
            "a validated refinement decides where the emission is"
        );
        assert_eq!(
            off_channel_emission(&n, ch, 101.4495e6, Some(&outcome(101.5e6))),
            None,
            "a refinement onto the neighbour's channel is not rescued by the probe centre"
        );
    }
}
