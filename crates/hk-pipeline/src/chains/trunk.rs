//! C23 control-channel hunt, as a chain on a running pipeline (T-287).
//!
//! T-267 built the capability and proved it blind through the device — 33 raster channels swept,
//! 6 candidates at FCO 1.0, exactly one confirmed, the continuous decoy rejected — but it stopped
//! short of a caller: everything it needed sat behind `pub(crate) Shared`, and a hook meant new
//! chain-contract variants. Nothing in a normal run called it, so a run wrote no control-channel
//! row and the capability's tests passed whether or not the pipeline could ever reach it. This
//! module is that caller.
//!
//! # The two stages, kept apart
//!
//! The split `hk_detect::trunk` enforces by type is the shape of this chain too, and this module
//! adds no way round it:
//!
//! 1. **Candidacy** is measured here: per-raster-channel frequency-channel occupancy over the
//!    chain's own dwell, against the band's own measured noise floor. Cheap, spectral, and *not*
//!    evidence of a control channel — C23's named pitfall is that continuous data emitters pass
//!    the FCO test.
//! 2. **Confirmation** is not measured here at all. The only value of type `ConfirmedCc` in the
//!    workspace still comes out of [`CcConfirmer::confirm`], which needs demodulated symbols and
//!    requires frame sync **and** CRC-valid blocks. This module calls it and writes down what it
//!    returns; it cannot manufacture one.
//!
//! Every threshold that decides anything (`MIN_CC_FCO`, the sync tolerance, the sync and CRC
//! counts) lives in `hk_detect::trunk` with its arithmetic, where a chain spec cannot reach it.
//! What a spec sets ([`super::spec::NodeSpec::TrunkCc`]) is only how much the hunt may *spend*.
//!
//! # Cost, on the capture and audio threads
//!
//! The hunt runs on its **own chain thread**, spawned by [`ChainManager`](super::ChainManager) on
//! the control thread, behind its own [`ChainReader`] and its own gate cursor — off the ring's
//! always-on readers, off the DSP readers' buffers, and off every audio chain. Attaching and
//! detaching never touch the capture thread (ADR-0001 S1); in lossless replay its cursor joins the
//! flow gate like any other chain's.
//!
//! Per pass the cost is bounded a priori: **at most one pass per `period_s` of stream time**, one
//! window of `window_s × fs` samples held at a time (nothing rolls over, so memory is one window,
//! not a stream), `floor(window_s × fs / 1024)` 1024-point power rows, `≤ max_channels` band-power
//! integrations of ~`raster/bin` bins per row, and **`≤ max_demods` down-conversions plus C4FM
//! demodulations** of that one window — the expensive half, and the one the admission cap exists
//! for. Then one `CcConfirmer` scan per demodulated channel, and at most one `TrunkSystem` upsert
//! per newly confirmed channel per run. No audio anywhere, no ring access outside the chain's own
//! reader, and the repository lock is taken only for the upsert.
//!
//! The one deliberate exception to the period bound: if the stream ends before the first full
//! window, one shorter pass is allowed, so a short recording still gets exactly one hunt. It can
//! happen at most once per chain.
//!
//! # Metadata only (M4)
//!
//! The chain writes a [`TrunkSystem`] row — a protocol (still `Unknown`; naming it is T-268), the
//! measured control-channel frequency, and times — and nothing else. No demodulated audio, no
//! payload, no recording, no stream. That is what lets it run under the fail-closed
//! `metadata-only` class a 12.5 kHz LMR band derives, and the validator in
//! [`super::spec`] refuses any `trunk-cc` spec that sets `requires_content` or carries a record
//! node, so it stays true.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};

use hk_core::{Discontinuity, ProvenanceHandle};
use hk_demod::fsk::{C4fmConfig, C4fmDemod};
use hk_detect::trunk::{
    CcCandidate, CcConfirmer, MIN_CC_FCO, RASTER_TOLERANCE_HZ, best_lmr_raster,
};
use hk_dsp::{Ddc, DdcSpec, InputInfo, SegmentEngine, WelchConfig, WindowKind};
use hk_model::{SampleTime, Timestamp, TrunkSystem};
use num_complex::{Complex, Complex32};

