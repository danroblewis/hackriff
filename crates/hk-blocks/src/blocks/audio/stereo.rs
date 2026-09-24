//! `stereo_decode` (ADR-0015 §12.13 LP-9, T-873): the FM stereo multiplex → left and right.
//!
//! The composite is `M + p·sin ωt + S·sin 2ωt` (ITU-R BS.450: the 38 kHz subcarrier is the
//! pilot's second harmonic, both crossing zero upward together), with `M = (L+R)/2` in 0–15 kHz,
//! the 19 kHz pilot `p`, and `S = (L−R)/2` double-sideband suppressed-carrier in 23–53 kHz.
//!
//! 1. **Pilot PLL.** `hk_demod::pilot::PilotPll` (the one `wfm` and `subcarrier` use) locks an
//!    NCO so the pilot is `a·cos θ`; then `sin ωt = cos θ`, `ωt = θ + π/2` and the subcarrier
//!    `sin 2ωt = −sin 2θ`. The PLL's lock detector is phase coherence with hysteresis, never an
//!    absolute level, so the block is indifferent to the input's scale (Hz or deviation-units).
//! 2. **L−R demodulation.** `2·x·(−sin 2θ)` brings `S` to baseband at the same scale as `M`.
//! 3. **One filter for both.** `M` and `S` are filtered as the real and imaginary parts of one
//!    complex stream through the shared DDC stages ([`Rate`], as `audio_out` uses: 15 kHz band,
//!    stopband by 18.5 kHz, so the pilot, the L−R band's image of `M` and the 76 kHz products
//!    are rejected). Both channels therefore see the identical delay and response.
//! 4. **Matrix.** `L = M + S`, `R = M − S`.
//!
//! **Honest mono fallback.** While the pilot is absent or the PLL unlocked, nothing is fed to
//! the `S` path, so `L = R = M` exactly (after the filter's own settling, a few ms, which is also
//! what keeps the switch click-free) — never an L−R guessed from an unlocked carrier. The status
//! says which: `stereo` is 1 only while the pilot is locked and `S` is being decoded, and every
//! locked → unlocked transition (a fade, a `DISCONTINUITY`, the station dropping its pilot)
//! increments `lock_losses`, so a loss between two status polls is still reported, never hidden
//! (§12.13 "Honesty").
//!
//! **Evidence** (ADR-0015 §2.1): S1 `pilot_lock`, group `pilot` (so a `subcarrier` locked to the
//! same pilot in one pipeline is one fact, §13.1): the window's phase coherence `|Σ b|/Σ |b|` of
//! the pilot phasor against the NCO, `b` averaged over each PLL update block (≈ 4 kHz, which
//! nulls the audio 4 kHz either side) — ≈ 1 for a locked pilot, ≈ `1/√n` for noise; `n` counts
//! blocks. Calibrated (no table yet: `bits` 0.0, see [`crate::evidence`]).

use hk_demod::pilot::{PilotConfig, PilotPll};
use hk_recipe::{Params, PortType};
use num_complex::{Complex32, Complex64};

use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::blocks::iq::common::*;
use crate::blocks::iq::filter::Rate;
use crate::evidence::calibrated;
use crate::registry::BuildCtx;
use crate::status::{Lock, Status};
use hk_model::synth::{EvidenceSet, GroupId, MetricId, Stage};

/// The stereo pilot, Hz (the modulation standard, not a band plan).
pub(crate) const PILOT_HZ: f64 = 19_000.0;
/// Upper edge of the L−R band, Hz (38 kHz ± 15 kHz).
const LMR_TOP_HZ: f64 = 53_000.0;
/// Stopband edge, Hz: below the 19 kHz pilot (as `audio_out`).
const STOP_HZ: f64 = 18_500.0;
/// Phase-detector update rate of the PLL and of the evidence blocks, Hz.
const UPDATE_HZ: f64 = 4_000.0;
/// Time constant of the status coherence readout, s.
const QUALITY_TAU_S: f64 = 0.25;

