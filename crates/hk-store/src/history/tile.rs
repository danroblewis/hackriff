//! In-memory tile accumulators: level-0 frame folding and level-to-level rollup.

use hk_model::{CalibrationStateId, TileKey, Timestamp};

use super::config::{HistogramConfig, LevelGeometry};
use super::frame::{FrameInput, GainState};
use super::stats::{exact_percentile, hist_percentile, undb};

/// Most distinct gain states listed per tile; further states are counted in
/// [`ProvenanceSummary::other_gain_frames`].
pub const MAX_GAIN_STATES: usize = 8;

/// Provenance and gain-state digest of the frames behind a tile or query result (C26: "store
/// provenance per tile so C30 can rule front-end changes out first").
///
/// `frames` counts frame contributions per level-0 tile (a frame spanning two tiles counts once in
/// each); ratios such as [`ProvenanceSummary::suspect_fraction`] are unaffected.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProvenanceSummary {
    /// Frame contributions.
    pub frames: u64,
    /// Contributions flagged overloaded/clipped.
    pub suspect_frames: u64,
    /// Samples reported dropped before contributing frames.
    pub dropped_samples: u64,
    /// Frame-to-frame gain changes seen while folding (summed over tiles).
    pub gain_changes: u64,
    /// Distinct gain states and their frame counts (at most [`MAX_GAIN_STATES`]).
    pub gain_states: Vec<(GainState, u64)>,
    /// Frames under gain states beyond the listed ones.
    pub other_gain_frames: u64,
    /// Frames without a gain state.
    pub unknown_gain_frames: u64,
    /// Calibration of the first frame.
    pub calibration: Option<CalibrationStateId>,
    /// More than one calibration contributed.
    pub calibration_mixed: bool,
    /// Earliest frame start.
    pub first_frame: Option<Timestamp>,
    /// Latest frame start.
    pub last_frame: Option<Timestamp>,
}

impl ProvenanceSummary {
    /// Fraction of contributions flagged suspect (the hk-model `SpectrumTile::suspect_fraction`).
    pub fn suspect_fraction(&self) -> f64 {
        if self.frames == 0 {
            0.0
        } else {
            self.suspect_frames as f64 / self.frames as f64
        }
    }

    pub(crate) fn clear(&mut self) {
        let mut states = std::mem::take(&mut self.gain_states);
        states.clear();
        *self = Self {
            gain_states: states,
            ..Self::default()
        };
    }

    fn add_gain(&mut self, g: GainState, frames: u64) {
        if let Some(s) = self.gain_states.iter_mut().find(|(s, _)| *s == g) {
            s.1 += frames;
        } else if self.gain_states.len() < MAX_GAIN_STATES {
            self.gain_states.push((g, frames));
        } else {
            self.other_gain_frames += frames;
        }
    }

    fn set_calibration(&mut self, cal: Option<CalibrationStateId>, mixed: bool, had_frames: bool) {
        if !had_frames {
            self.calibration = cal;
            self.calibration_mixed = mixed;
        } else if mixed || self.calibration != cal {
            self.calibration_mixed = true;
        }
    }

    pub(crate) fn add_frame(&mut self, f: &FrameInput<'_>, last_gain: &mut Option<GainState>) {
        self.set_calibration(f.calibration, false, self.frames > 0);
        self.frames += 1;
        self.suspect_frames += u64::from(f.suspect);
        self.dropped_samples += f.dropped_samples;
        match f.gain {
            Some(g) => {
                if last_gain.is_some_and(|l| l != g) {
                    self.gain_changes += 1;
                }
                *last_gain = Some(g);
                self.add_gain(g, 1);
            }
            None => self.unknown_gain_frames += 1,
        }
        self.first_frame = Some(self.first_frame.map_or(f.t, |t| t.min(f.t)));
        self.last_frame = Some(self.last_frame.map_or(f.t, |t| t.max(f.t)));
    }

    /// Folds another summary into this one.
    pub fn merge(&mut self, o: &ProvenanceSummary) {
        if o.frames == 0 {
            return;
        }
        self.set_calibration(o.calibration, o.calibration_mixed, self.frames > 0);
        self.frames += o.frames;
        self.suspect_frames += o.suspect_frames;
        self.dropped_samples += o.dropped_samples;
        self.gain_changes += o.gain_changes;
        for &(g, n) in &o.gain_states {
            self.add_gain(g, n);
        }
        self.other_gain_frames += o.other_gain_frames;
        self.unknown_gain_frames += o.unknown_gain_frames;
        for t in [o.first_frame, o.last_frame].into_iter().flatten() {
            self.first_frame = Some(self.first_frame.map_or(t, |x| x.min(t)));
            self.last_frame = Some(self.last_frame.map_or(t, |x| x.max(t)));
        }
    }
}

