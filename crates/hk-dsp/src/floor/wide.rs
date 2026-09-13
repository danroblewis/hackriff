//! The learned response shape and the wide-signal detection reference
//! ([`FloorKind::Wide`](super::FloorKind)).
//!
//! **Response shape `S`** (per bin, ≤ 1): the floor's static *downward* features (baseband
//! roll-off, notch filters, the low side of a tilt) that a 256-bin block estimator cannot resolve.
//! Learned per segment:
//! 1. each bin tracks the 25 % quantile of `ln psd` over time (Robbins–Monro, τ =
//!    `shape_time_constant_s`), so a signal present in fewer than 75 % of frames barely moves it;
//! 2. the tracked level is morphologically **opened** over `shape_min_width_bins` (carriers and
//!    other bumps narrower than that vanish, dips survive);
//! 3. it is referenced to the frame's FCME band floor on the same quantile scale (FCME finds the
//!    floor between signals up to 80 % occupancy);
//! 4. a soft deadband maps it to 1 within `shape_deadband_db` plus four times the larger of the
//!    tracker's standard error and the measured bin-to-bin spread of the tracked level.
//!
//! Signals only add power, so upward features (wide signals) never enter `S`. `S` applies from a
//! segment's 32nd frame. Blocks whose shape varies by more than 1 dB are re-estimated by FCME on
//! `psd / S`; flatter blocks take their raw floor over the block's mean shape. The per-bin floors
//! are the interpolated normalised block floors times `S`.
//!
//! **Wide reference** (on the normalised block floors, dB): clamp from below at the
//! `floor_quantile` level `Q`, `N = max(F, Q)`; slope-limited lower envelope
//! `E[b] = min_{|j−b| ≤ H} (N[j] + g·|b−j|)`; a block is **step-like** when `N[b] − E[b] >
//! step_db`. Step-like blocks take `min(F[b], min_{|j−b| ≤ H} N[j])`, all others their own `F[b]`.
//! A gradual slope below `g + step_db/H` dB per block (0.37 dB per 64 bins at the defaults, 23 dB
//! across 4096 bins) is never step-like, so the reference equals the per-frame floor there; the
//! interior of a flat signal at least `step_db + g·d` above the floor, `d` blocks from its nearest
//! edge, is. Limits: signals wider than `(1 − floor_quantile)` of the span, and dips wider than
//! `floor_quantile` of it, are ambiguous with a floor step and read as floor.

use super::{BlockFcme, BlockLayout, WideReferenceConfig, gamma};

/// Sliding minimum or maximum over `±h` (van Herk / Gil-Werman, `O(n)`; allocation-free).
fn sliding_extreme(
    x: &[f32],
    h: usize,
    out: &mut [f32],
    fwd: &mut [f32],
    bwd: &mut [f32],
    take_min: bool,
) {
    let n = x.len();
    let pick = |a: f32, b: f32| if take_min { a.min(b) } else { a.max(b) };
    let init = if take_min {
        f32::INFINITY
    } else {
        f32::NEG_INFINITY
    };
    let naive = |i: usize| {
        let lo = i.saturating_sub(h);
        let hi = (i + h + 1).min(n);
        x[lo..hi].iter().copied().fold(init, pick)
    };
    let w = 2 * h + 1;
    if h == 0 || n <= w {
        for (i, o) in out.iter_mut().enumerate() {
            *o = naive(i);
        }
        return;
    }
    for start in (0..n).step_by(w) {
        let end = (start + w).min(n);
        fwd[start] = x[start];
        for i in start + 1..end {
            fwd[i] = pick(fwd[i - 1], x[i]);
        }
        bwd[end - 1] = x[end - 1];
        for i in (start..end - 1).rev() {
            bwd[i] = pick(bwd[i + 1], x[i]);
        }
    }
    for (i, o) in out.iter_mut().enumerate() {
        *o = if i < h || i + h >= n {
            naive(i)
        } else {
            pick(bwd[i - h], fwd[i + h])
        };
    }
}