pub(crate) fn build(p: &Params, _: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    let out_rate = f64_or(p, "output_rate_hz", 48_000.0);
    let band = f64_or(p, "audio_bandwidth_hz", 15_000.0);
    Ok(Box::new(Stereo {
        params: p.clone(),
        band,
        pll_bandwidth_hz: f64_or(p, "pll_bandwidth_hz", 10.0),
        rate: Rate::new(out_rate, 2.0 * band, 60.0),
        fs: 0.0,
        // Replaced at `init`, when the input rate is known.
        pll: PilotPll::new(PilotConfig::default(), 240_000.0),
        block_len: 1,
        blk: Complex64::new(0.0, 0.0),
        blk_count: 0,
        q_alpha: 1.0,
        q_sum: Complex64::new(0.0, 0.0),
        q_abs: 0.0,
        ev_sum: Complex64::new(0.0, 0.0),
        ev_abs: 0.0,
        ev_n: 0,
        was_locked: false,
        lock_losses: 0,
        non_finite: 0,
        status: Status::default(),
    }))
}

struct Stereo {
    params: Params,
    band: f64,
    pll_bandwidth_hz: f64,
    rate: Rate,
    fs: f64,
    pll: PilotPll,
    /// Input samples per coherence block (the PLL's own update interval).
    block_len: usize,
    /// The block being accumulated: Σ x·e^{−jθ}.
    blk: Complex64,
    blk_count: usize,
    /// Status coherence: exponential averages of `b` and `|b|` per block.
    q_alpha: f64,
    q_sum: Complex64,
    q_abs: f64,
    /// Evidence (§2.1): Σ b, Σ |b| and the blocks since `reset()`.
    ev_sum: Complex64,
    ev_abs: f64,
    ev_n: u64,
    was_locked: bool,
    lock_losses: u64,
    non_finite: u64,
    status: Status,
}

impl Stereo {
    fn new_pll(&self) -> PilotPll {
        let config = PilotConfig {
            nominal_hz: PILOT_HZ,
            update_rate_hz: UPDATE_HZ.min(self.fs / 4.0),
            loop_bandwidth_hz: self.pll_bandwidth_hz,
            // Lock on phase coherence alone: the input's scale is the upstream block's choice.
            min_deviation_hz: 0.0,
            ..PilotConfig::default()
        };
        PilotPll::new(config, self.fs)
    }

    /// Drops every piece of history (a `DISCONTINUITY`/`RESET`, or `reset()`): the pilot phase
    /// is unknown after a gap, so the block is mono until the PLL re-locks.
    fn restart(&mut self, item: u64) {
        self.rate.restart(item);
        self.pll = self.new_pll();
        self.blk = Complex64::new(0.0, 0.0);
        self.blk_count = 0;
        self.q_sum = Complex64::new(0.0, 0.0);
        self.q_abs = 0.0;
        self.note_lock(false);
    }

    /// Records the lock state; a locked → unlocked transition is a reported loss.
    fn note_lock(&mut self, locked: bool) {
        if self.was_locked && !locked {
            self.lock_losses += 1;
        }
        self.was_locked = locked;
    }

    /// Closes one coherence block.
    #[inline]
    fn close_block(&mut self) {
        let b = self.blk / self.block_len as f64;
        let a = b.norm();
        self.ev_sum += b;
        self.ev_abs += a;
        self.ev_n += 1;
        self.q_sum += (b - self.q_sum) * self.q_alpha;
        self.q_abs += (a - self.q_abs) * self.q_alpha;
        self.blk = Complex64::new(0.0, 0.0);
        self.blk_count = 0;
    }
}

