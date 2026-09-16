//! Sweep characterisation of a candidate region, as a chain on a running pipeline (T-297).
//!
//! T-294 built `hk_dsp::chirp` and left it deliberately uncalled, reporting that rather than
//! smuggling a seam into a task not scoped for one. So the capability had no caller — the fifth
//! instance in waiting of the pattern T-242, T-247, T-206 and T-267/T-287 each closed: **a
//! capability that nothing calls is indistinguishable from an absent one**, and its tests pass
//! either way. This module is that caller.
//!
//! # Which product it reads, and why it cannot be the detector's
//!
//! The **IQ in the ring**, channelised to the candidate region's own band.
//!
//! That is forced, not preferred. The detector consumes `K` averaged periodograms, from which one
//! `(f_lo, f_hi)` per component is formed; for a sweep that completes inside one analysis frame
//! the centre track is **not present in that product at all** (ADR-0017 §1.3(b)), so no amount of
//! reading detections recovers a sweep rate. T-294 measured the other side of it: a lag-product
//! estimator reading **one 4.096 ms frame of IQ** separates a LoRa SF9 chirp from a wideband
//! burst, a carrier and a 2-FSK burst at 96–99.5 % against 0–2.2 %, down to about −3 dB in the
//! channel, recovering the rate to 1.6 %. Something therefore has to read IQ for a candidate
//! region, and this is where.
//!
//! # Why a chain, and why a new trigger
//!
//! A chain is the one thing in the pipeline that already reads arbitrary-channel IQ off the
//! running capture with a gate cursor and admission control ([`super::trunk`] is the precedent).
//! What a chain could *not* do before is run **beside** a decode chain: [`select_for_track`] picks
//! the first matching [`Trigger::ConfirmedTrack`] spec and stops, and `try_attach` returns early
//! when that spec's `min_detections` is unmet. Measured on T-255's SF9 scene, that is exactly the
//! hole a characteriser has to reach: the LoRa region matches `fsk-bursts`, never reaches its four
//! member detections, and the track closes `unmatched` — **no chain at all, so no IQ is ever read
//! for the one region in the scene that sweeps**. A sweep spec placed after `fsk-bursts` would
//! never be reached for that track; one placed before would steal every bursty track from it.
//!
//! Hence [`Trigger::EveryTrack`]: attaches *in addition to* whatever decode chain was selected,
//! for every confirmed track whose priors match. Additive — no existing spec names it, `Trigger`
//! keeps its default, and chain selection is untouched.
//!
//! # Cost, on the capture and DSP threads
//!
//! The characteriser runs on its **own chain thread**, spawned by [`ChainManager`](super::ChainManager)
//! on the control thread, behind its own [`ChainReader`] and its own gate cursor — off the ring's
//! always-on readers, off the DSP readers' buffers, and off every audio chain. Attaching and
//! detaching never touch the capture thread (ADR-0001 S1); in lossless replay its cursor joins the
//! flow gate like any other chain's, and it keeps reading and releasing for as long as it is
//! attached precisely so that it can never stall the gate.
//!
//! Per pass the cost is bounded a priori: **at most `max_passes` passes per region, and the chain
//! stops at the first characterisation**; one window of `window_s × fs` samples held at a time
//! (nothing rolls over, so memory is one window, not a stream); one DDC of that window to the
//! region's own bandwidth; `floor(window / frame)` power sums of `frame` samples to choose the
//! strongest whole frame; and then **exactly two FFTs of `frame` points** — the two lags — on that
//! one frame. No demodulation, no audio, no ring access outside the chain's own reader, and the
//! repository lock only for the single write at detach. Across the run at most `max_chains` such
//! chains exist at once; above that the attach is refused and counted.
//!
//! # Evidence, never identity
//!
//! A characterisation is a [`field::SWEEP_RATE_HZ_PER_S`] field on the emitter's
//! [`EmissionFeatures`](hk_model::EmissionFeatures) snapshot, written through the one
//! characterisation call site ([`crate::characterise::characterise_with`]). It says *this region
//! sweeps at rate α*; it never says the region is LoRa, a radar or anything else. It sets no
//! family, no identity, no status and no lifecycle — the rule [`crate::characterise`] and
//! [`crate::classify`] already follow.
//!
//! **A region that cannot be characterised says so by writing nothing.** The two lags must both
//! peak above [`SWEEP_MIN_PAPR`] and agree on the rate within [`SWEEP_MAX_DISAGREEMENT`]; below
//! about −3 dB in the channel, or with no linear sweep there at all, they do not, and the chain
//! writes no field and counts `sweep_uncharacterised`. Absent means *not measured* — the same rule
//! that stops `unknown` being written as a family. Nothing here ever writes a zero rate, and no
//! threshold of T-294's or the detector's is read, moved or reachable from a spec: what a spec
//! sets is only how much one characterisation may **spend**.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use hk_context::signature::FeatureObservation;
use hk_core::{Discontinuity, ProvenanceHandle};
use hk_dsp::chirp::{
    SWEEP_MAX_DISAGREEMENT, SWEEP_MIN_PAPR, SweepTest, default_lags, linear_sweep,
};
use hk_dsp::{Ddc, DdcSpec, InputInfo};
use hk_model::signature::field;
use hk_model::{SampleTime, Timestamp};
use num_complex::{Complex, Complex32};

