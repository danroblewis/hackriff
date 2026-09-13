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

/// Centre IF for rate `c`: IF smoothed over 0.6 T, the best of 8 phases by eye opening, then
/// 2-means, Fisher ratio, occupancy, valley depth and decision periodicity. `None` when
/// T < 2 samples, the IF is shorter than 16 T, or fewer than 16 valid centres.
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
