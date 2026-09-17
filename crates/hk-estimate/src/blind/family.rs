//! FSK symbol-centre statistics (S5 `symbol_centre_stats`) and envelope helpers.

use serde::{Deserialize, Serialize};

use super::util::{kmeans2, mean, median, moving_avg};

/// Instantaneous frequency sampled at the symbol centres of one candidate rate.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FskCentreStats {
    /// Candidate rate, Bd.
    pub rate_bd: f64,
    /// 2-means separation of the centre IF, Hz.
    pub separation_hz: f64,
    /// Separation over the within-cluster standard deviation.
    pub fisher_j: f64,
    /// Smaller cluster's share.
    pub occupancy: f64,
    /// Histogram density at the mid-point over the mean density at the two centres.
    pub valley: f64,
    /// Largest agreement of the decision sequence with itself at lags 1..8 (≈ 0.5 for data,
    /// ≈ 1 for a tone or chirp sampled at a multiple of its period).
    pub periodicity: f64,
    /// Symbol centres used.
    pub symbols: usize,
}

/// Fewest members the smaller of the two IF clusters may hold for the Fisher ratio to be a
/// measurement at all (T-311).
///
/// [`FskCentreStats::fisher_j`] is a separation divided by the **pooled within-cluster standard
/// deviation**, a second-order statistic: over `m₁` members its relative standard error is
/// `sqrt(2/m₁)`, so at 3σ it is `3·sqrt(2/m₁)`, which reaches **100 % of itself** at
/// `m₁ = 18 = (3·sqrt 2)²`. Below that the denominator is not a measurement, so the ratio is not
/// one either — a handful of outliers on one side of a single cluster produce a "separation"
/// indistinguishable from a keyed emission's.
///
/// This is the one place in the family scores where a lock genuinely **is** binary: with too few
/// members there is no continuous quantity to report, only a ratio of two numbers one of which was
/// not estimated. So the candidate is refused here, the score goes **absent** rather than low
/// (T-297's rule for `sweep_rate_hz_per_s`: absent means *not measured*), and a not-yet-locked
/// reading can never be scored as weak evidence for a different family.
///
/// Measured on the dev grid: `pulse` seed 2 read `blind_fsk` 0.07 over the full record and **1.00**
/// over its last eighth, on a minority cluster of 5 members out of 43 centres.
pub const MIN_CLUSTER_MEMBERS: f64 = 18.0;

/// 3σ standard error of a **proportion** over `m` draws: `3·sqrt(p(1−p)/m) ≤ 3·0.5/sqrt(m)`.
///
/// The same constant `feature_length_invariance::K_PROPORTION` derives, for the same reason.
pub(crate) const K_PROPORTION: f64 = 1.5;

/// 3σ **relative** standard error of a second-order sample statistic over `m` draws:
/// `Var(m₂)/m₂² = 2/m`, so `3·sqrt(2/m)`. The same constant
/// `feature_length_invariance::K_ORDER2` derives.
pub(crate) const K_ORDER2: f64 = 4.243;

/// A veto that is **no sharper than the measurement it reads** (T-311).
///
/// Every gate in C14's family scoring is a threshold on a continuous measurement, applied as a
/// step: pass, or multiply by a penalty. A step is only honest when the measurement it tests is
/// exact. None of these are — each is a sample statistic with its own standard error — and where a
/// class sits near a threshold, the shipped code read that statistic's *sampling noise* as a
/// multiplicative jump in the score, which is what put a lock/no-lock transition into a number
/// handed to a fitted Gaussian.
///
/// So each gate ramps linearly across ±3σ of its own statistic about the **unchanged** threshold:
/// identical outside that band, identical at the centre, and continuous where the shipped form
/// read noise. `width` is that 3σ, derived per gate from the count the statistic is formed over.
///
/// `exceeds` says which side is penalised: `true` when a value **above** `threshold` is the
/// failure (`periodicity`, `env_cv`, `valley`), `false` when a value **below** it is
/// (`occupancy`, `separation`).
pub(crate) fn soft_veto(
    value: f64,
    threshold: f64,
    width: f64,
    penalty: f64,
    exceeds: bool,
) -> f64 {
    let w = width.max(f64::MIN_POSITIVE);
    let t = ((value - (threshold - w)) / (2.0 * w)).clamp(0.0, 1.0);
    let t = if exceeds { t } else { 1.0 - t };
    1.0 - (1.0 - penalty) * t
}

