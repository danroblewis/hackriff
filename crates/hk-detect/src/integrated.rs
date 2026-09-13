//! The integrated (emitter-level) spectrum: a sliding window of `blocks × block_s` (default
//! 4 × 0.25 s) of mean PSD and mean floor, evaluated at every block boundary with S4's
//! `detect_integrated` (seed +6 dB, extend +3 dB over the floor: with `n_eff` ≈ 10⁴ the
//! statistical threshold is ≈ 0.2 dB, so the model-uncertainty margin dominates).
//!
//! Its outputs are the integrated emitters with their spur/DC/edge/comb flags, the comb (rule 4
//! needs a set of persistent narrow lines, so it is judged here), and emitter-candidate
//! confirmation once the window spans ≥ `confirm_s`. Impulsive frames are not integrated.
//! Buffers are sized per resolution; evaluation allocates nothing.

use std::ops::Range;

use hk_model::{DetectionFlags, Timestamp};

use crate::alpha::db;
use crate::comb::{Comb, CombFinder};
use crate::config::{IntegrationConfig, Rules};
use crate::rules::{Geometry, SpurDecision, edge_hit, spur_decision};

/// One emitter in the integrated spectrum.
#[derive(Clone, Debug, PartialEq)]
pub struct IntegratedEmitter {
    /// Bins `[lo, hi)`.
    pub bins: Range<usize>,
    /// Lower edge, Hz.
    pub f_lo_hz: f64,
    /// Upper edge, Hz.
    pub f_hi_hz: f64,
    /// Excess-weighted centroid, Hz.
    pub f_center_hz: f64,
    /// Peak bin, Hz.
    pub f_peak_hz: f64,
    /// Extent width, Hz.
    pub bandwidth_hz: f64,
    /// Peak `P/floor`, dB.
    pub peak_snr_db: f64,
    /// Peak-bin excess power, dBFS (per bin).
    pub peak_excess_dbfs: f64,
    /// Integrated excess power, dBFS.
    pub level_dbfs: f64,
    /// Spur/DC/comb/edge/marginal flags.
    pub flags: DetectionFlags,
}

/// The latest evaluation.
#[derive(Clone, Debug, PartialEq)]
pub struct IntegratedEvaluation {
    /// Detector segment.
    pub segment: u64,
    /// Seconds integrated.
    pub span_s: f64,
    /// Frames integrated.
    pub frames: u64,
    /// Spans at least `confirm_s`: its emitters confirm candidates.
    pub confirming: bool,
    /// Includes a partial block (segment end).
    pub partial: bool,
    /// Start of the first integrated frame.
    pub t_start: Timestamp,
    /// End of the last integrated frame.
    pub t_end: Timestamp,
    /// Emitters.
    pub emitters: Vec<IntegratedEmitter>,
    /// Narrow non-artefact lines tested for a comb.
    pub comb: Comb,
}

impl Default for IntegratedEvaluation {
    fn default() -> Self {
        Self {
            segment: 0,
            span_s: 0.0,
            frames: 0,
            confirming: false,
            partial: false,
            t_start: Timestamp::UNIX_EPOCH,
            t_end: Timestamp::UNIX_EPOCH,
            emitters: Vec::new(),
            comb: Comb::default(),
        }
    }
}

/// Mean spectra of an evaluation, for the cross-capture trust tests ([`crate::trust`]).
#[derive(Clone, Debug, PartialEq)]
pub struct IntegratedSnapshot {
    /// Geometry.
    pub geometry: Geometry,
    /// Seconds integrated.
    pub span_s: f64,
    /// Mean PSD per bin, FS²/Hz.
    pub mean_psd: Vec<f64>,
    /// Mean floor per bin, FS²/Hz.
    pub mean_floor: Vec<f64>,
    /// Mean PSD of each block (oldest first), for block-to-block stability.
    pub block_psd: Vec<Vec<f64>>,
}

/// Integrated SNR, level and block spread over a frequency span (S4 `measure`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpanMeasure {
    /// Peak-bin SNR `10·log10(max(P/F − 1, 1e-3))`, dB.
    pub snr_db: f64,
    /// Integrated excess, dBFS.
    pub level_dbfs: f64,
    /// Max − min over blocks of the peak (±1 bin) SNR, dB.
    pub spread_db: f64,
}