/// The learned downward response shape of one segment.
#[derive(Debug, Default)]
pub(crate) struct ResponseShape {
    mean: Vec<f32>,
    updates: u64,
    shape: Vec<f32>,
    /// Per block: the shape varies by more than [`ACTIVE_RANGE_DB`] over it (FCME on `psd / S`).
    active: Vec<bool>,
    /// Per block: mean `ln S` (an inactive block's floor is its raw floor over `exp` of this).
    block_ln: Vec<f32>,
    any_active: bool,
    eroded: Vec<f32>,
    opened: Vec<f32>,
    fwd: Vec<f32>,
    bwd: Vec<f32>,
    since_recompute: u32,
    /// `n_avg` of the cached `q_ln`.
    q_n: f64,
    /// `ln` of the unit-mean `Gamma(n)` quantile at [`SHAPE_QUANTILE`].
    q_ln: f32,
}

/// Frames between shape recomputations (the level tracker updates every frame).
const RECOMPUTE_FRAMES: u32 = 4;

/// Quantile of each bin's PSD over time that the shape is learned from.
pub(crate) const SHAPE_QUANTILE: f64 = 0.25;

/// A block whose shape varies by more than this is re-estimated by FCME on `psd / S`; a flatter
/// one uses its raw floor over the mean shape, dB.
const ACTIVE_RANGE_DB: f32 = 1.0;

/// Frames of a segment before the shape applies: until then the per-bin quantile tracker is too
/// noisy (and, in a dense band, too close to its start) to tell a floor dip from a gap between
/// signals, and the floors are the plain block estimates.
const MIN_SHAPE_UPDATES: u64 = 32;

impl ResponseShape {
    pub(crate) fn resize(&mut self, bins: usize, blocks: usize) {
        for v in [
            &mut self.mean,
            &mut self.eroded,
            &mut self.opened,
            &mut self.fwd,
            &mut self.bwd,
        ] {
            v.resize(bins, 0.0);
        }
        self.shape.resize(bins, 1.0);
        self.active.resize(blocks, false);
        self.block_ln.resize(blocks, 0.0);
        self.reset();
    }

    /// Forgets the learned shape (a new receiver state).
    pub(crate) fn reset(&mut self) {
        self.updates = 0;
        self.shape.fill(1.0);
        self.active.fill(false);
        self.block_ln.fill(0.0);
        self.any_active = false;
        self.since_recompute = 0;
    }

    /// Per-bin shape `S` (linear, ≤ 1).
    pub(crate) fn shape(&self) -> &[f32] {
        &self.shape
    }

    /// Block `b` needs FCME on `psd / S` (its shape varies by more than [`ACTIVE_RANGE_DB`]).
    pub(crate) fn block_active(&self, b: usize) -> bool {
        self.active[b]
    }

    /// Mean `ln S` over block `b`.
    pub(crate) fn block_ln(&self, b: usize) -> f32 {
        self.block_ln[b]
    }

    /// Some bin has `S < 1`.
    pub(crate) fn any_active(&self) -> bool {
        self.any_active
    }