use super::{ChainMsg, ChainReader, Next};
use crate::events::Candidate;
use crate::gate::GateCursor;
use crate::run::Shared;
use crate::stats::inc;

/// `Feat::method` of the field this chain contributes: the producer, so a reader can tell a
/// measured sweep rate from anything else that might one day write the same field.
pub(crate) const SWEEP_METHOD: &str = "hk-pipeline/sweep@1";

/// A channel narrower than this is floored to it (a degenerate zero-width DDC spec is not a
/// measurement), matching the floor `chains::iq` uses.
const MIN_BANDWIDTH_HZ: f64 = 1_000.0;

/// Fewest samples in an analysis frame for the two-lag test to mean anything: below this
/// `default_lags` collapses onto neighbouring bins and the "agreement" is arithmetic, not evidence.
const MIN_FRAME_SAMPLES: usize = 64;

/// Longest wait for the region's inventory emitter after the chain detaches.
///
/// A track's emitter is written when the track closes, which races this chain's detach. Bounded so
/// a run can never hang on it, and paid only when there is actually a characterisation to write.
const EMITTER_WAIT: Duration = Duration::from_secs(2);

/// What one characterisation may spend. Built from [`super::spec::ChainShape::Sweep`]; see the
/// module docs for the bound each field carries.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SweepNode {
    /// Samples collected per pass, s.
    pub window_s: f64,
    /// Analysis frame the two-lag test runs on, s.
    pub frame_s: f64,
    /// Most windows examined before the chain gives up on this region.
    pub max_passes: u64,
}