impl IntegratedSnapshot {
    /// S4 `measure` over `[f_lo, f_hi]`.
    pub fn measure(&self, f_lo_hz: f64, f_hi_hz: f64) -> Option<SpanMeasure> {
        let g = &self.geometry;
        let n = g.bins;
        let b0 = g.bin_at_or_above(f_lo_hz).min(n.saturating_sub(1));
        let b1 = g.bin_at_or_above(f_hi_hz).max(b0 + 1).min(n);
        if b0 >= b1 {
            return None;
        }
        let ratio = |b: usize| self.mean_psd[b] / self.mean_floor[b].max(1e-300);
        let pk = (b0..b1).max_by(|&a, &b| ratio(a).total_cmp(&ratio(b)))?;
        let snr_db = db((ratio(pk) - 1.0).max(1e-3));
        let excess: f64 = (b0..b1)
            .map(|b| (self.mean_psd[b] - self.mean_floor[b]).max(0.0))
            .sum();
        let level_dbfs = db((excess * g.bin_width_hz).max(1e-30));
        let (lo, hi) = (pk.saturating_sub(1).max(b0), (pk + 2).min(b1));
        let fl: f64 = self.mean_floor[lo..hi].iter().sum::<f64>() / (hi - lo) as f64;
        let (mut smin, mut smax) = (f64::INFINITY, f64::NEG_INFINITY);
        for block in &self.block_psd {
            let rb = block[lo..hi].iter().sum::<f64>() / (hi - lo) as f64 / fl.max(1e-300);
            let sb = db((rb - 1.0).max(1e-3));
            smin = smin.min(sb);
            smax = smax.max(sb);
        }
        let spread_db = if smax >= smin { smax - smin } else { 0.0 };
        Some(SpanMeasure {
            snr_db,
            level_dbfs,
            spread_db,
        })
    }

    /// Median over non-edge bins of `10·log10(other.floor / self.floor)` (same geometry).
    pub fn floor_change_db(&self, other: &IntegratedSnapshot) -> Option<f64> {
        if self.geometry.bins != other.geometry.bins {
            return None;
        }
        let g = &self.geometry;
        let mut v: Vec<f64> = (0..g.bins)
            .filter(|&b| g.offset_hz(b as f64).abs() <= g.usable_half_hz)
            .map(|b| db(other.mean_floor[b].max(1e-300) / self.mean_floor[b].max(1e-300)))
            .collect();
        if v.is_empty() {
            return None;
        }
        let mid = v.len() / 2;
        let (_, &mut m, _) = v.select_nth_unstable_by(mid, f64::total_cmp);
        Some(m)
    }
}

/// The sliding integrated spectrum.
#[derive(Clone, Debug)]
pub struct IntegratedSpectrum {
    cfg: IntegrationConfig,
    bins: usize,
    slots: usize,
    block_frames: u32,
    frame_period_s: f64,
    psd: Vec<f64>,
    floor: Vec<f64>,
    frames: Vec<u32>,
    t_start: Vec<Timestamp>,
    t_end: Vec<Timestamp>,
    head: usize,
    completed: usize,
    mean_psd: Vec<f64>,
    mean_floor: Vec<f64>,
    lines: Vec<f64>,
    eval: IntegratedEvaluation,
    evaluated: bool,
    dirty: bool,
}

impl IntegratedSpectrum {
    /// An empty integrator.
    pub fn new(cfg: IntegrationConfig, comb_lines: usize) -> Self {
        let slots = cfg.blocks + 1;
        Self {
            cfg,
            bins: 0,
            slots,
            block_frames: 1,
            frame_period_s: 0.0,
            psd: Vec::new(),
            floor: Vec::new(),
            frames: vec![0; slots],
            t_start: vec![Timestamp::UNIX_EPOCH; slots],
            t_end: vec![Timestamp::UNIX_EPOCH; slots],
            head: 0,
            completed: 0,
            mean_psd: Vec::new(),
            mean_floor: Vec::new(),
            lines: Vec::with_capacity(comb_lines.max(8) * 4),
            eval: IntegratedEvaluation {
                comb: Comb {
                    member_hz: Vec::with_capacity(comb_lines),
                    ..Comb::default()
                },
                ..IntegratedEvaluation::default()
            },
            evaluated: false,
            dirty: false,
        }
    }

    /// Starts a segment of `bins` bins with frames every `frame_period_s`.
    pub fn configure(&mut self, bins: usize, frame_period_s: f64, segment: u64) {
        if bins != self.bins {
            self.bins = bins;
            self.psd.resize(bins * self.slots, 0.0);
            self.floor.resize(bins * self.slots, 0.0);
            self.mean_psd.resize(bins, 0.0);
            self.mean_floor.resize(bins, 0.0);
            self.eval.emitters.reserve(bins / 2 + 1);
        }
        self.psd.fill(0.0);
        self.floor.fill(0.0);
        self.frames.fill(0);
        self.frame_period_s = frame_period_s;
        self.block_frames = ((self.cfg.block_s / frame_period_s).round() as u32).max(1);
        self.head = 0;
        self.completed = 0;
        self.evaluated = false;
        self.dirty = false;
        self.eval.segment = segment;
        self.eval.emitters.clear();
        self.eval.comb.flagged = false;
        self.eval.comb.member_hz.clear();
        self.eval.comb.members = 0;
        self.eval.confirming = false;
        self.eval.span_s = 0.0;
        self.eval.frames = 0;
    }