use super::{ChainMsg, ChainReader, Next};
use crate::events::Candidate;
use crate::gate::GateCursor;
use crate::run::Shared;
use crate::stats::{add, inc};

/// How far above the band's own **measured** noise floor a raster channel counts as occupied, dB.
///
/// A priori, and deliberately not a confirmation threshold: it decides only which channels are
/// worth a demodulation. Moving it can add or remove candidates; it can never confirm one, because
/// confirmation is frame sync plus CRC. 6 dB is the conventional "clearly above the floor", and it
/// is the value T-267's blind acceptance scene used.
const OCCUPIED_MARGIN_DB: f64 = 6.0;

/// FFT length of the occupancy sweep's power rows.
///
/// At 500 kHz this is a 488 Hz bin and a 2.05 ms frame: ~25 bins across a 12.5 kHz channel, and
/// frames short enough that a bursty neighbour reads as bursty rather than continuous.
const SWEEP_FFT_LEN: usize = 1024;

/// Samples per symbol the candidate down-conversion aims for (the plan floors to an integer
/// decimation, so the realised value is at least this).
const DEMOD_SPS: f64 = 10.0;

/// Fraction of the sample rate the sweep treats as usable (the window's flat middle).
const USABLE_FRACTION: f64 = 0.8;

/// What one hunt may spend. Built from [`super::spec::ChainShape::TrunkCc`] plus the spec's
/// raster; see the module docs for the bound each field carries.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TrunkCcNode {
    /// Samples collected per pass, s.
    pub window_s: f64,
    /// Most raster channels measured for occupancy per pass.
    pub max_channels: usize,
    /// Most candidates demodulated per pass.
    pub max_demods: usize,
    /// Least stream time between passes, s.
    pub period_s: f64,
    /// Channel raster, Hz (the a-priori LMR grid the spec names).
    pub raster_hz: f64,
}