/// An open column's would-be stats: `(t, per-f (p_lo, p_hi, occupied seconds))`.
pub(crate) type ColumnPreview = (usize, Vec<(f32, f32, f64)>);

/// One frame value waiting for its level-0 time column to close.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ColEntry {
    pub f: u32,
    pub v: f32,
    /// Caller threshold, dB; NaN = use the default floor at close.
    pub thr: f32,
    pub dur_s: f32,
}

/// A tile accumulator. Per-cell arrays are row-major `t · nf + f`; `hist` is `f · bins + bin` and
/// holds the value histogram of each frequency cell over the **whole tile**, which is exactly the
/// histogram of the parent cell this tile rolls up into.
#[derive(Clone, Debug)]
pub(crate) struct Tile {
    pub key: TileKey,
    pub nf: usize,
    pub nt: usize,
    pub bins: usize,
    /// Global cell index of frequency cell 0 / time cell 0.
    pub f_cell0: i64,
    pub t_cell0: i64,
    pub t_cell_s: f64,
    pub count: Vec<u32>,
    /// dB; −∞ when unobserved.
    pub max: Vec<f32>,
    pub sum_lin: Vec<f64>,
    /// Observed seconds (unclipped sum of frame durations).
    pub obs_s: Vec<f64>,
    /// Occupied seconds.
    pub occ_s: Vec<f64>,
    pub occ_max: Vec<f32>,
    pub p_lo: Vec<f32>,
    pub p_hi: Vec<f32>,
    pub hist: Vec<u32>,
    pub prov: ProvenanceSummary,
    pub last_gain: Option<GainState>,
    /// Level 0: the time column currently buffering values.
    pub col_t: Option<usize>,
    /// Level 0: the highest column already closed (later values for it take the late path).
    pub col_done: Option<usize>,
    pub col: Vec<ColEntry>,
}

impl Tile {
    pub fn new(key: TileKey, nf: usize, g: &LevelGeometry, bins: usize) -> Self {
        let n = nf * g.nt;
        let mut t = Self {
            key,
            nf,
            nt: g.nt,
            bins,
            f_cell0: 0,
            t_cell0: 0,
            t_cell_s: 0.0,
            count: vec![0; n],
            max: vec![0.0; n],
            sum_lin: vec![0.0; n],
            obs_s: vec![0.0; n],
            occ_s: vec![0.0; n],
            occ_max: vec![0.0; n],
            p_lo: vec![0.0; n],
            p_hi: vec![0.0; n],
            hist: vec![0; nf * bins],
            prov: ProvenanceSummary {
                gain_states: Vec::with_capacity(MAX_GAIN_STATES),
                ..Default::default()
            },
            last_gain: None,
            col_t: None,
            col_done: None,
            col: Vec::new(),
        };
        t.reset(key, g);
        t
    }

    /// Clears the accumulator for reuse under `key` (same dimensions). Allocation-free.
    pub fn reset(&mut self, key: TileKey, g: &LevelGeometry) {
        debug_assert_eq!(self.nt, g.nt);
        self.key = key;
        self.f_cell0 = key.f_block * self.nf as i64;
        self.t_cell0 = key.t_block * self.nt as i64;
        self.t_cell_s = g.t_cell_ns as f64 * 1e-9;
        self.count.fill(0);
        self.max.fill(f32::NEG_INFINITY);
        self.sum_lin.fill(0.0);
        self.obs_s.fill(0.0);
        self.occ_s.fill(0.0);
        self.occ_max.fill(0.0);
        self.p_lo.fill(f32::NAN);
        self.p_hi.fill(f32::NAN);
        self.hist.fill(0);
        self.prov.clear();
        self.last_gain = None;
        self.col_t = None;
        self.col_done = None;
        self.col.clear();
    }

    #[inline]
    pub fn hist_row(&self, f: usize) -> &[u32] {
        &self.hist[f * self.bins..(f + 1) * self.bins]
    }

    /// Observed and occupied seconds of cell `i`, with observation clipped to the cell duration
    /// (overlapping frames) and occupancy scaled with it.
    #[inline]
    pub fn cell_obs(&self, i: usize) -> (f64, f64) {
        let raw = self.obs_s[i];
        if raw <= 0.0 {
            return (0.0, 0.0);
        }
        let o = raw.min(self.t_cell_s);
        (o, self.occ_s[i].min(raw) * (o / raw))
    }

