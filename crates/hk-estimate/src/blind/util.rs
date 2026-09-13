//! Small numeric helpers with the semantics of the S5 prototype (numpy): centred moving average
//! with zero-padded edges, linear-interpolated quantiles, medians, 2-means with labels.

/// `np.convolve(v, ones(l)/l, "same")`: centred moving average, zero outside `v`, divided by
/// `l` everywhere (edges roll off, as in the prototype).
pub(crate) fn moving_avg(v: &[f64], l: usize) -> Vec<f64> {
    let l = l.max(1);
    let n = v.len();
    if l == 1 || n == 0 {
        return v.to_vec();
    }
    let mut prefix = Vec::with_capacity(n + 1);
    prefix.push(0.0);
    let mut acc = 0.0;
    for &x in v {
        acc += x;
        prefix.push(acc);
    }
    // same[i] = Σ v[i + (l−1)/2 − l + 1 ..= i + (l−1)/2] / l
    let back = l - 1 - (l - 1) / 2;
    let fwd = (l - 1) / 2;
    (0..n)
        .map(|i| {
            let a = i.saturating_sub(back);
            let b = (i + fwd + 1).min(n);
            (prefix[b] - prefix[a]) / l as f64
        })
        .collect()
}

/// Linear-interpolated quantile of an ascending slice (`np.quantile` default).
pub(crate) fn quantile_sorted(sorted: &[f64], p: f64) -> f64 {
    let n = sorted.len();
    if n == 0 {
        return f64::NAN;
    }
    let pos = p.clamp(0.0, 1.0) * (n - 1) as f64;
    let i = pos.floor() as usize;
    let t = pos - i as f64;
    if i + 1 < n {
        sorted[i] * (1.0 - t) + sorted[i + 1] * t
    } else {
        sorted[n - 1]
    }
}

/// Quantile of unsorted values.
pub(crate) fn quantile(v: &[f64], p: f64) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    quantile_sorted(&s, p)
}

/// Median (mean of the middle pair for even length).
pub(crate) fn median(v: &[f64]) -> f64 {
    quantile(v, 0.5)
}

pub(crate) fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        f64::NAN
    } else {
        v.iter().sum::<f64>() / v.len() as f64
    }
}

pub(crate) fn std(v: &[f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let m = mean(v);
    (v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / v.len() as f64).sqrt()
}

/// 2-means in 1-D, initialised at the 15 % and 85 % quantiles (S5 `kmeans_1d`). Returns the
/// centres (in initialisation order, not sorted) and a label per value.
pub(crate) fn kmeans2(v: &[f64]) -> ([f64; 2], Vec<u8>) {
    if v.is_empty() {
        return ([0.0, 0.0], Vec::new());
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let mut q = [quantile_sorted(&s, 0.15), quantile_sorted(&s, 0.85)];
    let label = |q: &[f64; 2], x: f64| u8::from((x - q[1]).abs() < (x - q[0]).abs());
    for _ in 0..30 {
        let (mut s0, mut n0, mut s1, mut n1) = (0.0, 0usize, 0.0, 0usize);
        for &x in v {
            if label(&q, x) == 0 {
                s0 += x;
                n0 += 1;
            } else {
                s1 += x;
                n1 += 1;
            }
        }
        let nq = [
            if n0 > 0 { s0 / n0 as f64 } else { q[0] },
            if n1 > 0 { s1 / n1 as f64 } else { q[1] },
        ];
        let close = |a: f64, b: f64| (a - b).abs() <= 1e-8 + 1e-5 * b.abs();
        let done = close(nq[0], q[0]) && close(nq[1], q[1]);
        q = nq;
        if done {
            break;
        }
    }
    let labels = v.iter().map(|&x| label(&q, x)).collect();
    (q, labels)
}

/// Instantaneous frequency `fs/2π · arg(x[n+1]·x*[n])`, Hz (length `len − 1`).
pub(crate) fn inst_freq(x: &[num_complex::Complex32], fs: f64) -> Vec<f64> {
    let scale = fs / std::f64::consts::TAU;
    x.windows(2)
        .map(|w| {
            let a = num_complex::Complex::new(f64::from(w[1].re), f64::from(w[1].im));
            let b = num_complex::Complex::new(f64::from(w[0].re), f64::from(w[0].im));
            (a * b.conj()).arg() * scale
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moving_avg_matches_numpy_same() {
        // np.convolve([1,2,3,4,5], ones(2)/2, "same") = [0.5, 1.5, 2.5, 3.5, 4.5]
        assert_eq!(
            moving_avg(&[1.0, 2.0, 3.0, 4.0, 5.0], 2),
            vec![0.5, 1.5, 2.5, 3.5, 4.5]
        );
        // ones(3)/3 -> [1, 2, 3, 4, 3]
        let m = moving_avg(&[1.0, 2.0, 3.0, 4.0, 5.0], 3);
        for (a, b) in m.iter().zip([1.0, 2.0, 3.0, 4.0, 3.0]) {
            assert!((a - b).abs() < 1e-12, "{m:?}");
        }
    }

    #[test]
    fn quantiles_and_kmeans() {
        assert_eq!(median(&[3.0, 1.0, 2.0, 4.0]), 2.5);
        assert!((quantile(&[0.0, 10.0], 0.1) - 1.0).abs() < 1e-12);
        let v: Vec<f64> = (0..100)
            .map(|i| if i % 4 == 0 { -3.0 } else { 5.0 })
            .collect();
        let (q, lab) = kmeans2(&v);
        assert!(
            (q[0] + 3.0).abs() < 1e-9 && (q[1] - 5.0).abs() < 1e-9,
            "{q:?}"
        );
        assert_eq!(lab[0], 0);
        assert_eq!(lab[1], 1);
    }
}