impl Block for Stereo {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = single_input(inputs, "stereo_decode", &[PortType::Real])?;
        self.fs = input.rate_hz;
        // The 76 kHz ± 15 kHz product of the L−R demodulation (top 91 kHz) must not alias into
        // the audio band: fs − (38 + 53) kHz above the stopband edge.
        let min_fs = 2.0 * PILOT_HZ + LMR_TOP_HZ + STOP_HZ;
        if self.fs < min_fs {
            return Err(BlockError::Unrealisable(format!(
                "stereo_decode needs the whole multiplex: input rate ≥ {min_fs} Hz"
            )));
        }
        if self.band * 1.05 >= STOP_HZ {
            return Err(BlockError::Params(
                "audio_bandwidth_hz must leave a transition below the 19 kHz pilot".into(),
            ));
        }
        let (max_items, hold_items) = self.rate.init(&input, 0.0, Some(STOP_HZ))?;
        self.block_len = (self.fs / UPDATE_HZ.min(self.fs / 4.0)).round().max(1.0) as usize;
        self.q_alpha = ema_alpha(QUALITY_TAU_S, self.fs / self.block_len as f64);
        self.restart(0);
        self.lock_losses = 0;
        let port = PortInfo {
            ty: PortType::Real,
            rate_hz: self.rate.out_rate,
            max_items,
            hold_items,
        };
        Ok(vec![port, port])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        let x = real_in(&input)?;
        let m = input.meta;
        if restarts(m.flags) {
            self.restart(m.index);
        }
        let first = self.rate.next_source(&m);
        let per = self.rate.per_item(&m);
        self.rate.scratch_in.clear();
        for &s in x {
            let s = finite_or_zero(s, &mut self.non_finite);
            let th = self.pll.step(s);
            let locked = self.pll.is_locked();
            let (sn, c) = th.sin_cos();
            let v = f64::from(s);
            self.blk += Complex64::new(v * c, -v * sn);
            self.blk_count += 1;
            if self.blk_count == self.block_len {
                self.close_block();
            }
            // The L−R channel is decoded only while the pilot is locked: 2·x·sin 2ωt with
            // sin 2ωt = −sin 2θ = −2 sin θ cos θ. Unlocked, S stays empty and L = R = M.
            let lmr = if locked {
                (-4.0 * v * sn * c) as f32
            } else {
                0.0
            };
            self.rate.scratch_in.push(Complex32::new(s, lmr));
            self.note_lock(locked);
        }
        self.rate.run();
        let produced = self.rate.scratch_out.len();
        for port in 0..2 {
            let out = io.output(port)?;
            set_meta(out, &m, first, per);
            let y = real_out(out)?;
            let sign = if port == 0 { 1.0 } else { -1.0 };
            y.extend(self.rate.scratch_out.iter().map(|z| z.re + sign * z.im));
        }

        let st = &mut self.status;
        st.items_in += x.len() as u64;
        st.items_out += 2 * produced as u64;
        let locked = self.was_locked;
        st.lock = if locked {
            Lock::Locked
        } else {
            Lock::Searching
        };
        st.quality = (self.q_abs > 0.0).then(|| (self.q_sum.norm() / self.q_abs).min(1.0) as f32);
        st.extra.set("pilot_locked", f64::from(u8::from(locked)));
        // The channel label a consumer reads: 1 exactly while L−R is being decoded, which in
        // this block is exactly while the pilot is locked (never "stereo" over a mono fallback).
        st.extra.set("stereo", f64::from(u8::from(locked)));
        st.extra.set("lock_losses", self.lock_losses as f64);
        let r = self.pll.report();
        if let Some(f) = r.frequency_hz {
            st.extra.set("pilot_hz", f);
        }
        if let Some(a) = r.deviation_hz {
            st.extra.set("pilot_amplitude", a);
        }
        report_non_finite(st, self.non_finite);
        Ok(())
    }

    fn reset(&mut self) {
        self.restart(0);
        self.ev_sum = Complex64::new(0.0, 0.0);
        self.ev_abs = 0.0;
        self.ev_n = 0;
    }

    fn evidence(&self, out: &mut EvidenceSet) {
        if self.ev_n > 0 && self.ev_abs > 0.0 {
            calibrated(
                out,
                Stage::S1,
                MetricId::PilotLock,
                GroupId::Pilot,
                self.ev_sum.norm() / self.ev_abs,
                self.ev_n,
            );
        }
    }

    fn update_params(&mut self, p: &Params, _: &BuildCtx<'_>) -> Result<ParamUpdate, BlockError> {
        // Every parameter shapes the filters or the loop: cold.
        if !cold_equal(&[], &self.params, p) {
            return Ok(ParamUpdate::Rebuild);
        }
        Ok(ParamUpdate::Applied)
    }

    fn status(&self) -> Status {
        self.status
    }
}

#[cfg(test)]
mod tests {
    use std::f64::consts::TAU;

    use hk_model::synth::{EvidenceSet, GroupId, MetricId, Stage};
    use hk_recipe::PortType;
    use serde_json::json;