    /// Folds one resampled frame value into level-0 cell `(t, f)`.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn add_value(
        &mut self,
        t: usize,
        f: usize,
        v_db: f32,
        pk_db: f32,
        v_lin: f64,
        dur_s: f64,
        hist: &HistogramConfig,
    ) {
        let i = t * self.nf + f;
        self.count[i] += 1;
        self.sum_lin[i] += v_lin;
        self.obs_s[i] += dur_s;
        if pk_db > self.max[i] {
            self.max[i] = pk_db;
        }
        self.hist[f * self.bins + hist.bin(v_db)] += 1;
    }

    /// Occupancy for a value whose column has already closed (percentiles are not revisited).
    pub fn add_late_occupancy(&mut self, t: usize, f: usize, v_db: f32, thr: f32, dur_s: f64) {
        let i = t * self.nf + f;
        if v_db > thr {
            self.occ_s[i] += dur_s;
        }
        if self.obs_s[i] > 0.0 {
            self.occ_max[i] = (self.occ_s[i] / self.obs_s[i]).min(1.0) as f32;
        }
    }

    /// Closes the buffered level-0 column: exact percentiles and occupancy decisions.
    pub fn close_column(
        &mut self,
        floor: Option<&[f32]>,
        margin: f32,
        pct: (f32, f32),
        scratch: &mut Vec<f32>,
    ) {
        let Some(t) = self.col_t.take() else {
            return;
        };
        self.col_done = Some(self.col_done.map_or(t, |d| d.max(t)));
        let mut col = std::mem::take(&mut self.col);
        col.sort_unstable_by_key(|e| e.f);
        let nf = self.nf;
        column_stats(&col, floor, margin, pct, scratch, |f, plo, phi, occ| {
            let i = t * nf + f;
            self.p_lo[i] = plo;
            self.p_hi[i] = phi;
            self.occ_s[i] += occ;
            if self.obs_s[i] > 0.0 {
                self.occ_max[i] = (self.occ_s[i] / self.obs_s[i]).min(1.0) as f32;
            }
        });
        col.clear();
        self.col = col;
    }

    /// The stats the open column would get if closed now: `(t, per-f (p_lo, p_hi, occ_s))`.
    pub fn column_preview(
        &self,
        floor: Option<&[f32]>,
        margin: f32,
        pct: (f32, f32),
    ) -> Option<ColumnPreview> {
        let t = self.col_t?;
        let mut col = self.col.clone();
        col.sort_unstable_by_key(|e| e.f);
        let mut out = vec![(f32::NAN, f32::NAN, 0.0); self.nf];
        let mut scratch = Vec::new();
        column_stats(&col, floor, margin, pct, &mut scratch, |f, a, b, o| {
            out[f] = (a, b, o);
        });
        Some((t, out))
    }

    /// Rolls a sealed child tile (one level finer) into this tile. The child becomes time column
    /// `child.t_block − t_cell0`; each parent frequency cell aggregates `f_factor` child cells.
    ///
    /// Rules: max-of-max; power mean (Σ linear mean × frames); histogram sum → percentiles;
    /// occupancy = time-weighted mean over the child's time cells, then the **maximum** over the
    /// child frequency cells (a lower bound on "any child occupied", exact for one emitter per
    /// cell); max-occupancy = max over all child cells, so a short busy period survives; coverage =
    /// the best-observed child frequency cell. The parent cell is overwritten, so a fold is
    /// idempotent.
    pub fn fold_child(
        &mut self,
        child: &Tile,
        f_factor: u32,
        hist_cfg: &HistogramConfig,
        pct: (f32, f32),
        group: &mut Vec<u32>,
    ) {
        let tp = child.key.t_block - self.t_cell0;
        if tp < 0 || tp >= self.nt as i64 {
            debug_assert!(false, "child outside parent tile");
            return;
        }
        let tp = tp as usize;
        group.clear();
        group.resize(self.bins, 0);
        let factor = i64::from(f_factor);
        let mut fc = 0usize;
        while fc < child.nf {
            let gc = child.f_cell0 + fc as i64;
            let fp = gc.div_euclid(factor) - self.f_cell0;
            let group_end = child.nf.min(fc + (factor - gc.rem_euclid(factor)) as usize);
            let (mut count, mut max, mut sum_lin, mut occ_max) =
                (0u64, f32::NEG_INFINITY, 0.0, 0f32);
            let (mut best_obs, mut best_ratio) = (0.0f64, 0.0f64);
            let mut any_hist = false;
            for f in fc..group_end {
                let (mut obs_f, mut occ_f) = (0.0, 0.0);
                for t in 0..child.nt {
                    let i = t * child.nf + f;
                    let n = child.count[i];
                    if n == 0 {
                        continue;
                    }
                    count += u64::from(n);
                    max = max.max(child.max[i]);
                    sum_lin += child.sum_lin[i];
                    occ_max = occ_max.max(child.occ_max[i]);
                    let (o, c) = child.cell_obs(i);
                    obs_f += o;
                    occ_f += c;
                }
                if obs_f > 0.0 {
                    best_obs = best_obs.max(obs_f);
                    best_ratio = best_ratio.max(occ_f / obs_f);
                }
                let row = child.hist_row(f);
                if row.iter().any(|&c| c > 0) {
                    any_hist = true;
                    for (g, &c) in group.iter_mut().zip(row) {
                        *g += c;
                    }
                }
            }
            if count > 0 && fp >= 0 && (fp as usize) < self.nf {
                let fp = fp as usize;
                let i = tp * self.nf + fp;
                self.count[i] = count.min(u64::from(u32::MAX)) as u32;
                self.max[i] = max;
                self.sum_lin[i] = sum_lin;
                self.obs_s[i] = best_obs;
                self.occ_s[i] = best_ratio * best_obs;
                // A time-weighted mean never exceeds its maximum, so max-of-max alone suffices
                // (and keeps quantisation monotone: stored max = max of stored child maxima).
                self.occ_max[i] = occ_max;
                if any_hist {
                    self.p_lo[i] = hist_percentile(group, hist_cfg, pct.0);
                    self.p_hi[i] = hist_percentile(group, hist_cfg, pct.1);
                    let row = &mut self.hist[fp * self.bins..(fp + 1) * self.bins];
                    for (h, &g) in row.iter_mut().zip(group.iter()) {
                        *h = h.saturating_add(g);
                    }
                }
            }
            if any_hist {
                group.fill(0);
            }
            fc = group_end;
        }
        self.prov.merge(&child.prov);
    }

    /// Mean of cell `i` in dB (NaN when unobserved).
    pub fn mean_db(&self, i: usize) -> f32 {
        let n = self.count[i];
        if n == 0 {
            f32::NAN
        } else {
            super::stats::db(self.sum_lin[i] / f64::from(n))
        }
    }

    /// Sets cell `i` from decoded (stored) values.
    #[allow(clippy::too_many_arguments)]
    pub fn set_decoded(
        &mut self,
        i: usize,
        count: u32,
        max: f32,
        mean_db: f32,
        p_lo: f32,
        p_hi: f32,
        occupancy: f32,
        occ_max: f32,
        coverage: f32,
    ) {
        self.count[i] = count;
        self.max[i] = max;
        self.sum_lin[i] = undb(mean_db) * f64::from(count);
        self.p_lo[i] = p_lo;
        self.p_hi[i] = p_hi;
        let obs = f64::from(coverage) * self.t_cell_s;
        self.obs_s[i] = obs;
        self.occ_s[i] = f64::from(occupancy) * obs;
        self.occ_max[i] = occ_max;
    }
}