    /// Frames per block.
    pub fn block_frames(&self) -> u32 {
        self.block_frames
    }

    /// Adds one frame; returns `true` when it completed a block.
    pub fn push(
        &mut self,
        psd: &[f32],
        floor: &[f32],
        t_start: Timestamp,
        t_end: Timestamp,
    ) -> bool {
        let n = self.bins;
        debug_assert_eq!(psd.len(), n);
        let h = self.head;
        let base = h * n;
        if self.frames[h] == 0 {
            self.t_start[h] = t_start;
        }
        for (a, &p) in self.psd[base..base + n].iter_mut().zip(psd) {
            if p.is_finite() {
                *a += f64::from(p);
            }
        }
        for (a, &f) in self.floor[base..base + n].iter_mut().zip(floor) {
            if f.is_finite() {
                *a += f64::from(f);
            }
        }
        self.frames[h] += 1;
        self.t_end[h] = t_end;
        self.dirty = true;
        if self.frames[h] >= self.block_frames {
            self.head = (h + 1) % self.slots;
            let nb = self.head * n;
            self.psd[nb..nb + n].fill(0.0);
            self.floor[nb..nb + n].fill(0.0);
            self.frames[self.head] = 0;
            self.completed = (self.completed + 1).min(self.cfg.blocks);
            true
        } else {
            false
        }
    }

    /// Frames not yet part of any evaluation (a partial block or a completed but unevaluated one).
    pub fn has_unevaluated(&self) -> bool {
        self.dirty
    }