    use crate::block::{BlockError, ParamUpdate, PortInfo};
    use crate::blocks::iq::testkit::{Chain, Lcg, assert_chunk_invariant, build, params, update};
    use crate::buffer::{ChunkFlags, PortSlice, PortVec};
    use crate::status::{Lock, Status};

    const FS: f64 = 240_000.0;
    const OUT: f64 = 48_000.0;
    /// The received pilot: a receiver clock a few ppm off makes it 19 000.3 Hz.
    const PILOT: f64 = 19_000.3;
    /// Left and right test tones, Hz, each of amplitude [`A`].
    const F_L: f64 = 1_000.0;
    const F_R: f64 = 2_500.0;
    const A: f64 = 0.4;

    /// What the composite carries.
    #[derive(Clone, Copy)]
    struct Parts {
        pilot: bool,
        lmr: bool,
    }

    const STEREO: Parts = Parts {
        pilot: true,
        lmr: true,
    };

    /// A BS.450 multiplex `M + 0.09·sin ωt + S·sin 2ωt` (deviation-normalised, as `fm_demod`
    /// with `deviation_hz` emits it) for samples `k0..k0+n`, with a small noise floor.
    fn mpx(k0: usize, n: usize, parts: Parts, seed: u64) -> Vec<f32> {
        let mut rng = Lcg::new(seed);
        (k0..k0 + n)
            .map(|k| {
                let t = k as f64 / FS;
                let l = A * (TAU * F_L * t).sin();
                let r = A * (TAU * F_R * t + 0.3).sin();
                let wt = TAU * PILOT * t + 0.7;
                let mut x = (l + r) / 2.0 + 0.001 * rng.gauss();
                if parts.pilot {
                    x += 0.09 * wt.sin();
                }
                if parts.lmr {
                    x += (l - r) / 2.0 * (2.0 * wt).sin();
                }
                x as f32
            })
            .collect()
    }