/// Percentiles and occupancy for a column's entries (sorted by `f`). Values without a caller
/// threshold use `floor[f] + margin`; when the floor is unknown (cold start), the 20th percentile
/// of every value in the column (across frequency) stands in for it.
pub(crate) fn column_stats(
    col: &[ColEntry],
    floor: Option<&[f32]>,
    margin: f32,
    pct: (f32, f32),
    scratch: &mut Vec<f32>,
    mut out: impl FnMut(usize, f32, f32, f64),
) {
    if col.is_empty() {
        return;
    }
    let known = |f: u32| floor.is_some_and(|fl| fl[f as usize].is_finite());
    let cold = if col.iter().any(|e| e.thr.is_nan() && !known(e.f)) {
        scratch.clear();
        scratch.extend(col.iter().map(|e| e.v));
        exact_percentile(scratch, 20.0)
    } else {
        f32::NAN
    };
    let mut i = 0;
    while i < col.len() {
        let f = col[i].f;
        let mut j = i + 1;
        while j < col.len() && col[j].f == f {
            j += 1;
        }
        scratch.clear();
        scratch.extend(col[i..j].iter().map(|e| e.v));
        let plo = exact_percentile(scratch, pct.0);
        let phi = exact_percentile(scratch, pct.1);
        let base = if known(f) {
            floor.map_or(cold, |fl| fl[f as usize])
        } else {
            cold
        };
        let thr_default = base + margin;
        let mut occ = 0.0;
        for e in &col[i..j] {
            let thr = if e.thr.is_nan() { thr_default } else { e.thr };
            if e.v > thr {
                occ += f64::from(e.dur_s);
            }
        }
        out(f as usize, plo, phi, occ);
        i = j;
    }
}