/// Runs the hunt until the stream ends or the chain is detached.
pub(crate) fn run(
    shared: Arc<Shared>,
    rx: Receiver<ChainMsg>,
    cand: Candidate,
    node: TrunkCcNode,
    cursor: GateCursor,
) {
    let fs = shared.fs;
    let c = &shared.counters.chains;
    let window = ((node.window_s * fs) as usize).max(SWEEP_FFT_LEN);
    let min_window = (window / 2).max(SWEEP_FFT_LEN);
    let period = (node.period_s * fs).max(0.0) as u64;
    let mut cr = ChainReader::new(Arc::clone(&shared), cand.first_sample, cursor);
    let mut buf: Vec<Complex<i8>> = Vec::with_capacity(window);
    let mut prov: Option<ProvenanceHandle> = None;
    let mut base = 0u64;
    let mut base_time = Timestamp::UNIX_EPOCH;
    let mut next_pass = 0u64;
    let mut passes = 0u64;
    // Control channels this chain has already written, by rounded frequency: a repeat sighting
    // updates `last_seen` rather than minting a second system for the same channel.
    let mut known: HashMap<i64, TrunkSystem> = HashMap::new();
    let (mut detach, mut closed) = (false, false);
    loop {
        match rx.try_recv() {
            // The hunt is not track-driven: its trigger is the band prior plus measured
            // occupancy, so a member box tells it nothing it does not measure itself.
            Ok(ChainMsg::Member(_)) => {}
            Ok(ChainMsg::Detach) => detach = true,
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => detach = true,
        }
        if !closed {
            match cr.next() {
                Next::Data(ch) => {
                    if ch.end_sample() < next_pass {
                        // Between passes the chain reads and discards. The period bound therefore
                        // costs no memory and no DSP, only the ring read every chain does.
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
        // One shorter pass is allowed when the stream ends before the first full window, so a
        // short recording still gets exactly one hunt. At most once per chain.
        let last_chance = (closed || detach) && passes == 0 && buf.len() >= min_window;
        if buf.len() >= window || last_chance {
            if let Some(p) = prov.clone() {
                hunt(&shared, &node, &buf, base, base_time, &p, &mut known);
                passes += 1;
                inc(&c.cc_passes);
            }
            next_pass = base + buf.len() as u64 + period;
            buf.clear();
        }
        if detach || closed {
            break;
        }
    }
}

/// One pass: sweep the raster for occupancy, demodulate the candidates admission allows, and
/// confirm what framing confirms.
fn hunt(
    shared: &Shared,
    node: &TrunkCcNode,
    buf: &[Complex<i8>],
    base: u64,
    t_start: Timestamp,
    prov: &ProvenanceHandle,
    known: &mut HashMap<i64, TrunkSystem>,
) {
    let c = &shared.counters.chains;
    let fs = prov.tune.sample_rate_hz;
    let raster = node.raster_hz;
    if !(fs.is_finite() && fs > 0.0 && raster > 0.0) {
        return;
    }
    // The ONLY frequency the hunt is given is where the device says it is tuned. The raster is an
    // a-priori standard and its origin is that centre — no band-plan lookup, no truth.
    let tune_center = prov.tune.center_hz;
    let half = USABLE_FRACTION * fs / 2.0;
    let max_k = ((half - raster / 2.0) / raster).floor();
    if !max_k.is_finite() || max_k < 0.0 {
        return;
    }
    let mut ks: Vec<i64> = (-(max_k as i64)..=(max_k as i64)).collect();
    if ks.len() > node.max_channels {
        // Keep the channels nearest the tuned centre: deterministic, blind, and the part of the
        // window with the least front-end roll-off.
        ks.sort_by_key(|k| k.abs());
        ks.truncate(node.max_channels);
        ks.sort_unstable();
    }
    let Some(fco) = occupancy(buf, fs, raster, &ks) else {
        return;
    };
    add(&c.cc_channels, ks.len() as u64);

    // ---- Candidacy. A pure-FCO detector would stop here and be wrong.
    let mut cands: Vec<(usize, f64)> = (0..ks.len())
        .filter(|&i| fco[i] >= MIN_CC_FCO)
        .map(|i| (i, fco[i]))
        .collect();
    add(&c.cc_candidates, cands.len() as u64);
    // ---- Admission control. Candidacy is cheap; demodulation is not, and on a busy LMR band many
    // channels are continuously occupied. Above the cap the highest-FCO candidates win, ties going
    // to the channel nearest the tuned centre — deterministic, and decided without truth.
    cands.sort_by(|a, b| b.1.total_cmp(&a.1).then(ks[a.0].abs().cmp(&ks[b.0].abs())));
    if cands.len() > node.max_demods {
        add(
            &c.cc_admission_refused,
            (cands.len() - node.max_demods) as u64,
        );
        cands.truncate(node.max_demods);
    }
    if crate::debug_enabled() {
        eprintln!(
            "hk-pipeline: trunk-cc pass at {:.4} MHz: {} raster channels, candidates {:?}",
            tune_center / 1e6,
            ks.len(),
            cands
                .iter()
                .map(|&(i, f)| (ks[i], format!("{f:.3}")))
                .collect::<Vec<_>>()
        );
    }
    if cands.is_empty() {
        return;
    }

    // ---- Confirmation: frame sync AND valid CRC, on demodulated symbols.
    let demod_cfg = C4fmConfig::default();
    let demod = C4fmDemod::new(demod_cfg);
    let confirmer = CcConfirmer::default();
    let decim = (fs / (DEMOD_SPS * demod_cfg.symbol_rate_bd))
        .floor()
        .max(1.0);
    let out_rate = fs / decim;
    let t_end = t_start.saturating_add_nanos((buf.len() as f64 * 1e9 / fs) as i64);
    for (i, fco_i) in cands {
        let k = ks[i];
        let offset = k as f64 * raster;
        let center_hz = tune_center + offset;
        let Some(fit) = best_lmr_raster(center_hz, tune_center, RASTER_TOLERANCE_HZ) else {
            continue;
        };
        let Some(candidate) = CcCandidate::new(center_hz, raster, fco_i, fit) else {
            continue;
        };
        // A deliberately wide channel, not a 12.5 kHz brick wall: the C4FM demodulator applies its
        // own channel filter, and leaving adjacent energy in is realistic. An extra candidate
        // costs a demodulation and is then rejected by sync + CRC, which is the design.
        let mut spec = DdcSpec::new(offset, 2.0 * raster);
        spec.output_rate_hz = Some(out_rate);
        let mut ddc = match Ddc::new(spec, fs) {
            Ok(d) => d,
            Err(_) => {
                inc(&c.errors);
                continue;
            }
        };
        let info = InputInfo {
            time: SampleTime {
                sample_index: base,
                host_time: t_start,
            },
            discontinuity: Discontinuity::NONE,
            dropped_before: 0,
            provenance: prov,
        };
        let baseband: Vec<Complex32> = match ddc.process(info, buf) {
            Ok(b) => b.samples.to_vec(),
            Err(_) => {
                inc(&c.errors);
                continue;
            }
        };
        inc(&c.cc_demods);
        let rate = ddc.output_rate_hz();
        let Ok(symbols) = demod.demodulate(&baseband, rate, 0.0) else {
            continue;
        };
        let Some(cc) = confirmer.confirm(&candidate, &symbols.dibits) else {
            continue;
        };
        inc(&c.cc_confirmed);
        let ev = cc.evidence();
        if crate::debug_enabled() {
            eprintln!(
                "hk-pipeline: trunk-cc confirmed {:.4} MHz (fco {fco_i:.3}): sync {} crc {}/{} \
                 pattern {}",
                cc.cc_freq_hz() / 1e6,
                ev.sync_hits(),
                ev.crc_valid(),
                ev.crc_checked(),
                ev.pattern()
            );
        }
        // Metadata only: a protocol (still `Unknown` — confirming a control channel is not naming
        // it, which is T-268), the measured frequency, and when it was heard.
        let key = cc.cc_freq_hz().round() as i64;
        let system = match known.get(&key) {
            Some(s) => {
                let mut s = s.clone();
                s.last_seen = t_end;
                s.updated_at = t_end;
                s
            }
            None => TrunkSystem::new(cc.protocol(), Some(cc.cc_freq_hz()), t_end),
        };
        let stored = {
            let mut repo = shared.repo();
            repo.put_trunk_system(&system)
        };
        match stored {
            Ok(()) => {
                if known.insert(key, system).is_none() {
                    inc(&c.cc_systems);
                }
            }
            Err(e) => {
                inc(&c.errors);
                eprintln!("hk-pipeline: trunk-cc write: {e}");
            }
        }
    }
}

/// Frequency-channel occupancy of each channel in `ks`, over `buf`.
///
/// One segmented power sweep gives every channel at once: per 1024-sample frame, the band power of
/// each channel; the channel's own floor is the median over frames, the **band's** floor is the
/// median of those (most raster channels are empty, so this is measured rather than assumed), and
/// a frame is occupied when the channel sits [`OCCUPIED_MARGIN_DB`] above it.
///
/// `None` when the window is too short to say anything, or when the measured floor is degenerate.
fn occupancy(buf: &[Complex<i8>], fs: f64, raster: f64, ks: &[i64]) -> Option<Vec<f64>> {
    let n = SWEEP_FFT_LEN;
    let frames = buf.len() / n;
    if frames == 0 || ks.is_empty() {
        return None;
    }
    let mut cfg = WelchConfig::new(n);
    cfg.overlap = 0;
    cfg.window = WindowKind::Hann;
    cfg.holds = false;
    cfg.spectral_kurtosis = false;
    let mut engine = SegmentEngine::new(cfg).ok()?;
    // Each channel's bins in the DC-centred power row: bin i is (i − N/2)·fs/N.
    let bin =
        |hz: f64| ((hz * n as f64 / fs).round() as i64 + (n / 2) as i64).clamp(0, n as i64 - 1);
    let bins: Vec<(usize, usize)> = ks
        .iter()
        .map(|&k| {
            let lo = bin(k as f64 * raster - raster / 2.0) as usize;
            let hi = bin(k as f64 * raster + raster / 2.0) as usize;
            (lo, hi.max(lo))
        })
        .collect();
    const INV: f32 = 1.0 / 128.0;
    let mut seg = vec![Complex32::default(); n];
    let mut powers: Vec<Vec<f64>> = vec![Vec::with_capacity(frames); ks.len()];
    for f in 0..frames {
        for (o, z) in seg.iter_mut().zip(&buf[f * n..(f + 1) * n]) {
            *o = Complex32::new(f32::from(z.re) * INV, f32::from(z.im) * INV);
        }
        engine.process(&seg);
        let row = engine.last_power();
        for (ci, &(lo, hi)) in bins.iter().enumerate() {
            powers[ci].push(row[lo..=hi].iter().map(|&v| f64::from(v)).sum());
        }
    }
    let floors: Vec<f64> = powers.iter().map(|p| median(p)).collect();
    let band_floor = median(&floors);
    if !band_floor.is_finite() || band_floor <= 0.0 {
        return None;
    }
    let threshold = band_floor * 10f64.powf(OCCUPIED_MARGIN_DB / 10.0);
    Some(
        powers
            .iter()
            .map(|p| p.iter().filter(|&&v| v >= threshold).count() as f64 / p.len() as f64)
            .collect(),
    )
}

fn median(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    s[s.len() / 2]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tone at `offset_hz` present for `duty` of the window, over noise.
    fn scene(n: usize, fs: f64, emissions: &[(f64, f64)]) -> Vec<Complex<i8>> {
        let mut out = vec![Complex::new(0i8, 0i8); n];
        let mut state = 0x1234_5678u64;
        let mut rand = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            ((state >> 33) as f64 / (1u64 << 31) as f64) - 1.0
        };
        for (i, s) in out.iter_mut().enumerate() {
            let (mut re, mut im) = (4.0 * rand(), 4.0 * rand());
            for &(offset, duty) in emissions {
                if (i as f64) / (n as f64) % 1.0 >= duty {
                    continue;
                }
                let ph = std::f64::consts::TAU * offset * i as f64 / fs;
                re += 40.0 * ph.cos();
                im += 40.0 * ph.sin();
            }
            *s = Complex::new(re.clamp(-128.0, 127.0) as i8, im.clamp(-128.0, 127.0) as i8);
        }
        out
    }

    /// Occupancy separates a continuous channel from a bursty one and from empty ones — which is
    /// candidacy, and candidacy alone. Nothing here confirms anything.
    #[test]
    fn occupancy_separates_continuous_from_bursty_and_empty() {
        let (fs, raster) = (500_000.0, 12_500.0);
        let n = 1 << 17;
        // +3 continuous, −5 on for a quarter of the window, everything else empty.
        let buf = scene(n, fs, &[(3.0 * raster, 1.0), (-5.0 * raster, 0.25)]);
        let ks: Vec<i64> = (-8..=8).collect();
        let fco = occupancy(&buf, fs, raster, &ks).expect("a measurable floor");
        let at = |k: i64| fco[ks.iter().position(|&x| x == k).unwrap()];
        assert!(at(3) >= MIN_CC_FCO, "continuous channel: fco {:.3}", at(3));
        assert!(
            at(-5) < MIN_CC_FCO,
            "a quarter-duty channel is not a candidate: fco {:.3}",
            at(-5)
        );
        assert!(at(7) < MIN_CC_FCO, "empty channel: fco {:.3}", at(7));
    }

    #[test]
    fn a_window_too_short_or_a_flat_floor_yields_nothing_rather_than_candidates() {
        let (fs, raster) = (500_000.0, 12_500.0);
        assert!(occupancy(&[], fs, raster, &[0]).is_none());
        assert!(occupancy(&vec![Complex::new(0i8, 0i8); 512], fs, raster, &[0]).is_none());
        // An all-zero window has no floor to measure against, so it produces no candidates at all
        // rather than declaring every channel occupied above a zero threshold.
        let quiet = vec![Complex::new(0i8, 0i8); 1 << 14];
        assert!(occupancy(&quiet, fs, raster, &[-1, 0, 1]).is_none());
    }
}
