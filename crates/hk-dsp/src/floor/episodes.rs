//! Floor-change episodes (AWARE-006): a per-block level classifier feeding a region aggregator.
//! The event model is specified in the [module docs](super#floor-change-events); this file
//! implements it.

use std::ops::Range;

use hk_core::ProvenanceHandle;
use hk_model::SampleTime;

use super::tracker::{
    EndReason, FloorChangeClass, FloorChangeConfig, FloorEvent, FloorEventKind, FloorStats,
    GainKey, SlowFloorConfig,
};
use super::{BlockLayout, db_ratio, median_in_place};
use crate::spectrum::Spectrum;

/// A frame's `(seq, t)`.
pub(crate) type Stamp = (u64, SampleTime);

/// Settings in frames, derived at each segment start.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Timing {
    pub confirm: u64,
    pub end: u64,
    pub holdoff: u64,
    pub settle: u64,
    pub alpha: f64,
    pub alpha_long: f64,
    /// EMA factor of a run's excess (time constant `confirm_s / 3`).
    pub ema: f32,
    /// EMA factor of a block's recent level (time constant 0.1 s).
    pub recent: f32,
    pub rebaseline: Option<u64>,
    /// Hit frames (below −threshold) a fall needs in its last `confirm` frames.
    pub fall_hits: u64,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            confirm: 1,
            end: 1,
            holdoff: 0,
            settle: 1,
            alpha: 1.0,
            alpha_long: 1.0,
            ema: 1.0,
            recent: 1.0,
            rebaseline: None,
            fall_hits: 1,
        }
    }
}

/// What every event needs from the frame that produces it.
pub(crate) struct EventCtx<'a> {
    pub now: Stamp,
    pub provenance: &'a ProvenanceHandle,
    pub segment: u64,
    pub fs: f64,
    pub uncertainty_db: f32,
}

/// One post-warm-up frame.
pub(crate) struct Frame<'a> {
    pub ev: EventCtx<'a>,
    pub counter: u64,
    pub impulsive: bool,
    /// Raw per-frame block floors.
    pub floor: &'a [f32],
    pub spectrum: &'a Spectrum,
    pub layout: &'a BlockLayout,
    pub gain: GainKey,
    pub stat_block_db: f32,
    pub quantisation_db: Option<f64>,
    pub quantisation_margin_db: f64,
    /// Slow-floor updates in this segment so far (cumulative-mean start).
    pub updates: u64,
}

/// A pending excursion of one block: `dir` +1 (rise) or −1 (fall), 0 idle.
#[derive(Clone, Copy, Debug)]
struct Run {
    dir: i8,
    onset: Stamp,
    frames: u64,
    hits: u64,
    last_hit: u64,
    base: f32,
    ema: f32,
    level_sum: f64,
    level_n: u64,
    recent: f32,
    last_e: f32,
    sum_d: f64,
    sum_d2: f64,
    n_d: u64,
    sum_sk: f64,
    sk_n: u64,
    /// Fall: hit frames among the last `confirm` frames (the engine's ring holds their excess).
    win_hits: u64,
    /// Fall: level set at confirmation (quantile of all window frames).
    fall_level: f32,
}

impl Run {
    fn idle(t: SampleTime) -> Self {
        Self {
            dir: 0,
            onset: (0, t),
            frames: 0,
            hits: 0,
            last_hit: 0,
            base: 1.0,
            ema: 0.0,
            level_sum: 0.0,
            level_n: 0,
            recent: 0.0,
            last_e: f32::NAN,
            sum_d: 0.0,
            sum_d2: 0.0,
            n_d: 0,
            sum_sk: 0.0,
            sk_n: 0,
            win_hits: 0,
            fall_level: 0.0,
        }
    }

    fn start(dir: i8, base: f32, onset: Stamp, counter: u64) -> Self {
        Self {
            dir,
            onset,
            base,
            last_hit: counter,
            ..Self::idle(onset.1)
        }
    }

    /// A rise: mean level over its non-impulsive frames. A fall: the level set at confirmation
    /// (see [`Engine::confirm_falls`]).
    fn level(&self) -> f32 {
        if self.dir < 0 && self.fall_level > 0.0 {
            self.fall_level
        } else if self.level_n > 0 {
            (self.level_sum / self.level_n as f64) as f32
        } else {
            self.base
        }
    }

    /// Fall: fraction of hit frames over the last `cap` frames.
    fn hit_fraction(&self, cap: usize) -> f64 {
        self.win_hits as f64 / self.frames.min(cap as u64).max(1) as f64
    }

    /// A rise whose recent excess is beyond the threshold, or a fall with a majority of hit
    /// frames in its window: it looks like a confirming change (blocks slow-floor adoption).
    fn beyond(&self, thr: f32, cap: usize) -> bool {
        if self.dir > 0 {
            self.level_n > 0 && self.ema >= thr
        } else {
            self.dir < 0 && self.frames > 0 && 2 * self.win_hits >= self.frames.min(cap as u64)
        }
    }

    fn confirmable(&self, t: &Timing, thr: f32) -> bool {
        match self.dir {
            1 => self.frames >= t.confirm && self.beyond(thr, t.confirm as usize),
            -1 => self.frames >= t.confirm && self.win_hits >= t.fall_hits,
            _ => false,
        }
    }

    /// Fall: records this frame's excess in the block's ring (`ring.len()` = window).
    fn record(&mut self, ring: &mut [f32], e: f32, thr: f32) {
        let cap = ring.len();
        if cap == 0 || self.frames == 0 {
            return;
        }
        let slot = ((self.frames - 1) % cap as u64) as usize;
        if self.frames > cap as u64 && ring[slot] < -thr {
            self.win_hits -= 1;
        }
        ring[slot] = e;
        if e < -thr {
            self.win_hits += 1;
        }
    }