    /// Folds in one frame's PSD (non-finite and non-positive bins are skipped) and recomputes the
    /// shape every few frames.
    ///
    /// The per-bin level is a Robbins–Monro tracker of the [`SHAPE_QUANTILE`] quantile of
    /// `ln psd` (step `1/((k+1)·f)` early, `alpha/f` in steady state, `f` the density of
    /// `ln Gamma(n)/n` at that quantile ≈ `0.3178·√n`), so a signal present in fewer than
    /// `1 − SHAPE_QUANTILE` of the frames barely moves it. The deadband widens by four times the
    /// larger of the tracker's model standard error (`5.9/√(frames·n)` dB) and the measured
    /// bin-to-bin spread of the tracked level (MAD of adjacent differences / √2: correlated
    /// frames, fixed ripple), so neither noise nor ripple enters the shape through the erosion.
    pub(crate) fn update(
        &mut self,
        psd: &[f32],
        alpha: f64,
        n_avg: f64,
        band_floor: f32,
        cfg: &WideReferenceConfig,
        layout: &BlockLayout,
    ) {
        let q = SHAPE_QUANTILE as f32;
        // Density of ln Gamma(n)/n at its quantile ≈ the standard normal density there times √n.
        let z = 0.674_49_f64;
        let density = (-0.5 * z * z).exp() / std::f64::consts::TAU.sqrt() * n_avg.max(0.1).sqrt();
        if self.updates == 0 {
            for (m, &p) in self.mean.iter_mut().zip(psd) {
                *m = if p.is_finite() && p > 0.0 {
                    p.ln()
                } else {
                    f32::MIN_POSITIVE.ln()
                };
            }
        } else {
            let step = (alpha.max(1.0 / (self.updates as f64 + 1.0)) / density) as f32;
            let (up, down) = (step * q, step * (1.0 - q));
            for (m, &p) in self.mean.iter_mut().zip(psd) {
                if p.is_finite() && p > 0.0 {
                    if p.ln() < *m {
                        *m -= down;
                    } else {
                        *m += up;
                    }
                }
            }
        }
        self.updates += 1;
        self.since_recompute += 1;
        if self.updates < MIN_SHAPE_UPDATES
            || (self.since_recompute < RECOMPUTE_FRAMES && self.updates > MIN_SHAPE_UPDATES)
        {
            return;
        }
        self.since_recompute = 0;
        let frames_eff = (self.updates as f64).min(2.0 / alpha.max(1e-12));
        let model_db = 5.9 / (frames_eff * n_avg.max(0.1)).sqrt();
        // Measured spread (`opened` is scratch until the dilation below).
        let n = self.mean.len();
        let measured_db = if n > 2 {
            for (d, w) in self.opened.iter_mut().zip(self.mean.windows(2)) {
                *d = (w[1] - w[0]).abs();
            }
            let diffs = &mut self.opened[..n - 1];
            let (_, &mut mad, _) = diffs.select_nth_unstable_by((n - 1) / 2, f32::total_cmp);
            // The MAD of |N(0, 2σ²)| is 0.6745·√2·σ; ln → dB.
            f64::from(mad) / (0.6745 * std::f64::consts::SQRT_2) * 10.0 / std::f64::consts::LN_10
        } else {
            0.0
        };
        let deadband_db = cfg.shape_deadband_db + 4.0 * model_db.max(measured_db);
        let h = cfg.shape_min_width_bins / 2;
        sliding_extreme(
            &self.mean,
            h,
            &mut self.eroded,
            &mut self.fwd,
            &mut self.bwd,
            true,
        );
        sliding_extreme(
            &self.eroded,
            h,
            &mut self.opened,
            &mut self.fwd,
            &mut self.bwd,
            false,
        );
        // Reference: the frame's FCME band floor on the tracker's quantile scale. FCME finds the
        // floor between signals up to 80 % occupancy, so a dense band never reads as the level
        // its gaps dip from.
        if self.q_n != n_avg {
            self.q_n = n_avg;
            self.q_ln = gamma::mean_quantile(n_avg.max(0.1), SHAPE_QUANTILE).ln() as f32;
        }
        let u = band_floor.max(1e-37).ln() + self.q_ln;
        // Soft deadband `d` (ln units): `ln S = r + d·exp((r + d)/d)` below `−d`, 0 above, so the
        // shape is continuous where learning starts (a step there would bias the blocks across
        // it) and approaches `r` for deep features.
        let d = (deadband_db * std::f64::consts::LN_10 / 10.0) as f32;
        self.any_active = false;
        self.active.fill(false);
        if !u.is_finite() {
            self.shape.fill(1.0);
            return;
        }
        for (s, &o) in self.shape.iter_mut().zip(&self.opened) {
            let r = o - u;
            *s = if r < -d {
                (r + d * ((r + d) / d).exp()).exp().max(1e-6)
            } else {
                1.0
            };
        }
        let range_ln = ACTIVE_RANGE_DB * std::f32::consts::LN_10 / 10.0;
        for b in 0..layout.count() {
            let (mut lo, mut hi, mut sum) = (f32::INFINITY, f32::NEG_INFINITY, 0.0f32);
            let block = &self.shape[layout.range(b)];
            for &v in block {
                let l = v.ln();
                lo = lo.min(l);
                hi = hi.max(l);
                sum += l;
            }
            self.active[b] = hi - lo > range_ln;
            self.block_ln[b] = sum / block.len() as f32;
            self.any_active |= lo < 0.0;
        }
    }
}