/// Runs the characteriser until the stream ends or the chain is detached.
pub(crate) fn run(
    shared: Arc<Shared>,
    rx: Receiver<ChainMsg>,
    cand: Candidate,
    node: SweepNode,
    cursor: GateCursor,
) {
    let fs = shared.fs;
    let c = &shared.counters.chains;
    let window = ((node.window_s * fs) as usize).max(MIN_FRAME_SAMPLES);
    let min_window = (window / 2).max(MIN_FRAME_SAMPLES);
    let mut cr = ChainReader::new(Arc::clone(&shared), cand.first_sample, cursor);
    let mut buf: Vec<Complex<i8>> = Vec::with_capacity(window);
    let mut prov: Option<ProvenanceHandle> = None;
    let mut base = 0u64;
    let mut base_time = Timestamp::UNIX_EPOCH;
    let mut passes = 0u64;
    // The region, widened by the member boxes of its track: the detector's first confirming box is
    // often one lobe of the emission, and the DDC wants the whole channel.
    let (mut f_lo, mut f_hi) = (cand.f_lo_hz, cand.f_hi_hz);
    // The characterisation, held until detach so it is written against a settled inventory.
    let mut measured: Option<(SweepTest, f64, f64)> = None;
    let (mut detach, mut closed) = (false, false);
    loop {
        match rx.try_recv() {
            Ok(ChainMsg::Member(m)) => {
                f_lo = f_lo.min(m.f_lo_hz);
                f_hi = f_hi.max(m.f_hi_hz);
            }
            Ok(ChainMsg::Detach) => detach = true,
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => detach = true,
        }
        let done = measured.is_some() || passes >= node.max_passes;
        if !closed {
            match cr.next() {
                Next::Data(ch) => {
                    if done {
                        // Finished measuring, but still attached: keep reading and releasing so the
                        // lossless flow gate never waits on this cursor. Costs the ring read every
                        // chain does, and nothing else.
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
                        if buf.len() < window {
                            buf.extend_from_slice(&cr.buf[..ch.len]);
                        }
                    }
                    cr.release_to(ch.end_sample());
                }
                Next::Lost => buf.clear(),
                Next::Idle => {}
                Next::Closed => closed = true,
            }
        }
        // One shorter pass is allowed when the stream ends before the first full window, so a short
        // recording still gets exactly one look. At most once per chain.
        let last_chance = (closed || detach) && passes == 0 && buf.len() >= min_window;
        if !done && (buf.len() >= window || last_chance) {
            if let Some(p) = prov.clone() {
                let center = 0.5 * (f_lo + f_hi);
                let bw = (f_hi - f_lo).max(MIN_BANDWIDTH_HZ);
                passes += 1;
                inc(&c.sweep_passes);
                match characterise_window(&node, &buf, base, base_time, &p, center, bw) {
                    Some(test) => measured = Some((test, center, bw)),
                    None => inc(&c.sweep_uncharacterised),
                }
            }
            buf.clear();
        }
        if detach || closed {
            break;
        }
    }
    drop(cr);

    let Some((test, center, bw)) = measured else {
        return;
    };
    if crate::debug_enabled() {
        eprintln!(
            "hk-pipeline: sweep-char {:.4} MHz ({:.1} kHz): α {:.4e} Hz/s (papr {:.0}, \
             disagreement {:.3})",
            center / 1e6,
            bw / 1e3,
            test.rate_hz_per_s,
            test.papr,
            test.disagreement
        );
    }
    write_characterisation(&shared, center, bw, &test);
}