    /// Amplitude of the tone at `f` in `x` (sampled at `fs`).
    fn amp(x: &[f32], fs: f64, f: f64) -> f64 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (n, &v) in x.iter().enumerate() {
            let ph = TAU * f * n as f64 / fs;
            re += f64::from(v) * ph.cos();
            im += f64::from(v) * ph.sin();
        }
        2.0 * (re * re + im * im).sqrt() / x.len() as f64
    }

    fn extra(st: &Status, key: &str) -> Option<f64> {
        st.extra.iter().find(|(k, _)| *k == key).map(|(_, v)| v)
    }

    fn block() -> Box<dyn crate::Block> {
        build("stereo_decode", json!({}), PortType::Real)
    }

    fn chain(chunk: usize) -> Chain {
        Chain::new(
            vec![block()],
            PortInfo {
                ty: PortType::Real,
                rate_hz: FS,
                max_items: chunk,
                hold_items: 0,
            },
        )
    }

    /// Output samples after the PLL has locked and the filters settled (0.3 s).
    fn settled(x: &[f32]) -> &[f32] {
        &x[(0.3 * OUT) as usize..]
    }

    fn max_diff(a: &[f32], b: &[f32]) -> f32 {
        assert_eq!(a.len(), b.len());
        a.iter()
            .zip(b)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0, f32::max)
    }

    /// U5 / ADR-0015 §12.13: a locked pilot gives separated channels at the source level, the
    /// status labels it stereo, and the block reports S1 `pilot_lock` evidence.
    #[test]
    fn a_locked_pilot_separates_left_from_right() {
        let data = PortVec::Real(mpx(0, FS as usize, STEREO, 1));
        let c =
            assert_chunk_invariant(|| vec![block()], PortType::Real, FS, &data, &[4_096, 1_500]);
        let (l, r) = (settled(&c.out(0, 0).real), settled(&c.out(0, 1).real));
        assert!(l.len() > 30_000, "{}", l.len());
        let sep = |own: f64, other: f64| 20.0 * (own / other).log10();
        let (ll, lr) = (amp(l, OUT, F_L), amp(l, OUT, F_R));
        let (rr, rl) = (amp(r, OUT, F_R), amp(r, OUT, F_L));
        assert!((ll - A).abs() < 0.02 * A, "left level {ll}");
        assert!((rr - A).abs() < 0.02 * A, "right level {rr}");
        assert!(sep(ll, lr) > 35.0, "left separation {} dB", sep(ll, lr));
        assert!(sep(rr, rl) > 35.0, "right separation {} dB", sep(rr, rl));

        let st = c.block(0).status();
        assert_eq!(st.lock, Lock::Locked);
        assert_eq!(extra(&st, "stereo"), Some(1.0));
        assert_eq!(extra(&st, "pilot_locked"), Some(1.0));
        assert_eq!(extra(&st, "lock_losses"), Some(0.0));
        let f = extra(&st, "pilot_hz").unwrap();
        assert!((f - PILOT).abs() < 0.2, "pilot {f} Hz");
        let a = extra(&st, "pilot_amplitude").unwrap();
        assert!((a - 0.09).abs() < 0.01, "pilot amplitude {a}");
        assert!(st.quality.unwrap() > 0.95, "{:?}", st.quality);

        let mut ev = EvidenceSet::new();
        c.block(0).evidence(&mut ev);
        let e = *ev.iter().next().expect("pilot_lock evidence");
        assert_eq!(
            (e.stage, e.metric, e.group),
            (Stage::S1, MetricId::PilotLock, GroupId::Pilot)
        );
        assert!(e.raw > 0.9, "coherence {}", e.raw);
        assert_eq!(e.n, 4_000, "one block per PLL update over 1 s");
        assert_eq!(e.bits, 0.0, "calibrated: hk-synth scores it");
    }

    /// The honest fallback: a mono station (no pilot), and even an L−R subcarrier whose pilot
    /// is missing, decode as mono — left and right identical, the mono sum at its level — and
    /// the status never says stereo.
    #[test]
    fn without_a_pilot_the_output_is_mono_and_labelled_mono() {
        for parts in [
            Parts {
                pilot: false,
                lmr: false,
            },
            Parts {
                pilot: false,
                lmr: true,
            },
        ] {
            let mut c = chain(4_096);
            c.run(&PortVec::Real(mpx(0, FS as usize, parts, 2)), 4_096);
            let (l, r) = (&c.out(0, 0).real, &c.out(0, 1).real);
            assert_eq!(l, r, "left = right, sample for sample");
            let m = settled(l);
            assert!((amp(m, OUT, F_L) - A / 2.0).abs() < 0.02 * A);
            assert!((amp(m, OUT, F_R) - A / 2.0).abs() < 0.02 * A);
            let st = c.block(0).status();
            assert_eq!(st.lock, Lock::Searching);
            assert_eq!(extra(&st, "stereo"), Some(0.0));
            assert_eq!(extra(&st, "pilot_locked"), Some(0.0));
            assert_eq!(extra(&st, "pilot_hz"), None, "no pilot measured");
        }
    }

    /// §12.13 "losing lock mid-stream is reported, never hidden": the pilot disappears half-way
    /// (its L−R subcarrier stays, so decoding it anyway would be visible); the status flips to
    /// mono, counts the loss, and the channels collapse to identical mono.
    #[test]
    fn losing_the_pilot_mid_stream_is_reported_and_falls_back_to_mono() {
        let chunk = 4_800;
        let mut c = chain(chunk);
        let half = FS as usize;
        let on = mpx(0, half, STEREO, 3);
        let off = mpx(
            half,
            half,
            Parts {
                pilot: false,
                lmr: true,
            },
            4,
        );
        for (i, x) in on.chunks(chunk).enumerate() {
            let flags = if i == 0 {
                ChunkFlags::DISCONTINUITY
            } else {
                ChunkFlags::NONE
            };
            c.feed(PortSlice::Real(x), flags);
        }
        let st = c.block(0).status();
        assert_eq!((st.lock, extra(&st, "stereo")), (Lock::Locked, Some(1.0)));
        let stereo_len = c.out(0, 0).real.len();
        for x in off.chunks(chunk) {
            c.feed(PortSlice::Real(x), ChunkFlags::NONE);
        }
        let st = c.block(0).status();
        assert_eq!(st.lock, Lock::Searching);
        assert_eq!(extra(&st, "stereo"), Some(0.0));
        assert_eq!(extra(&st, "lock_losses"), Some(1.0), "the loss is counted");
        let (l, r) = (&c.out(0, 0).real, &c.out(0, 1).real);
        // Stereo before the loss; mono (identical channels) 0.2 s after it.
        assert!(amp(settled(&l[..stereo_len]), OUT, F_R) < 0.02 * A);
        let tail = stereo_len + (0.2 * OUT) as usize;
        assert!(
            max_diff(&l[tail..], &r[tail..]) < 1e-6,
            "left = right after the loss"
        );
        assert!((amp(&l[tail..], OUT, F_R) - A / 2.0).abs() < 0.02 * A);
    }

    /// A gap (`DISCONTINUITY`) invalidates the pilot phase: mono until the PLL re-locks, and
    /// the drop is counted like any other loss of lock.
    #[test]
    fn a_discontinuity_drops_to_mono_until_relock() {
        let chunk = 4_096;
        let mut c = chain(chunk);
        c.run(&PortVec::Real(mpx(0, FS as usize, STEREO, 5)), chunk);
        assert_eq!(c.block(0).status().lock, Lock::Locked);
        let after = mpx(FS as usize + 1_000, chunk, STEREO, 6);
        c.feed(PortSlice::Real(&after), ChunkFlags::DISCONTINUITY);
        let st = c.block(0).status();
        assert_eq!(
            (st.lock, extra(&st, "stereo")),
            (Lock::Searching, Some(0.0))
        );
        assert_eq!(extra(&st, "lock_losses"), Some(1.0));
        let rest = mpx(FS as usize + 1_000 + chunk, FS as usize / 2, STEREO, 7);
        for x in rest.chunks(chunk) {
            c.feed(PortSlice::Real(x), ChunkFlags::NONE);
        }
        let st = c.block(0).status();
        assert_eq!((st.lock, extra(&st, "stereo")), (Lock::Locked, Some(1.0)));
    }

    /// Noise never locks, stays mono, and its pilot evidence is near the `1/√n` null.
    #[test]
    fn noise_never_locks_and_scores_little_pilot_evidence() {
        let mut rng = Lcg::new(8);
        let x: Vec<f32> = (0..FS as usize)
            .map(|_| (0.3 * rng.gauss()) as f32)
            .collect();
        let mut c = chain(4_096);
        c.run(&PortVec::Real(x), 4_096);
        assert_eq!(c.out(0, 0).real, c.out(0, 1).real);
        let st = c.block(0).status();
        assert_eq!(st.lock, Lock::Searching);
        assert_eq!(extra(&st, "lock_losses"), Some(0.0), "never locked");
        let mut ev = EvidenceSet::new();
        c.block(0).evidence(&mut ev);
        let e = *ev.iter().next().unwrap();
        assert!(e.raw < 0.2, "noise coherence {}", e.raw);
        assert!(e.n > 3_900);
    }

    #[test]
    fn refuses_a_rate_without_the_multiplex_and_every_param_is_cold() {
        let mut b = block();
        let at = |rate_hz| PortInfo {
            ty: PortType::Real,
            rate_hz,
            max_items: 4_096,
            hold_items: 0,
        };
        assert!(matches!(
            b.init(&[at(96_000.0)]),
            Err(BlockError::Unrealisable(_))
        ));
        let ports = b.init(&[at(FS)]).unwrap();
        assert_eq!(ports.len(), 2);
        assert!(
            ports
                .iter()
                .all(|p| p.ty == PortType::Real && p.rate_hz == OUT)
        );
        assert_eq!(
            update(b.as_mut(), json!({}), PortType::Real),
            ParamUpdate::Applied
        );
        assert_eq!(
            update(b.as_mut(), json!({"pll_bandwidth_hz": 20}), PortType::Real),
            ParamUpdate::Rebuild
        );
        let reg = crate::Registry::builtin();
        let maps = std::collections::BTreeMap::new();
        let ctx = crate::BuildCtx {
            field_maps: &maps,
            input_types: &[PortType::Real],
        };
        assert!(
            reg.build(
                "stereo_decode",
                &params(json!({"audio_bandwidth_hz": 20000})),
                &ctx
            )
            .is_err()
        );
    }
}