/// Self-similarity above which a candidate's decisions are a tone or a chirp rather than data.
///
/// Unchanged from the shipped test (`periodicity > 0.95`); what changed is that the test is no
/// longer **sharper than the measurement feeding it** — see [`periodicity_penalty`].
const PERIODICITY_MAX: f64 = 0.95;

/// What the FSK score is multiplied by when the decisions are fully periodic.
const PERIODICITY_PENALTY: f64 = 0.1;

/// How much [`FskCentreStats::periodicity`] vetoes the FSK score, over a transition **as wide as
/// the statistic's own standard error** (T-311).
///
/// The shipped test was a hard `periodicity > 0.95`, penalising by ×0.1 on one side and not at all
/// on the other. `periodicity` is the largest agreement of a binary decision sequence with itself
/// at lags 1..8 — a **proportion** over `m` centres, whose own 3σ sampling spread is
/// `K_PROPORTION/sqrt(m)`: 0.04 over 1237 centres and 0.12 over 153. Broadcast FM sits almost
/// exactly on 0.95 (its instantaneous frequency is audio, so consecutive decisions agree), and the
/// shipped test read one waveform's *sampling noise* about that value as a ×10 change in the
/// score: measured on the dev grid, one `wfm` waveform scored `blind_fsk` **1.00** over the full
/// record and **0.10** over each of its halves and quarters, with `periodicity` 0.9425 against
/// 0.9504. That is a lock/no-lock transition driven by the observation window, and it is what
/// T-311 is about.
///
/// **A threshold may not be sharper than the measurement it reads.** So the penalty ramps linearly
/// across ±3σ of the proportion about [`PERIODICITY_MAX`]: identical to the shipped test at the
/// centre and at either extreme, and — where the shipped test was reading noise — a continuous
/// function of a quantity that was continuous all along. This is *not* a continuum invented to
/// smooth a genuine step: the genuine step, too few cluster members for the ratio to exist at all,
/// is handled by [`MIN_CLUSTER_MEMBERS`] refusing the candidate outright.
pub(crate) fn periodicity_penalty(periodicity: f64, m: usize) -> f64 {
    soft_veto(
        periodicity,
        PERIODICITY_MAX,
        K_PROPORTION / (m.max(1) as f64).sqrt(),
        PERIODICITY_PENALTY,
        true,
    )
}

impl FskCentreStats {
    /// Members of the smaller of the two IF clusters: what the Fisher ratio's denominator, and
    /// every statistic derived from the split, is actually estimated over.
    pub(crate) fn minority(&self) -> f64 {
        (self.occupancy * self.symbols as f64).max(1.0)
    }

    /// The four FSK vetoes, each ramped over its own statistic's 3σ (T-311, [`soft_veto`]).
    ///
    /// | gate | statistic | 3σ width, and what it is the error of |
    /// |---|---|---|
    /// | occupancy > 0.1 | the smaller cluster's share, a **proportion** over `symbols` centres | `K_PROPORTION/sqrt(m)` |
    /// | separation > 0.1·OBW | a difference of two cluster means, whose error is the within-cluster spread over the smaller cluster — and `separation/fisher_j` **is** that spread | `K_ORDER2·(sep/OBW)/(fisher_j·sqrt(m₁))` |
    /// | valley < 0.6 | a ratio of histogram densities; each density is a count in 3 of 24 bins, so it is formed over about `m/8` draws and its 3σ relative error is Poisson | `valley·3/sqrt(m/8)` |
    /// | periodicity > 0.95 | a **proportion** over `symbols` centres | `K_PROPORTION/sqrt(m)` |
    ///
    /// The thresholds and the penalties are exactly the shipped ones. Only the sharpness changed.
    pub(crate) fn veto_product(&self, obw: f64) -> f64 {
        let m = (self.symbols.max(1)) as f64;
        let m1 = self.minority();
        let sep_rel = self.separation_hz / obw.max(f64::MIN_POSITIVE);
        let sep_w = K_ORDER2 * sep_rel / (self.fisher_j.max(1e-9) * m1.sqrt());
        let valley_w = self.valley * 3.0 / (m / 8.0).sqrt();
        soft_veto(
            self.occupancy,
            OCCUPANCY_MIN,
            K_PROPORTION / m.sqrt(),
            OCCUPANCY_PENALTY,
            false,
        ) * soft_veto(
            sep_rel,
            SEPARATION_MIN_OBW,
            sep_w,
            SEPARATION_PENALTY,
            false,
        ) * soft_veto(self.valley, VALLEY_MAX, valley_w, VALLEY_PENALTY, true)
            * periodicity_penalty(self.periodicity, self.symbols)
    }
}