/// Normalised block floors: FCME of `psd / S` where the shape varies over the block (falling
/// back when that block is invalid), elsewhere `block_floor` over the block's mean shape.
pub(crate) fn normalised_blocks(
    shape: &ResponseShape,
    psd: &[f32],
    layout: &BlockLayout,
    fcme: &mut BlockFcme,
    block_floor: &[f32],
    scaled: &mut [f32],
    out: &mut [f32],
) {
    out.copy_from_slice(block_floor);
    if !shape.any_active() {
        return;
    }
    for (q, (&p, &s)) in scaled.iter_mut().zip(psd.iter().zip(shape.shape())) {
        *q = p / s;
    }
    for (b, o) in out.iter_mut().enumerate() {
        let flat = *o * (-shape.block_ln(b)).exp();
        *o = if shape.block_active(b) {
            let est = fcme.estimate_block(&scaled[layout.range(b)]);
            if est.valid { est.floor as f32 } else { flat }
        } else {
            flat
        };
    }
}

/// The wide-reference block values from normalised block floors (`db` and `sel` are scratch of
/// `floor.len()`).
pub(crate) fn wide_blocks(
    floor: &[f32],
    cfg: &WideReferenceConfig,
    db: &mut [f32],
    sel: &mut [f32],
    out: &mut [f32],
) {
    let nb = floor.len();
    for (d, &f) in db.iter_mut().zip(floor) {
        *d = 10.0 * f.max(1e-37).log10();
    }
    sel.copy_from_slice(db);
    let k = ((nb - 1) as f64 * cfg.floor_quantile).round() as usize;
    let q = if nb == 1 {
        sel[0]
    } else {
        let (_, &mut v, _) = sel.select_nth_unstable_by(k, f32::total_cmp);
        v
    };
    let h = cfg.half_width_blocks;
    let g = cfg.slope_db_per_block as f32;
    let step = cfg.step_db as f32;
    for b in 0..nb {
        let lo = b.saturating_sub(h);
        let hi = (b + h + 1).min(nb);
        let (mut e, mut m) = (f32::INFINITY, f32::INFINITY);
        for (j, &d) in db.iter().enumerate().take(hi).skip(lo) {
            let n = d.max(q);
            e = e.min(n + g * b.abs_diff(j) as f32);
            m = m.min(n);
        }
        let v = if db[b].max(q) - e > step {
            db[b].min(m)
        } else {
            db[b]
        };
        out[b] = 10f32.powf(v / 10.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sliding_extreme_matches_naive() {
        let x: Vec<f32> = (0..97).map(|i| ((i * 37) % 23) as f32).collect();
        let mut out = vec![0.0; 97];
        let (mut f, mut b) = (vec![0.0; 97], vec![0.0; 97]);
        for h in [0usize, 1, 3, 7, 40, 60] {
            for take_min in [true, false] {
                sliding_extreme(&x, h, &mut out, &mut f, &mut b, take_min);
                for (i, &got) in out.iter().enumerate() {
                    let lo = i.saturating_sub(h);
                    let hi = (i + h + 1).min(97);
                    let w = &x[lo..hi];
                    let want = if take_min {
                        w.iter().copied().fold(f32::INFINITY, f32::min)
                    } else {
                        w.iter().copied().fold(f32::NEG_INFINITY, f32::max)
                    };
                    assert_eq!(got, want, "h {h} i {i} min {take_min}");
                }
            }
        }
    }

    #[test]
    fn wide_blocks_keep_slopes_and_cut_plateaus() {
        let cfg = WideReferenceConfig::default();
        let nb = 61;
        let (mut db, mut sel, mut out) = (vec![0.0; nb], vec![0.0; nb], vec![0.0; nb]);
        // 12 dB tilt: untouched (up to the dB round trip).
        let tilt: Vec<f32> = (0..nb).map(|b| 10f32.powf(0.2 * b as f32 / 10.0)).collect();
        wide_blocks(&tilt, &cfg, &mut db, &mut sel, &mut out);
        assert!(
            out.iter()
                .zip(&tilt)
                .all(|(&o, &t)| (o / t - 1.0).abs() < 1e-5),
            "{out:?}"
        );
        // +10 dB plateau over 29 blocks: its interior reads the floor.
        let mut plateau = vec![1.0f32; nb];
        plateau[16..45].fill(10.0);
        wide_blocks(&plateau, &cfg, &mut db, &mut sel, &mut out);
        assert!(
            out[16..45].iter().all(|&v| (v - 1.0).abs() < 1e-5),
            "{out:?}"
        );
        assert!(out[..16].iter().all(|&v| (v - 1.0).abs() < 1e-5));
    }
}
