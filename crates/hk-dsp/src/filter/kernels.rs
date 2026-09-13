//! Inner-loop kernels shared by the channelizer and the DDC.
//!
//! Reductions accumulate into eight independent lanes so LLVM can vectorise them without
//! `-ffast-math` (floating-point addition is not associative, so a single accumulator cannot
//! be split into SIMD lanes by the compiler). The lane grouping depends only on the slice
//! contents, so results are bit-identical however the input stream was chunked.

use num_complex::Complex32;

use crate::stft::IqSample;

const LANES: usize = 8;

/// `Σ taps[i]·x[i]` for real taps and complex samples. Uses the shorter length.
#[inline]
#[allow(clippy::needless_range_loop)]
pub fn dot_real(taps: &[f32], x: &[Complex32]) -> Complex32 {
    let n = taps.len().min(x.len());
    let (taps, x) = (&taps[..n], &x[..n]);
    let mut re = [0.0f32; LANES];
    let mut im = [0.0f32; LANES];
    let tc = taps.chunks_exact(LANES);
    let xc = x.chunks_exact(LANES);
    let (tr, xr) = (tc.remainder(), xc.remainder());
    for (t, s) in tc.zip(xc) {
        for l in 0..LANES {
            re[l] += t[l] * s[l].re;
            im[l] += t[l] * s[l].im;
        }
    }
    let mut acc = Complex32::new(re.iter().sum(), im.iter().sum());
    for (&t, s) in tr.iter().zip(xr) {
        acc.re += t * s.re;
        acc.im += t * s.im;
    }
    acc
}

/// `Σ taps[i]·x[i]` for complex taps and complex samples (no conjugation). Uses the shorter
/// length.
#[inline]
#[allow(clippy::needless_range_loop)]
pub fn dot_complex(taps: &[Complex32], x: &[Complex32]) -> Complex32 {
    let n = taps.len().min(x.len());
    let (taps, x) = (&taps[..n], &x[..n]);
    let mut re = [0.0f32; LANES];
    let mut im = [0.0f32; LANES];
    let tc = taps.chunks_exact(LANES);
    let xc = x.chunks_exact(LANES);
    let (tr, xr) = (tc.remainder(), xc.remainder());
    for (t, s) in tc.zip(xc) {
        for l in 0..LANES {
            re[l] += t[l].re * s[l].re - t[l].im * s[l].im;
            im[l] += t[l].re * s[l].im + t[l].im * s[l].re;
        }
    }
    let mut acc = Complex32::new(re.iter().sum(), im.iter().sum());
    for (t, s) in tr.iter().zip(xr) {
        acc.re += t.re * s.re - t.im * s.im;
        acc.im += t.re * s.im + t.im * s.re;
    }
    acc
}

#[inline(always)]
#[allow(clippy::needless_range_loop)]
fn accumulate(acc: &mut [Complex32], taps: &[f32], x: &[Complex32]) {
    let k = acc.len();
    let (taps, x) = (&taps[..k], &x[..k]);
    for j in 0..k {
        acc[j].re += taps[j] * x[j].re;
        acc[j].im += taps[j] * x[j].im;
    }
}

/// Polyphase fold with an offset: `acc[(start + i) mod M] = Σ taps[i]·x[i]` with
/// `M = acc.len()` (acc is overwritten). Uses the shorter of `taps` and `x`.
#[inline]
pub fn fold(acc: &mut [Complex32], taps: &[f32], x: &[Complex32], start: usize) {
    acc.fill(Complex32::default());
    let m = acc.len();
    let n = taps.len().min(x.len());
    let s = start % m;
    let head = (m - s).min(n);
    accumulate(&mut acc[s..s + head], &taps[..head], &x[..head]);
    let mut i = head;
    while i + m <= n {
        // Equal, known lengths let the vectoriser drop bounds checks.
        accumulate(&mut acc[..m], &taps[i..i + m], &x[i..i + m]);
        i += m;
    }
    accumulate(&mut acc[..n - i], &taps[i..n], &x[i..n]);
}

#[inline(always)]
#[allow(clippy::needless_range_loop)]
fn accumulate_complex(acc: &mut [Complex32], taps: &[Complex32], x: &[Complex32]) {
    let k = acc.len();
    let (taps, x) = (&taps[..k], &x[..k]);
    for j in 0..k {
        acc[j].re += taps[j].re * x[j].re - taps[j].im * x[j].im;
        acc[j].im += taps[j].re * x[j].im + taps[j].im * x[j].re;
    }
}

/// [`fold`] with complex taps (a frequency-shifted prototype):
/// `acc[(start + i) mod M] = Σ taps[i]·x[i]`.
#[inline]
pub fn fold_complex(acc: &mut [Complex32], taps: &[Complex32], x: &[Complex32], start: usize) {
    acc.fill(Complex32::default());
    let m = acc.len();
    let n = taps.len().min(x.len());
    let s = start % m;
    let head = (m - s).min(n);
    accumulate_complex(&mut acc[s..s + head], &taps[..head], &x[..head]);
    let mut i = head;
    while i + m <= n {
        accumulate_complex(&mut acc[..m], &taps[i..i + m], &x[i..i + m]);
        i += m;
    }
    accumulate_complex(&mut acc[..n - i], &taps[i..n], &x[i..n]);
}

/// Converts `src` into `dst` (same length) with full scale 1.
#[inline]
pub fn convert_into<T: IqSample>(dst: &mut [Complex32], src: &[T]) {
    for (d, &s) in dst.iter_mut().zip(src) {
        *d = s.to_complex32();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cx(i: usize) -> Complex32 {
        Complex32::new((i as f32 * 0.37).sin(), (i as f32 * 0.11).cos())
    }

    #[test]
    fn kernels_match_naive_sums() {
        for n in [1, 7, 8, 9, 63, 100] {
            let taps: Vec<f32> = (0..n).map(|i| (i as f32 * 0.9).cos()).collect();
            let ctaps: Vec<Complex32> = (0..n).map(|i| cx(i + 5)).collect();
            let x: Vec<Complex32> = (0..n).map(cx).collect();
            let want_r: Complex32 = taps.iter().zip(&x).map(|(&t, &s)| s * t).sum();
            let want_c: Complex32 = ctaps.iter().zip(&x).map(|(&t, &s)| s * t).sum();
            assert!((dot_real(&taps, &x) - want_r).norm() < 1e-4);
            assert!((dot_complex(&ctaps, &x) - want_c).norm() < 1e-4);
            let m = 4;
            for start in 0..m {
                let mut acc = vec![Complex32::default(); m];
                fold(&mut acc, &taps, &x, start);
                for (k, a) in acc.iter().enumerate() {
                    let want: Complex32 = (0..n)
                        .filter(|i| (start + i) % m == k)
                        .map(|i| x[i] * taps[i])
                        .sum();
                    assert!((a - want).norm() < 1e-4, "n {n} start {start} k {k}");
                }
            }
        }
    }
}