    /// Slots (oldest first) included in an evaluation.
    fn slots_in_eval(&self, include_partial: bool) -> impl Iterator<Item = usize> + '_ {
        let s = self.slots;
        let partial = include_partial && self.frames[self.head] > 0;
        let completed = self.completed;
        let head = self.head;
        (0..completed)
            .rev()
            .map(move |i| (head + s - 1 - i) % s)
            .chain(partial.then_some(head))
    }

    /// Evaluates the window (completed blocks, plus the partial block when `include_partial`).
    /// Returns `false` when nothing is integrated.
    pub fn evaluate(
        &mut self,
        include_partial: bool,
        rules: &Rules,
        geometry: &Geometry,
        comb: &mut CombFinder,
    ) -> bool {
        let n = self.bins;
        let mut frames = 0u64;
        self.mean_psd.fill(0.0);
        self.mean_floor.fill(0.0);
        let mut first = true;
        let (mut t0, mut t1) = (Timestamp::UNIX_EPOCH, Timestamp::UNIX_EPOCH);
        let slots: [usize; 16] = {
            let mut a = [usize::MAX; 16];
            for (i, s) in self.slots_in_eval(include_partial).take(16).enumerate() {
                a[i] = s;
            }
            a
        };
        for &s in slots.iter().take_while(|&&s| s != usize::MAX) {
            if self.frames[s] == 0 {
                continue;
            }
            frames += u64::from(self.frames[s]);
            if first {
                t0 = self.t_start[s];
                first = false;
            }
            t1 = self.t_end[s];
            let base = s * n;
            for b in 0..n {
                self.mean_psd[b] += self.psd[base + b];
                self.mean_floor[b] += self.floor[base + b];
            }
        }
        if frames == 0 {
            return false;
        }
        let inv = 1.0 / frames as f64;
        for b in 0..n {
            self.mean_psd[b] *= inv;
            self.mean_floor[b] *= inv;
        }
        self.dirty = !include_partial && self.frames[self.head] > 0;
        self.evaluated = true;
        let e = &mut self.eval;
        e.frames = frames;
        e.span_s = frames as f64 * self.frame_period_s;
        e.confirming = e.span_s + 0.5 * self.frame_period_s >= self.cfg.confirm_s;
        e.partial = include_partial;
        e.t_start = t0;
        e.t_end = t1;
        e.emitters.clear();
        let seed = 10f64.powf(self.cfg.seed_db / 10.0);
        let extend = 10f64.powf(self.cfg.extend_db / 10.0);
        let df = geometry.bin_width_hz;
        let mut b = 0;
        while b < n {
            let ratio = |i: usize| self.mean_psd[i] / self.mean_floor[i].max(1e-300);
            let r = ratio(b);
            if r.is_nan() || r <= extend {
                b += 1;
                continue;
            }
            let lo = b;
            let mut has_seed = false;
            while b < n && ratio(b) > extend {
                has_seed |= ratio(b) > seed;
                b += 1;
            }
            if !has_seed {
                continue;
            }
            let hi = b;
            let (mut ex_sum, mut ex_f, mut pk, mut pk_ratio) = (0.0, 0.0, lo, 0.0);
            for i in lo..hi {
                let ex = (self.mean_psd[i] - self.mean_floor[i]).max(0.0);
                ex_sum += ex;
                ex_f += ex * geometry.bin_hz(i as f64);
                if ratio(i) > pk_ratio {
                    pk_ratio = ratio(i);
                    pk = i;
                }
            }
            let (f_lo, f_hi) = geometry.extent_hz(lo, hi);
            let f_center = if ex_sum > 0.0 {
                ex_f / ex_sum
            } else {
                0.5 * (f_lo + f_hi)
            };
            let bw = (hi - lo) as f64 * df;
            let peak_snr_db = db(pk_ratio);
            let mut flags = DetectionFlags {
                edge: edge_hit(f_lo, f_hi, geometry),
                marginal: peak_snr_db < rules.marginal_snr_db,
                ..DetectionFlags::default()
            };
            flags.marginal |= flags.edge;
            if let SpurDecision {
                reason: Some(reason),
                ..
            } = spur_decision(f_lo, f_hi, f_center, bw, geometry, rules, None, 0.0)
            {
                flags.spur_candidate = true;
                flags.spur_reason = Some(reason);
            }
            e.emitters.push(IntegratedEmitter {
                bins: lo..hi,
                f_lo_hz: f_lo,
                f_hi_hz: f_hi,
                f_center_hz: f_center,
                f_peak_hz: geometry.bin_hz(pk as f64),
                bandwidth_hz: bw,
                peak_snr_db,
                peak_excess_dbfs: db(((self.mean_psd[pk] - self.mean_floor[pk]) * df).max(1e-30)),
                level_dbfs: db((ex_sum * df).max(1e-30)),
                flags,
            });
        }
        // Rule 4 on the narrow, non-edge, non-artefact lines (the strongest `max_lines`).
        self.lines.clear();
        for em in &e.emitters {
            if em.bandwidth_hz <= rules.comb.max_line_width_hz
                && !em.flags.edge
                && !em.flags.spur_candidate
            {
                self.lines.push(em.f_center_hz);
            }
        }
        if self.lines.len() > rules.comb.max_lines {
            // Keep the strongest lines: order by level, then take the first `max_lines`.
            let emitters = &e.emitters;
            let level_of = |f: f64| {
                emitters
                    .iter()
                    .find(|m| m.f_center_hz == f)
                    .map_or(f64::NEG_INFINITY, |m| m.level_dbfs)
            };
            self.lines
                .sort_unstable_by(|&a, &b| level_of(b).total_cmp(&level_of(a)));
            self.lines.truncate(rules.comb.max_lines);
        }
        let lo = geometry.center_hz - geometry.usable_half_hz;
        let hi = geometry.center_hz + geometry.usable_half_hz;
        comb.evaluate(&self.lines, lo, hi, &mut e.comb);
        if e.comb.flagged {
            let tol = rules.comb.tolerance_hz + df / 2.0;
            for em in &mut e.emitters {
                if em.bandwidth_hz <= rules.comb.max_line_width_hz
                    && !em.flags.spur_candidate
                    && CombFinder::is_member(&e.comb, em.f_center_hz, tol)
                {
                    em.flags.spur_candidate = true;
                    em.flags.spur_reason = Some(hk_model::detection::SpurReason::Comb);
                }
            }
        }
        true
    }

    /// The latest evaluation, if any in this segment.
    pub fn evaluation(&self) -> Option<&IntegratedEvaluation> {
        self.evaluated.then_some(&self.eval)
    }

    /// Mean spectra of the latest evaluation (allocates; off the real-time path).
    pub fn snapshot(&self, geometry: &Geometry) -> Option<IntegratedSnapshot> {
        if !self.evaluated {
            return None;
        }
        let n = self.bins;
        let block_psd = self
            .slots_in_eval(self.eval.partial)
            .filter(|&s| self.frames[s] > 0)
            .map(|s| {
                let inv = 1.0 / f64::from(self.frames[s]);
                self.psd[s * n..s * n + n]
                    .iter()
                    .map(|&v| v * inv)
                    .collect()
            })
            .collect();
        Some(IntegratedSnapshot {
            geometry: *geometry,
            span_s: self.eval.span_s,
            mean_psd: self.mean_psd.clone(),
            mean_floor: self.mean_floor.clone(),
            block_psd,
        })
    }
}