/// Smallest share the minority IF cluster may hold before the split is one level with outliers.
const OCCUPANCY_MIN: f64 = 0.1;
/// Penalty applied when it does not.
const OCCUPANCY_PENALTY: f64 = 0.3;
/// Smallest two-level separation, as a fraction of OBW99, that is a keyed deviation rather than
/// noise on one level.
const SEPARATION_MIN_OBW: f64 = 0.1;
/// Penalty applied when it does not.
const SEPARATION_PENALTY: f64 = 0.2;
/// Deepest density at the mid-point, relative to the two levels, that still reads as two levels.
const VALLEY_MAX: f64 = 0.6;
/// Penalty applied when it does not.
const VALLEY_PENALTY: f64 = 0.15;

/// Centre IF for rate `c`: IF smoothed over 0.6 T, the best of 8 phases by eye opening, then
/// 2-means, Fisher ratio, occupancy, valley depth and decision periodicity. `None` when
/// T < 2 samples, the IF is shorter than 16 T, fewer than 16 valid centres, or the smaller cluster
/// holds fewer than [`MIN_CLUSTER_MEMBERS`].
pub(crate) fn symbol_centre_stats(
    fi: &[f64],
    fs: f64,
    c: f64,
    valid: &[bool],
) -> Option<FskCentreStats> {
    let t = fs / c;
    if t < 2.0 || (fi.len() as f64) < 16.0 * t {
        return None;
    }
    let v = moving_avg(fi, ((0.6 * t) as usize).max(1));
    let mut best: Option<(f64, Vec<f64>)> = None;
    for j in 0..8 {
        let ph = t * j as f64 / 8.0;
        let count = ((v.len() as f64 - 1.0 - ph) / t) as usize;
        let s: Vec<f64> = (0..count)
            .map(|k| (ph + k as f64 * t) as usize)
            .filter(|&i| valid.is_empty() || valid[i.min(valid.len() - 1)])
            .map(|i| v[i])
            .collect();
        if s.len() < 16 {
            continue;
        }
        let med = median(&s);
        let eye = s.iter().map(|x| (x - med).abs()).sum::<f64>() / s.len() as f64;
        if best.as_ref().is_none_or(|b| eye > b.0) {
            best = Some((eye, s));
        }
    }
    let (_, s) = best?;
    let (q, lab) = kmeans2(&s);
    let sep = (q[1] - q[0]).abs();
    let mut vars = Vec::new();
    let mut n1 = 0usize;
    for k in 0..2u8 {
        let members: Vec<f64> = s
            .iter()
            .zip(&lab)
            .filter(|&(_, &l)| l == k)
            .map(|(&x, _)| x)
            .collect();
        if k == 1 {
            n1 = members.len();
        }
        if !members.is_empty() {
            let m = mean(&members);
            vars.push(members.iter().map(|x| (x - m).powi(2)).sum::<f64>() / members.len() as f64);
        }
    }
    let within = mean(&vars).sqrt();
    let frac1 = n1 as f64 / s.len() as f64;
    let occ = frac1.min(1.0 - frac1);
    // The ratio's denominator is estimated from the smaller cluster; below MIN_CLUSTER_MEMBERS it
    // is not estimated at all, and the candidate is refused rather than reported weakly (T-311).
    if occ * (s.len() as f64) < MIN_CLUSTER_MEMBERS {
        return None;
    }
    let (lo, hi) = (q[0].min(q[1]), q[0].max(q[1]));
    // np.histogram over 24 equal bins in [lo − 0.3 sep, hi + 0.3 sep].
    let e0 = lo - 0.3 * sep;
    let e1 = hi + 0.3 * sep;
    let bins = 24usize;
    let w = (e1 - e0) / bins as f64;
    let mut hist = [0.0f64; 24];
    if w > 0.0 {
        for &x in &s {
            if x >= e0 && x <= e1 {
                hist[(((x - e0) / w) as usize).min(bins - 1)] += 1.0;
            }
        }
    }
    let dens = |val: f64| -> f64 {
        let i = if w > 0.0 {
            (((val - e0) / w - 0.5).round().max(0.0) as usize).min(bins - 1)
        } else {
            0
        };
        let a = i.saturating_sub(1);
        let b = (i + 2).min(bins);
        hist[a..b].iter().sum::<f64>() / (b - a) as f64
    };
    let mid = 0.5 * (lo + hi);
    let valley = dens(mid) / (0.5 * (dens(lo) + dens(hi)) + 1e-9);
    let d: Vec<bool> = s.iter().map(|&x| x > mid).collect();
    let periodic = (1..9.min(d.len() / 4))
        .map(|p| {
            let agree = d[p..]
                .iter()
                .zip(&d[..d.len() - p])
                .filter(|(a, b)| a == b)
                .count();
            agree as f64 / (d.len() - p) as f64
        })
        .fold(0.0, f64::max);
    Some(FskCentreStats {
        rate_bd: c,
        separation_hz: sep,
        fisher_j: sep / (within + 1e-9),
        occupancy: occ,
        valley,
        periodicity: periodic,
        symbols: s.len(),
    })
}