    /// Variance of the first differences of the excess, dB² (= 2 σ² for white excess noise;
    /// a level trend adds nothing).
    fn var_d(&self) -> Option<f64> {
        (self.n_d > 0).then(|| {
            let n = self.n_d as f64;
            let m = self.sum_d / n;
            (self.sum_d2 / n - m * m).max(0.0)
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        e: f32,
        f: f32,
        impulsive: bool,
        sk: Option<f64>,
        counter: u64,
        thr: f32,
        t: &Timing,
    ) {
        self.frames += 1;
        let hit = if self.dir > 0 { e > thr } else { e < -thr };
        if hit {
            self.hits += 1;
            self.last_hit = counter;
        }
        if impulsive || !(self.dir > 0 || hit) {
            self.last_e = f32::NAN;
            return;
        }
        if let Some(s) = sk {
            self.sum_sk += s;
            self.sk_n += 1;
        }
        if self.last_e.is_finite() {
            let d = f64::from(e - self.last_e);
            self.sum_d += d;
            self.sum_d2 += d * d;
            self.n_d += 1;
        }
        self.last_e = e;
        self.level_n += 1;
        self.level_sum += f64::from(f);
        if self.level_n == 1 {
            self.ema = e;
            self.recent = f;
        } else {
            self.ema += t.ema * (e - self.ema);
            self.recent += t.recent * (f - self.recent);
        }
    }
}

/// A sub-threshold excursion of a block from its slow floor (the slow-floor gate).
#[derive(Clone, Copy, Debug, Default)]
struct Settle {
    dir: i8,
    frames: u64,
    sum: f64,
    n: u64,
}

/// One frame of the slow-floor gate for a block. Returns `(frozen, adopted)`: `frozen` when the
/// slow floor must not follow this frame, `adopted` when the gate re-seeded it.
#[allow(clippy::too_many_arguments)]
fn settle_step(
    st: &mut Settle,
    f: f32,
    slow: &mut f32,
    impulsive: bool,
    blocked: bool,
    cfg: &SlowFloorConfig,
    thr: f32,
    frames: u64,
) -> (bool, bool) {
    let e = db_ratio(f, *slow);
    let sdb = cfg.settle_db as f32;
    if st.dir != 0 && e * f32::from(st.dir) >= sdb / 2.0 {
        // The excursion continues (hysteresis at half the settle level).
    } else if e.abs() > sdb {
        *st = Settle {
            dir: if e > 0.0 { 1 } else { -1 },
            ..Settle::default()
        };
    } else {
        *st = Settle::default();
        return (false, false);
    }
    st.frames += 1;
    if !impulsive && e.abs() < thr {
        st.sum += f64::from(f);
        st.n += 1;
    }
    if st.frames >= frames && !blocked {
        let adopt = st.n > 0 && 2 * st.n >= st.frames;
        if adopt {
            *slow = (st.sum / st.n as f64) as f32;
        }
        *st = Settle::default();
        return (!adopt, adopt);
    }
    (true, false)
}

#[derive(Clone, Copy, Debug)]
struct Block {
    /// Episode slot this block belongs to.
    member: Option<usize>,
    /// Frame counter before which this block cannot seed a confirmation.
    holdoff_until: u64,
    /// Long reference (IIR of the slow floor, frozen during excursions).
    long: f32,
    run: Run,
    settle: Settle,
    /// Member: the pre-episode baseline.
    base: f32,
    /// Member: drift index at joining.
    g0: f64,
    /// Member: recent level (fast EMA).
    recent: f32,
    /// Member of a structured group: the slow floor stays at the baseline.
    hold_slow: bool,
    ret_frames: u64,
    ret_onset: Stamp,
    ret_sum: f64,
    ret_used: u64,
    /// Reference carried across a comparable reset.
    carry: f32,
    /// Member: onset of the run it joined with.
    onset: Stamp,
    /// Scratch: a member of the surviving episode before a group joined.
    mark: bool,
}

impl Block {
    fn new(t: SampleTime) -> Self {
        Self {
            member: None,
            holdoff_until: 0,
            long: 1.0,
            run: Run::idle(t),
            settle: Settle::default(),
            base: 1.0,
            g0: 0.0,
            recent: 1.0,
            hold_slow: false,
            ret_frames: 0,
            ret_onset: (0, t),
            ret_sum: 0.0,
            ret_used: 0,
            carry: 1.0,
            onset: (0, t),
            mark: false,
        }
    }

    fn clear_excursions(&mut self, t: SampleTime) {
        self.run = Run::idle(t);
        self.settle = Settle::default();
        self.ret_frames = 0;
    }
}

#[derive(Clone, Debug)]
struct Episode {
    active: bool,
    suspended: bool,
    id: u64,
    class: FloorChangeClass,
    emitted: bool,
    onset: Stamp,
    /// Frame counter at confirmation (rebaseline clock).
    opened: u64,
    members: u32,
    bins: Range<usize>,
    f_lo_hz: f64,
    f_hi_hz: f64,
    band_fraction: f32,
    baseline_db: f32,
    level_db: f32,
    step_db: f32,
    peak_step_db: f32,
    step_unc_db: f32,
    sk: Option<f32>,
    excess_std_db: f32,
    q_limited_before: bool,
    baseline_segment: u64,
    gain: GainKey,
    leave_n: u32,
    leave_onset: Stamp,
    leave_lo: usize,
    leave_hi: usize,
    /// Some member is part-way through its return this frame.
    returning: bool,
}

impl Episode {
    fn inactive(t: SampleTime, gain: GainKey) -> Self {
        Self {
            active: false,
            suspended: false,
            id: 0,
            class: FloorChangeClass::Unverified,
            emitted: false,
            onset: (0, t),
            opened: 0,
            members: 0,
            bins: 0..0,
            f_lo_hz: 0.0,
            f_hi_hz: 0.0,
            band_fraction: 0.0,
            baseline_db: 0.0,
            level_db: 0.0,
            step_db: 0.0,
            peak_step_db: 0.0,
            step_unc_db: 0.0,
            sk: None,
            excess_std_db: 0.0,
            q_limited_before: false,
            baseline_segment: 0,
            gain,
            leave_n: 0,
            leave_onset: (0, t),
            leave_lo: usize::MAX,
            leave_hi: 0,
            returning: false,
        }
    }

    /// Records a leaving block. The onset reported is the latest return onset among the blocks
    /// leaving together: the frame from which all of them stayed back.
    fn note_leave(&mut self, b: usize, onset: Stamp) {
        if self.leave_n == 0 || onset.0 > self.leave_onset.0 {
            self.leave_onset = onset;
        }
        self.leave_n += 1;
        self.leave_lo = self.leave_lo.min(b);
        self.leave_hi = self.leave_hi.max(b + 1);
    }

    #[allow(clippy::too_many_arguments)]
    fn event(
        &self,
        kind: FloorEventKind,
        onset: Stamp,
        ctx: &EventCtx,
        change_bins: Range<usize>,
        end_reason: Option<EndReason>,
        merged_into: Option<u64>,
        interrupted: bool,
    ) -> FloorEvent {
        let secs = |a: SampleTime, b: SampleTime| {
            b.sample_index.saturating_sub(a.sample_index) as f64 / ctx.fs
        };
        let duration_s = match kind {
            FloorEventKind::Rise | FloorEventKind::Extend | FloorEventKind::Fall => {
                secs(onset.1, ctx.now.1)
            }
            FloorEventKind::End => secs(self.onset.1, onset.1),
            FloorEventKind::Update | FloorEventKind::Unknown => secs(self.onset.1, ctx.now.1),
        };
        // Bins are uniform, so the change edges follow from the extent's edges.
        let bw = (self.f_hi_hz - self.f_lo_hz) / self.bins.len().max(1) as f64;
        let at = |bin: usize| self.f_lo_hz + (bin as f64 - self.bins.start as f64) * bw;
        let (change_f_lo_hz, change_f_hi_hz) = (at(change_bins.start), at(change_bins.end));
        FloorEvent {
            kind,
            episode: self.id,
            class: self.class,
            end_reason,
            merged_into,
            split_from: None,
            interrupted,
            onset_seq: onset.0,
            onset_t: onset.1,
            confirmed_seq: ctx.now.0,
            confirmed_t: ctx.now.1,
            episode_onset_t: self.onset.1,
            duration_s,
            bins: self.bins.clone(),
            change_bins,
            change_f_lo_hz,
            change_f_hi_hz,
            f_lo_hz: self.f_lo_hz,
            f_hi_hz: self.f_hi_hz,
            band_fraction: self.band_fraction,
            baseline_dbfs_per_hz: self.baseline_db,
            baseline_segment: self.baseline_segment,
            level_dbfs_per_hz: self.level_db,
            step_db: self.step_db,
            peak_step_db: self.peak_step_db,
            step_uncertainty_db: self.step_unc_db,
            uncertainty_db: ctx.uncertainty_db,
            sk: self.sk,
            excess_std_db: self.excess_std_db,
            quantisation_limited_before: self.q_limited_before,
            gain: self.gain,
            segment: ctx.segment,
            provenance: ctx.provenance.clone(),
        }
    }
}

/// Summary of the new blocks of a confirmed group.
struct Group {
    onset: Stamp,
    baseline_db: f32,
    level_db: f32,
    excess_std_db: f32,
    step_unc_db: f32,
    sk: Option<f32>,
    class: FloorChangeClass,
    q_before: bool,
}

fn db(x: f32) -> f32 {
    10.0 * x.max(1e-37).log10()
}

fn classify(sk: Option<f32>, excess_std_db: f32, cfg: &FloorChangeConfig) -> FloorChangeClass {
    if f64::from(excess_std_db) > cfg.max_excess_std_db {
        return FloorChangeClass::Structured;
    }
    match sk {
        None => FloorChangeClass::Unverified,
        Some(s) if (f64::from(s) - 1.0).abs() <= cfg.sk_tolerance => FloorChangeClass::NoiseLike,
        Some(_) => FloorChangeClass::Structured,
    }
}

/// Mean SK over a block's bins, when the spectrum carries SK.
fn block_sk(sk: &[f32], range: Range<usize>) -> Option<f64> {
    if sk.len() < range.end {
        return None;
    }
    let (mut s, mut n) = (0.0f64, 0u32);
    for &v in &sk[range] {
        if v.is_finite() {
            s += f64::from(v);
            n += 1;
        }
    }
    (n > 0).then(|| s / f64::from(n))
}

/// Bins covered by blocks `b0..b1`: from half a hop below the first centre to half a hop above
/// the last, extended to the span edges for the outermost blocks.
pub(crate) fn region_bins(layout: &BlockLayout, b0: usize, b1: usize) -> Range<usize> {
    let bins = layout.bins();
    let half_hop = layout.hop_bins() as f64 / 2.0;
    let lo = if b0 == 0 {
        0
    } else {
        (layout.centre(b0) - half_hop).round() as usize
    };
    let hi = if b1 == layout.count() {
        bins
    } else {
        ((layout.centre(b1 - 1) + half_hop).round() as usize).min(bins)
    };
    lo..hi.max(lo + 1)
}

fn edges(spectrum: &Spectrum, bins: &Range<usize>) -> (f64, f64) {
    let bw = spectrum.bin_width_hz();
    (
        spectrum.bin_frequency_hz(bins.start) - bw / 2.0,
        spectrum.bin_frequency_hz(bins.end - 1) + bw / 2.0,
    )
}

/// The per-block classifier and region aggregator of one tracker.
#[derive(Debug, Default)]
pub(crate) struct Engine {
    blocks: Vec<Block>,
    episodes: Vec<Episode>,
    next_id: u64,
    /// Drift index: running sum of the per-frame median slow-floor change of idle blocks, dB.
    drift: f64,
    /// The current segment finished its warm-up.
    ready: bool,
    carry_pending: bool,
    reset_at: Option<Stamp>,
    s1: Vec<f32>,
    s2: Vec<f32>,
    prev: Vec<f32>,
    /// Per block, the excess of a pending fall's last `cap` frames (dB).
    ring: Vec<f32>,
    cap: usize,
    ring_scratch: Vec<f32>,
}

impl Engine {
    /// Sizes the fall window to `cap` frames per block (at a segment start; allocates only when
    /// the window grows).
    pub(crate) fn set_window(&mut self, cap: usize) {
        if cap != self.cap {
            let nb = self.blocks.len();
            self.cap = cap;
            self.ring.resize(nb * cap, 0.0);
            self.ring_scratch.resize(cap, 0.0);
            for blk in &mut self.blocks {
                if blk.run.dir < 0 {
                    blk.run.dir = 0;
                }
            }
        }
    }

    /// Sizes for `nb` blocks, dropping all state.
    pub(crate) fn resize(&mut self, nb: usize, t: SampleTime, gain: GainKey) {
        self.blocks.clear();
        self.blocks.resize(nb, Block::new(t));
        self.episodes.clear();
        self.episodes.resize(nb, Episode::inactive(t, gain));
        self.s1.resize(nb, 0.0);
        self.s2.resize(nb, 0.0);
        self.prev.resize(nb, 0.0);
        self.cap = 0;
        self.ready = false;
        self.carry_pending = false;
        self.drift = 0.0;
    }

    /// Open episodes (including those suspended across a reset).
    pub(crate) fn active_count(&self) -> u32 {
        self.episodes.iter().filter(|e| e.active).count() as u32
    }

    /// A segment reset. Comparable (same receiver state): open episodes are suspended and every
    /// block's reference is carried to the end of the next warm-up. Otherwise every open
    /// episode closes with `Unknown`.
    pub(crate) fn on_reset(
        &mut self,
        comparable: bool,
        ctx: &EventCtx,
        slow: &[f32],
        stats: &mut FloorStats,
        on_event: &mut impl FnMut(&FloorEvent),
    ) {
        if comparable && (self.ready || self.carry_pending) {
            if !self.carry_pending {
                for (blk, &s) in self.blocks.iter_mut().zip(slow) {
                    blk.carry = if blk.member.is_some() { blk.base } else { s };
                }
                for ep in self.episodes.iter_mut().filter(|e| e.active) {
                    ep.suspended = true;
                }
                self.carry_pending = true;
                self.reset_at = Some(ctx.now);
            }
        } else {
            for ep in self.episodes.iter_mut().filter(|e| e.active) {
                ep.active = false;
                ep.suspended = false;
                stats.episode_ends += 1;
                if ep.emitted {
                    let ev = ep.event(
                        FloorEventKind::Unknown,
                        ctx.now,
                        ctx,
                        ep.bins.clone(),
                        None,
                        None,
                        false,
                    );
                    on_event(&ev);
                    stats.unknown_events += 1;
                }
            }
            for blk in &mut self.blocks {
                blk.member = None;
            }
            self.carry_pending = false;
            self.reset_at = None;
        }
        self.ready = false;
        for blk in &mut self.blocks {
            blk.clear_excursions(ctx.now.1);
            blk.holdoff_until = 0;
        }
    }

    /// Fills `s1`/`s2` with the members' baselines and recent levels; returns
    /// `(count, first, end)`.
    fn members(&mut self, i: usize) -> (usize, usize, usize) {
        let (mut n, mut b0, mut b1) = (0, usize::MAX, 0);
        for (b, blk) in self.blocks.iter().enumerate() {
            if blk.member == Some(i) {
                self.s1[n] = blk.base;
                self.s2[n] = blk.recent;
                n += 1;
                b0 = b0.min(b);
                b1 = b + 1;
            }
        }
        (n, b0, b1)
    }

    /// Recomputes episode `i`'s extent, baseline and level from its members (no-op without
    /// members).
    fn summarize(&mut self, i: usize, layout: &BlockLayout, spectrum: &Spectrum) {
        let (n, b0, b1) = self.members(i);
        self.episodes[i].members = n as u32;
        if n == 0 {
            return;
        }
        let mut covered = 0;
        let mut b = b0;
        while b < b1 {
            if self.blocks[b].member == Some(i) {
                let s = b;
                while b < b1 && self.blocks[b].member == Some(i) {
                    b += 1;
                }
                covered += region_bins(layout, s, b).len();
            } else {
                b += 1;
            }
        }
        let baseline = median_in_place(&mut self.s1[..n]);
        let level = median_in_place(&mut self.s2[..n]);
        let bins = region_bins(layout, b0, b1);
        let ep = &mut self.episodes[i];
        (ep.f_lo_hz, ep.f_hi_hz) = edges(spectrum, &bins);
        ep.bins = bins;
        ep.band_fraction = covered as f32 / layout.bins() as f32;
        ep.baseline_db = db(baseline);
        ep.level_db = db(level);
        ep.step_db = ep.level_db - ep.baseline_db;
    }

    /// End of a segment's warm-up: restores or closes suspended episodes, re-seeds carried
    /// references, and starts runs backdated to the first beyond-threshold warm-up frame.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_segment(
        &mut self,
        fr: &Frame,
        slow: &mut [f32],
        warm: &[f32],
        stamps: &[Stamp],
        cfg: &FloorChangeConfig,
        timing: &Timing,
        stats: &mut FloorStats,
        on_event: &mut impl FnMut(&FloorEvent),
    ) {
        let nb = self.blocks.len();
        let w = stamps.len();
        let thr = cfg.threshold_db as f32;
        let end_thr = cfg.end_threshold_db as f32;
        self.drift = 0.0;
        for blk in &mut self.blocks {
            blk.clear_excursions(fr.ev.now.1);
            blk.holdoff_until = 0;
        }
        if self.carry_pending {
            let reset_at = self.reset_at.unwrap_or(fr.ev.now);
            for ep in &mut self.episodes {
                ep.leave_n = 0;
                ep.leave_lo = usize::MAX;
                ep.leave_hi = 0;
            }
            for (b, s) in slow.iter_mut().enumerate() {
                let blk = &mut self.blocks[b];
                if let Some(i) = blk.member {
                    if db_ratio(*s, blk.carry) >= end_thr {
                        blk.base = blk.carry;
                        blk.g0 = 0.0;
                        blk.recent = *s;
                        if blk.hold_slow {
                            *s = blk.carry;
                        }
                    } else {
                        blk.member = None;
                        self.episodes[i].note_leave(b, reset_at);
                    }
                } else if db_ratio(*s, blk.carry).abs() > thr {
                    *s = blk.carry;
                }
            }
            for i in 0..self.episodes.len() {
                if !self.episodes[i].suspended {
                    continue;
                }
                self.episodes[i].suspended = false;
                self.settle_leaves(i, reset_at, EndReason::Reset, fr, stats, on_event);
            }
            self.carry_pending = false;
            self.reset_at = None;
        }
        for b in 0..nb {
            self.blocks[b].long = slow[b];
            if self.blocks[b].member.is_some() {
                continue;
            }
            let s = slow[b];
            for dir in [1i8, -1] {
                let beyond = |v: f32| {
                    let e = db_ratio(v, s);
                    if dir > 0 { e > thr } else { e < -thr }
                };
                let mut c = 0;
                while c < w && beyond(warm[(w - 1 - c) * nb + b]) {
                    c += 1;
                }
                if c == 0 {
                    continue;
                }
                let j0 = w - c;
                let counter = |j: usize| fr.counter.saturating_sub((w - 1 - j) as u64);
                let mut r = Run::start(dir, s, stamps[j0], counter(j0));
                let cap = self.cap;
                for j in j0..w {
                    let v = warm[j * nb + b];
                    let e = db_ratio(v, s);
                    r.push(e, v, false, None, counter(j), thr, timing);
                    if dir < 0 {
                        r.record(&mut self.ring[b * cap..(b + 1) * cap], e, thr);
                    }
                }
                self.blocks[b].run = r;
                break;
            }
        }
        self.ready = true;
    }

    /// One post-warm-up frame.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn step(
        &mut self,
        fr: &Frame,
        slow: &mut [f32],
        cfg: &FloorChangeConfig,
        slow_cfg: &SlowFloorConfig,
        timing: &Timing,
        stats: &mut FloorStats,
        on_event: &mut impl FnMut(&FloorEvent),
    ) {
        let nb = self.blocks.len();
        self.prev.copy_from_slice(slow);
        let a = timing.alpha.max(1.0 / (fr.updates as f64 + 1.0)) as f32;
        let a_long = timing.alpha_long.max(1.0 / (fr.updates as f64 + 1.0)) as f32;
        for b in 0..nb {
            if self.blocks[b].member.is_some() {
                self.member_block(b, fr, slow, cfg, slow_cfg, timing, a, stats);
            } else {
                self.idle_block(b, fr, slow, cfg, slow_cfg, timing, a, a_long, stats);
            }
        }
        self.update_drift(slow);
        self.returns(fr, slow, timing, stats, on_event);
        self.confirm_rises(fr, slow, cfg, timing, stats, on_event);
        self.confirm_falls(fr, slow, cfg, timing, stats, on_event);
        self.rebaseline(fr, slow, timing, stats, on_event);
        if !fr.impulsive {
            self.track_peaks(fr);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn idle_block(
        &mut self,
        b: usize,
        fr: &Frame,
        slow: &mut [f32],
        cfg: &FloorChangeConfig,
        slow_cfg: &SlowFloorConfig,
        timing: &Timing,
        a: f32,
        a_long: f32,
        stats: &mut FloorStats,
    ) {
        let thr = cfg.threshold_db as f32;
        let end_thr = cfg.end_threshold_db as f32;
        let f = fr.floor[b];
        let sk = block_sk(&fr.spectrum.sk, fr.layout.range(b));
        let cap = self.cap;
        let ring = &mut self.ring[b * cap..(b + 1) * cap];
        let blk = &mut self.blocks[b];
        let mut r = blk.run;
        if r.dir != 0 {
            let e = db_ratio(f, r.base);
            let keep = if r.dir > 0 {
                e >= end_thr
            } else {
                e <= -end_thr || fr.counter - r.last_hit <= timing.confirm
            };
            if keep {
                r.push(e, f, fr.impulsive, sk, fr.counter, thr, timing);
                if r.dir < 0 {
                    r.record(ring, e, thr);
                }
            } else {
                r.dir = 0;
            }
        }
        if r.dir == 0 {
            let s = slow[b];
            let (lo, hi) = (s.min(blk.long), s.max(blk.long));
            let (eu, ed) = (db_ratio(f, lo), db_ratio(f, hi));
            if eu > thr {
                r = Run::start(1, lo, fr.ev.now, fr.counter);
                r.push(eu, f, fr.impulsive, sk, fr.counter, thr, timing);
            } else if ed < -thr {
                r = Run::start(-1, hi, fr.ev.now, fr.counter);
                r.push(ed, f, fr.impulsive, sk, fr.counter, thr, timing);
                r.record(ring, ed, thr);
            }
        }
        let blocked = r.dir != 0 && r.beyond(thr, cap);
        let (frozen, adopted) = settle_step(
            &mut blk.settle,
            f,
            &mut slow[b],
            fr.impulsive,
            blocked,
            slow_cfg,
            thr,
            timing.settle,
        );
        if adopted {
            r = Run::idle(fr.ev.now.1);
            stats.adoptions += 1;
        }
        if r.dir == 0 && !frozen && !adopted && !fr.impulsive {
            slow[b] += a * (f - slow[b]);
        }
        if r.dir == 0 && !frozen {
            blk.long += a_long * (slow[b] - blk.long);
        }
        blk.run = r;
    }

    #[allow(clippy::too_many_arguments)]
    fn member_block(
        &mut self,
        b: usize,
        fr: &Frame,
        slow: &mut [f32],
        cfg: &FloorChangeConfig,
        slow_cfg: &SlowFloorConfig,
        timing: &Timing,
        a: f32,
        stats: &mut FloorStats,
    ) {
        let thr = cfg.threshold_db as f32;
        let end_thr = cfg.end_threshold_db as f32;
        let f = fr.floor[b];
        let drift = self.drift;
        let blk = &mut self.blocks[b];
        let e_ret = db_ratio(f, blk.base) - (drift - blk.g0) as f32;
        if e_ret < end_thr {
            if blk.ret_frames == 0 {
                blk.ret_onset = fr.ev.now;
                blk.ret_sum = 0.0;
                blk.ret_used = 0;
            }
            blk.ret_frames += 1;
            if !fr.impulsive {
                blk.ret_sum += f64::from(f);
                blk.ret_used += 1;
            }
        } else {
            blk.ret_frames = 0;
        }
        if !fr.impulsive {
            blk.recent += timing.recent * (f - blk.recent);
        }
        if !blk.hold_slow && blk.ret_frames == 0 {
            let (frozen, adopted) = settle_step(
                &mut blk.settle,
                f,
                &mut slow[b],
                fr.impulsive,
                false,
                slow_cfg,
                thr,
                timing.settle,
            );
            stats.adoptions += u64::from(adopted);
            if !frozen && !adopted && !fr.impulsive {
                slow[b] += a * (f - slow[b]);
            }
        }
    }

    fn update_drift(&mut self, slow: &[f32]) {
        let nb = self.blocks.len();
        let mut k = 0;
        for (b, blk) in self.blocks.iter().enumerate() {
            if blk.member.is_none() && blk.run.dir == 0 {
                self.s1[k] = db_ratio(slow[b], self.prev[b]);
                k += 1;
            }
        }
        if k >= (nb / 10).max(4) {
            self.drift += f64::from(median_in_place(&mut self.s1[..k]));
        }
    }

    fn returns(
        &mut self,
        fr: &Frame,
        slow: &mut [f32],
        timing: &Timing,
        stats: &mut FloorStats,
        on_event: &mut impl FnMut(&FloorEvent),
    ) {
        // One physical return, one event: a member that completed its return waits while another
        // member of its episode is part-way through (edge blocks cross the threshold a few
        // frames apart).
        for ep in &mut self.episodes {
            ep.returning = false;
        }
        for blk in &self.blocks {
            if let Some(i) = blk.member {
                let complete = blk.ret_frames >= timing.end && blk.ret_used > 0;
                if blk.ret_frames > 0 && !complete {
                    self.episodes[i].returning = true;
                }
            }
        }
        let mut any = false;
        for (b, s) in slow.iter_mut().enumerate() {
            let blk = &self.blocks[b];
            let Some(i) = blk.member else { continue };
            if blk.ret_frames < timing.end || blk.ret_used == 0 || self.episodes[i].returning {
                continue;
            }
            if !any {
                for ep in &mut self.episodes {
                    ep.leave_n = 0;
                    ep.leave_lo = usize::MAX;
                    ep.leave_hi = 0;
                }
                any = true;
            }
            let blk = &mut self.blocks[b];
            *s = (blk.ret_sum / blk.ret_used as f64) as f32;
            blk.long = *s;
            blk.member = None;
            blk.holdoff_until = fr.counter + timing.holdoff;
            let onset = blk.ret_onset;
            blk.clear_excursions(fr.ev.now.1);
            self.episodes[i].note_leave(b, onset);
        }
        if !any {
            return;
        }
        for i in 0..self.episodes.len() {
            if !self.episodes[i].active || self.episodes[i].leave_n == 0 {
                continue;
            }
            let onset = self.episodes[i].leave_onset;
            self.settle_leaves(i, onset, EndReason::Returned, fr, stats, on_event);
        }
    }

    /// After blocks left episode `i` (returned, or not restored after a comparable reset): `End`
    /// when no member remains; otherwise split disconnected parts off (each a `Rise` with
    /// `split_from`) and `Update` the rest to its real extent.
    fn settle_leaves(
        &mut self,
        i: usize,
        onset: Stamp,
        reason: EndReason,
        fr: &Frame,
        stats: &mut FloorStats,
        on_event: &mut impl FnMut(&FloorEvent),
    ) {
        let before = self.episodes[i].bins.clone();
        self.summarize(i, fr.layout, fr.spectrum);
        if self.episodes[i].members == 0 {
            self.episodes[i].active = false;
            stats.episode_ends += 1;
            let ep = &self.episodes[i];
            if ep.emitted {
                let ev = ep.event(
                    FloorEventKind::End,
                    onset,
                    &fr.ev,
                    before,
                    Some(reason),
                    None,
                    false,
                );
                on_event(&ev);
            }
            return;
        }
        if self.episodes[i].leave_n == 0 {
            return;
        }
        self.split(i, fr, stats, on_event);
        let ep = &self.episodes[i];
        if ep.emitted {
            let change = region_bins(fr.layout, ep.leave_lo, ep.leave_hi);
            let ev = ep.event(
                FloorEventKind::Update,
                onset,
                &fr.ev,
                change,
                None,
                None,
                false,
            );
            on_event(&ev);
            stats.update_events += 1;
        }
    }

    /// Splits episode `i` when its members are no longer contiguous: the largest run keeps the
    /// id, every other run becomes a new episode (same class, onset and rebaseline clock)
    /// announced by a `Rise` with `split_from`. One physical region, one open episode.
    fn split(
        &mut self,
        i: usize,
        fr: &Frame,
        stats: &mut FloorStats,
        on_event: &mut impl FnMut(&FloorEvent),
    ) {
        let nb = self.blocks.len();
        let (mut runs, mut best) = (0usize, (0usize, 0usize));
        let mut b = 0;
        while b < nb {
            if self.blocks[b].member != Some(i) {
                b += 1;
                continue;
            }
            let s = b;
            while b < nb && self.blocks[b].member == Some(i) {
                b += 1;
            }
            runs += 1;
            if b - s > best.1 - best.0 {
                best = (s, b);
            }
        }
        if runs < 2 {
            return;
        }
        let parent = self.episodes[i].id;
        let mut b = 0;
        while b < nb {
            if self.blocks[b].member != Some(i) || (best.0..best.1).contains(&b) {
                b += 1;
                continue;
            }
            let s = b;
            while b < nb && self.blocks[b].member == Some(i) {
                b += 1;
            }
            let j = self
                .episodes
                .iter()
                .position(|e| !e.active)
                .expect("one slot per block");
            let mut child = self.episodes[i].clone();
            child.id = self.next_id;
            self.next_id += 1;
            child.leave_n = 0;
            child.leave_lo = usize::MAX;
            child.leave_hi = 0;
            self.episodes[j] = child;
            for blk in &mut self.blocks[s..b] {
                blk.member = Some(j);
            }
            self.summarize(j, fr.layout, fr.spectrum);
            stats.splits += 1;
            let ep = &self.episodes[j];
            if ep.emitted {
                let mut ev = ep.event(
                    FloorEventKind::Rise,
                    ep.onset,
                    &fr.ev,
                    ep.bins.clone(),
                    None,
                    None,
                    false,
                );
                ev.split_from = Some(parent);
                on_event(&ev);
                stats.rise_events += 1;
            }
        }
        self.summarize(i, fr.layout, fr.spectrum);
    }

    /// Statistics of the non-member blocks of `range` with a run in direction `dir`.
    fn group(
        &mut self,
        range: Range<usize>,
        dir: i8,
        fr: &Frame,
        cfg: &FloorChangeConfig,
        timing: &Timing,
    ) -> Group {
        let mut k = 0usize;
        let mut onset: Option<Stamp> = None;
        let (mut var, mut n_var, mut used, mut sk_s, mut sk_n) = (0.0f64, 0u32, 0u64, 0.0f64, 0u64);
        for b in range {
            let blk = &self.blocks[b];
            if blk.member.is_some() || blk.run.dir != dir {
                continue;
            }
            let r = &blk.run;
            self.s1[k] = r.base;
            self.s2[k] = r.level();
            k += 1;
            if onset.is_none_or(|o| r.onset.0 < o.0) {
                onset = Some(r.onset);
            }
            if let Some(v) = r.var_d() {
                var += v / 2.0;
                n_var += 1;
            }
            used += r.level_n;
            sk_s += r.sum_sk;
            sk_n += r.sk_n;
        }
        let baseline = median_in_place(&mut self.s1[..k]);
        let level = median_in_place(&mut self.s2[..k]);
        let excess_std = if n_var > 0 {
            (var / f64::from(n_var)).sqrt()
        } else {
            0.0
        };
        let step_stat = excess_std / (used as f64 / k as f64).max(1.0).sqrt();
        let step_unc = step_stat
            .hypot(f64::from(fr.stat_block_db) * (timing.alpha / (2.0 - timing.alpha)).sqrt());
        let sk = (sk_n > 0).then(|| (sk_s / sk_n as f64) as f32);
        let baseline_db = db(baseline);
        Group {
            onset: onset.unwrap_or(fr.ev.now),
            baseline_db,
            level_db: db(level),
            excess_std_db: excess_std as f32,
            step_unc_db: step_unc as f32,
            sk,
            class: classify(sk, excess_std as f32, cfg),
            q_before: fr
                .quantisation_db
                .is_some_and(|q| f64::from(baseline_db) < q + fr.quantisation_margin_db),
        }
    }

    fn confirm_rises(
        &mut self,
        fr: &Frame,
        slow: &mut [f32],
        cfg: &FloorChangeConfig,
        timing: &Timing,
        stats: &mut FloorStats,
        on_event: &mut impl FnMut(&FloorEvent),
    ) {
        let thr = cfg.threshold_db as f32;
        let nb = self.blocks.len();
        let seed = |blk: &Block| {
            blk.member.is_none()
                && blk.run.dir > 0
                && blk.run.confirmable(timing, thr)
                && fr.counter >= blk.holdoff_until
        };
        if !self.blocks.iter().any(seed) {
            return;
        }
        let joinable = |blk: &Block| blk.member.is_some() || blk.run.dir > 0;
        let mut b = 0;
        while b < nb {
            if !joinable(&self.blocks[b]) {
                b += 1;
                continue;
            }
            let start = b;
            let mut seeded = false;
            while b < nb && joinable(&self.blocks[b]) {
                seeded |= seed(&self.blocks[b]);
                b += 1;
            }
            if seeded {
                self.rise_group(start..b, fr, slow, cfg, timing, stats, on_event);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn rise_group(
        &mut self,
        range: Range<usize>,
        fr: &Frame,
        slow: &mut [f32],
        cfg: &FloorChangeConfig,
        timing: &Timing,
        stats: &mut FloorStats,
        on_event: &mut impl FnMut(&FloorEvent),
    ) {
        let g = self.group(range.clone(), 1, fr, cfg, timing);
        let emittable = g.class != FloorChangeClass::Structured || cfg.emit_structured;
        // The surviving episode among those touched: a noise-like one before an unverified one
        // before a structured one (so the episode consumers accept keeps its identity), then an
        // emitted one, then the oldest.
        let rank = |e: &Episode| {
            let class = match e.class {
                FloorChangeClass::NoiseLike => 2,
                FloorChangeClass::Unverified => 1,
                FloorChangeClass::Structured => 0,
            };
            (class, e.emitted, std::cmp::Reverse(e.id))
        };
        let mut survivor: Option<usize> = None;
        for b in range.clone() {
            if let Some(i) = self.blocks[b].member {
                if survivor.is_none_or(|s| rank(&self.episodes[i]) > rank(&self.episodes[s])) {
                    survivor = Some(i);
                }
            }
        }
        for blk in &mut self.blocks {
            blk.mark = survivor.is_some() && blk.member == survivor;
        }
        let i = match survivor {
            Some(s) => {
                // Absorb every other episode touched by this group.
                for b in range.clone() {
                    let Some(o) = self.blocks[b].member else {
                        continue;
                    };
                    if o == s {
                        continue;
                    }
                    for blk in &mut self.blocks {
                        if blk.member == Some(o) {
                            blk.member = Some(s);
                        }
                    }
                    let other = &mut self.episodes[o];
                    other.active = false;
                    stats.merges += 1;
                    stats.episode_ends += 1;
                    let (o_onset, o_peak) = (other.onset, other.peak_step_db);
                    if other.emitted {
                        let id = self.episodes[s].id;
                        let other = &self.episodes[o];
                        let ev = other.event(
                            FloorEventKind::End,
                            fr.ev.now,
                            &fr.ev,
                            other.bins.clone(),
                            Some(EndReason::Merged),
                            Some(id),
                            false,
                        );
                        on_event(&ev);
                    }
                    let sv = &mut self.episodes[s];
                    if o_onset.0 < sv.onset.0 {
                        sv.onset = o_onset;
                    }
                    sv.peak_step_db = sv.peak_step_db.max(o_peak);
                }
                s
            }
            None => {
                let i = self
                    .episodes
                    .iter()
                    .position(|e| !e.active)
                    .expect("one slot per block");
                let ep = &mut self.episodes[i];
                *ep = Episode::inactive(g.onset.1, fr.gain);
                ep.active = true;
                ep.id = self.next_id;
                self.next_id += 1;
                ep.class = g.class;
                ep.onset = g.onset;
                ep.opened = fr.counter;
                ep.baseline_segment = fr.ev.segment;
                ep.sk = g.sk;
                ep.excess_std_db = g.excess_std_db;
                ep.step_unc_db = g.step_unc_db;
                ep.q_limited_before = g.q_before;
                ep.peak_step_db = g.level_db - g.baseline_db;
                if g.class == FloorChangeClass::Structured {
                    stats.structured_episodes += 1;
                }
                i
            }
        };
        let hold = g.class == FloorChangeClass::Structured;
        for b in range.clone() {
            let blk = &mut self.blocks[b];
            if blk.member.is_some() || blk.run.dir <= 0 {
                continue;
            }
            let r = blk.run;
            blk.member = Some(i);
            blk.onset = r.onset;
            blk.base = r.base;
            blk.g0 = self.drift;
            blk.recent = if r.level_n > 0 { r.recent } else { fr.floor[b] };
            blk.hold_slow = hold;
            blk.clear_excursions(fr.ev.now.1);
            if !hold {
                slow[b] = blk.recent;
            }
        }
        self.summarize(i, fr.layout, fr.spectrum);
        if survivor.is_some() && self.episodes[i].emitted {
            // One Extend per contiguous run of added blocks (new and absorbed), so a region
            // widening on both sides reports only what it added.
            let nb = self.blocks.len();
            let mut b = 0;
            while b < nb {
                let added = |blk: &Block| blk.member == Some(i) && !blk.mark;
                if !added(&self.blocks[b]) {
                    b += 1;
                    continue;
                }
                let s = b;
                let mut onset = self.blocks[b].onset;
                while b < nb && added(&self.blocks[b]) {
                    if self.blocks[b].onset.0 < onset.0 {
                        onset = self.blocks[b].onset;
                    }
                    b += 1;
                }
                let ep = &mut self.episodes[i];
                ep.peak_step_db = ep.peak_step_db.max(ep.step_db);
                let ev = ep.event(
                    FloorEventKind::Extend,
                    onset,
                    &fr.ev,
                    region_bins(fr.layout, s, b),
                    None,
                    None,
                    false,
                );
                on_event(&ev);
                stats.extend_events += 1;
            }
            return;
        }
        let ep = &mut self.episodes[i];
        let kind = if !ep.emitted && emittable {
            ep.emitted = true;
            if survivor.is_some() {
                ep.class = g.class;
            }
            FloorEventKind::Rise
        } else {
            return;
        };
        let onset = if kind == FloorEventKind::Rise {
            // The Rise's level is the confirmation run's.
            if survivor.is_none() {
                ep.level_db = g.level_db;
                ep.step_db = g.level_db - ep.baseline_db;
            }
            ep.onset
        } else {
            g.onset
        };
        ep.peak_step_db = ep.peak_step_db.max(ep.step_db);
        let ev = ep.event(kind, onset, &fr.ev, ep.bins.clone(), None, None, false);
        on_event(&ev);
        stats.rise_events += 1;
    }

    fn confirm_falls(
        &mut self,
        fr: &Frame,
        slow: &mut [f32],
        cfg: &FloorChangeConfig,
        timing: &Timing,
        stats: &mut FloorStats,
        on_event: &mut impl FnMut(&FloorEvent),
    ) {
        let thr = cfg.threshold_db as f32;
        let nb = self.blocks.len();
        let seed = |blk: &Block| {
            blk.member.is_none()
                && blk.run.dir < 0
                && blk.run.confirmable(timing, thr)
                && fr.counter >= blk.holdoff_until
        };
        let cap = self.cap;
        if !self.blocks.iter().any(seed) {
            return;
        }
        let pending = |blk: &Block| blk.member.is_none() && blk.run.dir < 0;
        let mut b = 0;
        while b < nb {
            if !pending(&self.blocks[b]) {
                b += 1;
                continue;
            }
            let mut start = b;
            let mut seeded = false;
            while b < nb && pending(&self.blocks[b]) {
                seeded |= seed(&self.blocks[b]);
                b += 1;
            }
            if !seeded {
                continue;
            }
            let (seg_start, seg_end) = (start, b);
            // Each block's level: the `hit_fraction/2` quantile of all its window frames (the
            // median of a clean fall; the middle of the low frames of an interrupted one, never
            // an average of the low frames alone).
            for j in seg_start..seg_end {
                let r = self.blocks[j].run;
                let n = r.frames.min(cap as u64) as usize;
                if n == 0 {
                    continue;
                }
                let scratch = &mut self.ring_scratch[..n];
                scratch.copy_from_slice(&self.ring[j * cap..j * cap + n]);
                let k = ((n - 1) as f64 * 0.5 * r.hit_fraction(cap)).round() as usize;
                let (_, &mut e, _) = scratch.select_nth_unstable_by(k, f32::total_cmp);
                self.blocks[j].run.fall_level = r.base * 10f32.powf(e / 10.0);
            }
            let mut end = b;
            // Edge blocks with few hit frames relative to the group (partly covered by the
            // change, flickering across the threshold) do not vote on `interrupted` or set the
            // level; they still fall with the group.
            let k = end - start;
            for (j, v) in self.s1[..k].iter_mut().enumerate() {
                *v = self.blocks[start + j].run.hit_fraction(cap) as f32;
            }
            let min_hit = cfg.edge_hit_fraction as f32 * median_in_place(&mut self.s1[..k]);
            while start + 1 < end && (self.blocks[start].run.hit_fraction(cap) as f32) < min_hit {
                start += 1;
            }
            while end > start + 1 && (self.blocks[end - 1].run.hit_fraction(cap) as f32) < min_hit {
                end -= 1;
            }
            let interrupted_blocks = self.blocks[start..end]
                .iter()
                .filter(|blk| blk.run.hit_fraction(cap) < cfg.fall_hit_fraction)
                .count();
            let interrupted = 2 * interrupted_blocks > end - start;
            // An interrupted fall narrower than one block width is a block straddling a sharp
            // floor edge whose estimate flips between the two sides, not a floor change: drop it
            // (no event, no re-seed) and hold the blocks off.
            let min_blocks = fr.layout.block_bins().div_ceil(fr.layout.hop_bins());
            if interrupted && end - start < min_blocks {
                for blk in &mut self.blocks[seg_start..seg_end] {
                    blk.run = Run::idle(fr.ev.now.1);
                    blk.holdoff_until = fr.counter + timing.holdoff;
                }
                continue;
            }
            let g = self.group(start..end, -1, fr, cfg, timing);
            let (start, end) = (seg_start, seg_end);
            let mut ep = Episode::inactive(g.onset.1, fr.gain);
            ep.id = self.next_id;
            self.next_id += 1;
            ep.class = g.class;
            ep.onset = g.onset;
            ep.bins = region_bins(fr.layout, start, end);
            (ep.f_lo_hz, ep.f_hi_hz) = edges(fr.spectrum, &ep.bins);
            ep.band_fraction = ep.bins.len() as f32 / fr.layout.bins() as f32;
            ep.baseline_db = g.baseline_db;
            ep.level_db = g.level_db;
            ep.step_db = g.level_db - g.baseline_db;
            ep.peak_step_db = ep.step_db;
            ep.step_unc_db = g.step_unc_db;
            ep.sk = g.sk;
            ep.excess_std_db = g.excess_std_db;
            ep.q_limited_before = g.q_before;
            ep.baseline_segment = fr.ev.segment;
            for (blk, s) in self.blocks[start..end]
                .iter_mut()
                .zip(&mut slow[start..end])
            {
                *s = blk.run.level();
                blk.long = *s;
                blk.holdoff_until = fr.counter + timing.holdoff;
                blk.clear_excursions(fr.ev.now.1);
            }
            let ev = ep.event(
                FloorEventKind::Fall,
                g.onset,
                &fr.ev,
                ep.bins.clone(),
                None,
                None,
                interrupted,
            );
            on_event(&ev);
            stats.level_falls += 1;
            stats.floor_corrections += u64::from(interrupted);
        }
    }

    fn rebaseline(
        &mut self,
        fr: &Frame,
        slow: &mut [f32],
        timing: &Timing,
        stats: &mut FloorStats,
        on_event: &mut impl FnMut(&FloorEvent),
    ) {
        let Some(limit) = timing.rebaseline else {
            return;
        };
        for i in 0..self.episodes.len() {
            let ep = &self.episodes[i];
            if !ep.active || fr.counter.saturating_sub(ep.opened) < limit {
                continue;
            }
            self.summarize(i, fr.layout, fr.spectrum);
            for (b, blk) in self.blocks.iter_mut().enumerate() {
                if blk.member != Some(i) {
                    continue;
                }
                if blk.hold_slow {
                    slow[b] = blk.recent;
                }
                blk.long = slow[b];
                blk.member = None;
                blk.clear_excursions(fr.ev.now.1);
            }
            let ep = &mut self.episodes[i];
            ep.active = false;
            stats.rebaselines += 1;
            stats.episode_ends += 1;
            if ep.emitted {
                let ev = ep.event(
                    FloorEventKind::End,
                    fr.ev.now,
                    &fr.ev,
                    ep.bins.clone(),
                    Some(EndReason::Rebaselined),
                    None,
                    false,
                );
                on_event(&ev);
            }
        }
    }

    fn track_peaks(&mut self, fr: &Frame) {
        for i in 0..self.episodes.len() {
            if !self.episodes[i].active {
                continue;
            }
            let mut n = 0;
            for (b, blk) in self.blocks.iter().enumerate() {
                if blk.member == Some(i) {
                    self.s1[n] = db_ratio(fr.floor[b], blk.base);
                    n += 1;
                }
            }
            if n > 0 {
                let m = median_in_place(&mut self.s1[..n]);
                let ep = &mut self.episodes[i];
                ep.peak_step_db = ep.peak_step_db.max(m);
            }
        }
    }
}