/// One pass: channelise the window to the region's own band, choose the strongest whole frame, and
/// run the two-lag sweep test on it.
///
/// `None` is the honest answer for everything that is not a linear sweep — a carrier, a wideband
/// burst, a 2-FSK burst, or a sweep below the estimator's SNR floor. Nothing is guessed here.
fn characterise_window(
    node: &SweepNode,
    buf: &[Complex<i8>],
    base: u64,
    t_start: Timestamp,
    prov: &ProvenanceHandle,
    center_hz: f64,
    bandwidth_hz: f64,
) -> Option<SweepTest> {
    let fs = prov.tune.sample_rate_hz;
    if !(fs.is_finite() && fs > 0.0) {
        return None;
    }
    // The only frequencies used are the region's own measured edges and where the device says it
    // is tuned. No band plan, no raster, no truth.
    // The region's own width is the **flat** passband, and the output rate is left to the DDC's
    // default: an integer decimation giving at least 2× that bandwidth. Asking for an output rate
    // equal to the bandwidth instead — the obvious reading of "sample it at its own channel width",
    // and what the first version of this did — puts the passband edge exactly on the output Nyquist
    // with no transition room, so the filter rolls off and aliases precisely at the band edges a
    // chirp sweeps through. Measured: every window came back uncharacterised, the chirp included.
    let ddc_spec = DdcSpec::new(center_hz - prov.tune.center_hz, bandwidth_hz);
    let mut ddc = Ddc::new(ddc_spec, fs).ok()?;
    let info = InputInfo {
        time: SampleTime {
            sample_index: base,
            host_time: t_start,
        },
        discontinuity: Discontinuity::NONE,
        dropped_before: 0,
        provenance: prov,
    };
    let baseband: Vec<Complex32> = ddc.process(info, buf).ok()?.samples.to_vec();
    let rate = ddc.output_rate_hz();
    if !(rate.is_finite() && rate > 0.0) {
        return None;
    }
    let n = (node.frame_s * rate).round() as usize;
    if n < MIN_FRAME_SAMPLES || baseband.len() < n {
        return None;
    }
    // Which frame, decided without truth: the strongest whole frame in the window. A region's
    // emission rarely fills the whole dwell, and a frame of its silence carries no sweep to find.
    let frames = baseband.len() / n;
    let mut best = 0usize;
    let mut best_power = f64::NEG_INFINITY;
    for i in 0..frames {
        let p: f64 = baseband[i * n..(i + 1) * n]
            .iter()
            .map(|v| f64::from(v.norm_sqr()))
            .sum();
        if p > best_power {
            best_power = p;
            best = i;
        }
    }
    let frame = &baseband[best * n..(best + 1) * n];
    let lags = default_lags(n);
    if crate::debug_enabled() {
        let one = |lag: usize| match hk_dsp::chirp::chirp_rate(frame, rate, lag) {
            Some(r) => format!(
                "lag {lag}: α {:.4e} papr {:.1} res {:.3e}",
                r.rate_hz_per_s, r.papr, r.resolution_hz_per_s
            ),
            None => format!("lag {lag}: none"),
        };
        eprintln!(
            "hk-pipeline: sweep-char probe {:.4} MHz bw {:.1} kHz -> rate {:.1} kHz, n {n}, \
             frame {best}/{frames}; {}; {}",
            center_hz / 1e6,
            bandwidth_hz / 1e3,
            rate / 1e3,
            one(lags.0),
            one(lags.1)
        );
    }
    linear_sweep(frame, rate, lags, SWEEP_MIN_PAPR, SWEEP_MAX_DISAGREEMENT)
}

/// Writes the sweep rate as evidence on the region's inventory emitter.
///
/// The emitter is resolved by the region the chain measured, never by a frequency looked up
/// anywhere. A track's emitter is written when the track closes, which races this chain's detach,
/// so the lookup is retried for a bounded [`EMITTER_WAIT`] — paid only when there is something to
/// write. With no emitter the measurement is dropped and counted, never attached to a guess.
fn write_characterisation(shared: &Shared, center_hz: f64, bandwidth_hz: f64, test: &SweepTest) {
    let c = &shared.counters.chains;
    let deadline = Instant::now() + EMITTER_WAIT;
    loop {
        let found = {
            let repo = shared.repo();
            crate::refine::emitter_for_channel(&repo, center_hz, bandwidth_hz).ok()
        };
        if let Some(Some(emitter)) = found {
            // The two lags' disagreement is what this measurement knows about its own error, so it
            // is carried as the field's 1σ rather than a zero the aggregate would have to unlearn.
            let sigma = test.disagreement * test.rate_hz_per_s.abs();
            let obs = FeatureObservation::new().num(
                field::SWEEP_RATE_HZ_PER_S,
                test.rate_hz_per_s,
                sigma,
                SWEEP_METHOD,
            );
            let mut repo = shared.repo();
            // Timed by the emitter's **own latest observation**, which is the convention the other
            // path through this call site already uses (`TrackInventory::characterise` passes
            // `e.last_seen`). The chain's window starts earlier than anything the inventory has
            // recorded for this emitter, and a cluster assignment stamped before its cluster
            // existed is refused outright — measured: "a cluster cannot change before it exists".
            let t = match repo.emitter(emitter) {
                Ok(e) => e.last_seen,
                Err(e) => {
                    inc(&c.errors);
                    eprintln!("hk-pipeline: sweep-char emitter: {e}");
                    return;
                }
            };
            match crate::characterise::characterise_with(&mut repo, emitter, obs, t) {
                Ok(_) => inc(&c.sweep_characterised),
                Err(e) => {
                    inc(&c.errors);
                    eprintln!("hk-pipeline: sweep-char write: {e}");
                }
            }
            return;
        }
        if Instant::now() >= deadline {
            inc(&c.sweep_no_emitter);
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
}