/// Majority filter of width 5 on 0/1 labels (scipy `median_filter(size=5)`, reflect mode).
pub(crate) fn median5(lab: &[u8]) -> Vec<u8> {
    let n = lab.len() as isize;
    if n == 0 {
        return Vec::new();
    }
    let at = |j: isize| -> u8 {
        let mut k = j;
        // Half-sample symmetric reflection, repeated for very short inputs.
        loop {
            if k < 0 {
                k = -k - 1;
            } else if k >= n {
                k = 2 * n - k - 1;
            } else {
                break;
            }
        }
        lab[k as usize]
    };
    (0..n)
        .map(|i| u8::from((i - 2..=i + 2).map(at).map(u32::from).sum::<u32>() >= 3))
        .collect()
}

/// Longest run of `true` after bridging interior gaps shorter than `min_gap`; `(start, end)`.
/// `(0, len)` when there is none.
pub(crate) fn longest_run(mask: &[bool], min_gap: usize) -> (usize, usize) {
    let n = mask.len();
    let mut m = mask.to_vec();
    if min_gap > 1 {
        let mut i = 0;
        let mut seen_true = false;
        while i < n {
            if m[i] {
                seen_true = true;
                i += 1;
                continue;
            }
            let start = i;
            while i < n && !m[i] {
                i += 1;
            }
            if seen_true && i < n && i - start < min_gap {
                m[start..i].iter_mut().for_each(|v| *v = true);
            }
        }
    }
    let mut best = (0usize, 0usize);
    let mut i = 0;
    while i < n {
        if !m[i] {
            i += 1;
            continue;
        }
        let s = i;
        while i < n && m[i] {
            i += 1;
        }
        if i - s > best.1 - best.0 {
            best = (s, i);
        }
    }
    if best.1 == 0 { (0, n) } else { best }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn majority_filter_and_runs() {
        assert_eq!(
            median5(&[0, 1, 0, 0, 1, 1, 1, 0, 1]),
            vec![0, 0, 0, 1, 1, 1, 1, 1, 1]
        );
        let mask = [
            false, true, true, false, true, true, true, false, false, false, true,
        ];
        assert_eq!(longest_run(&mask, 2), (1, 7));
        assert_eq!(longest_run(&mask, 1), (4, 7));
        assert_eq!(longest_run(&[false; 4], 2), (0, 4));
    }
}
