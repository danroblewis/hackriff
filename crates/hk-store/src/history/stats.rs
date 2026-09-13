//! Percentile helpers: exact order statistics for level-0 cells and histogram percentiles for
//! rolled-up cells.

use super::config::HistogramConfig;

/// Exact percentile `q` (0–100) of `values`, with linear interpolation between order statistics
/// at position `q/100·(n−1)` (numpy's default). Reorders `values`. `NaN` when empty.
pub fn exact_percentile(values: &mut [f32], q: f32) -> f32 {
    let n = values.len();
    if n == 0 {
        return f32::NAN;
    }
    let pos = f64::from(q.clamp(0.0, 100.0)) / 100.0 * (n - 1) as f64;
    let lo = pos.floor() as usize;
    let frac = (pos - lo as f64) as f32;
    let (_, v_lo, right) = values.select_nth_unstable_by(lo, f32::total_cmp);
    let v_lo = *v_lo;
    if frac == 0.0 || right.is_empty() {
        return v_lo;
    }
    let v_hi = right.iter().copied().fold(f32::INFINITY, f32::min);
    v_lo + (v_hi - v_lo) * frac
}

/// Percentile `q` (0–100) of a fixed-bin histogram, assuming mass is uniform within each bin.
///
/// **Error bound:** the result lies in the same bin as the pooled order statistic `x₍⌈q·n/100⌉₎`
/// (1-based rank), so it is within one bin width (`step_db`, default 0.5 dB) of that sample,
/// whenever the sample is inside the histogram range. Where the distribution is dense this is also
/// within a step of the interpolated (numpy) percentile used at level 0; where the rank falls in a
/// gap between modes (e.g. noise vs bursts) the two rank conventions can differ by the gap.
/// Values outside the range were clamped into the end bins, so percentiles there are only bounds.
/// `NaN` when the histogram is empty.
pub fn hist_percentile(counts: &[u32], cfg: &HistogramConfig, q: f32) -> f32 {
    let total: u64 = counts.iter().map(|&c| u64::from(c)).sum();
    if total == 0 {
        return f32::NAN;
    }
    let rank = f64::from(q.clamp(0.0, 100.0)) / 100.0 * total as f64;
    let mut cum = 0.0f64;
    let mut last_nonzero = 0;
    for (b, &c) in counts.iter().enumerate() {
        if c == 0 {
            continue;
        }
        last_nonzero = b;
        let c = f64::from(c);
        if cum + c >= rank {
            let frac = ((rank - cum) / c).clamp(0.0, 1.0);
            return cfg.lo_db + cfg.step_db * (b as f64 + frac) as f32;
        }
        cum += c;
    }
    cfg.lo_db + cfg.step_db * (last_nonzero + 1) as f32
}

/// dB of a linear power, floored at −300 dB.
#[inline]
pub fn db(lin: f64) -> f32 {
    (10.0 * lin.max(1e-30).log10()) as f32
}

/// Linear power of a dB value.
#[inline]
pub fn undb(db: f32) -> f64 {
    10f64.powf(f64::from(db) / 10.0)
}

/// Rounds a dB value to the stored 0.01 dB resolution (query outputs are always rounded, so a
/// value read from memory and the same value read back from disk compare equal).
#[inline]
pub fn round_centi(v: f32) -> f32 {
    if v.is_finite() {
        (v * 100.0).round().clamp(-32767.0, 32767.0) / 100.0
    } else {
        v
    }
}

/// Rounds a fraction to the stored 1/65535 resolution.
#[inline]
pub fn round_frac(v: f32) -> f32 {
    if v.is_finite() {
        (v.clamp(0.0, 1.0) * 65535.0).round() / 65535.0
    } else {
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_matches_numpy_convention() {
        let mut v: Vec<f32> = (0..11).map(|i| i as f32).rev().collect();
        assert_eq!(exact_percentile(&mut v, 10.0), 1.0);
        let mut v = vec![1.0, 2.0, 3.0, 4.0];
        // pos = 0.3 → 1.3
        assert!((exact_percentile(&mut v, 10.0) - 1.3).abs() < 1e-6);
        assert!(exact_percentile(&mut [], 50.0).is_nan());
    }

    #[test]
    fn histogram_percentile_within_one_step() {
        let cfg = HistogramConfig {
            lo_db: -100.0,
            step_db: 0.5,
            bins: 200,
        };
        let mut counts = vec![0u32; 200];
        let mut values = Vec::new();
        let mut x = 12345u64;
        for _ in 0..5000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let v = -80.0 + (x % 20_000) as f32 / 1000.0; // −80..−60 dB
            values.push(v);
            counts[cfg.bin(v)] += 1;
        }
        for q in [10.0, 50.0, 90.0] {
            let exact = exact_percentile(&mut values.clone(), q);
            let h = hist_percentile(&counts, &cfg, q);
            assert!((exact - h).abs() <= cfg.step_db, "q{q}: {exact} vs {h}");
        }
    }
}
