//! Classical measurements behind the analog mode rules (T-065). Everything here reads the
//! detection-box samples of a [`ChannelSnippet`](hk_estimate::ChannelSnippet) plus C13's N0,
//! OBW99 and CFO; nothing looks at a label.
//!
//! 1. **Welch PSD** of the box (Hann, 50 % overlap, ~15 Hz bins on narrow boxes). The strongest
//!    line within the box gives the carrier candidate; its power over ±L bins (L covers drift),
//!    noise-subtracted, gives the **line SNR** and the **line fraction** ρ = P_line / P_signal.
//!    ρ ≈ 1 for a carrier, 1/(1 + m²/2) for tone AM, J₀²(β) (small) for FM, ≈ 0 for SSB.
//! 2. **Channel filter** ±W around the line (carrier-dominant signals) or the CFO, decimated,
//!    with `W = max(0.6·OBW99, 3.5 kHz)` capped at half the box, so a narrow OBW99 (a carrier,
//!    lightly modulated AM) still keeps voice-band sidebands.
//! 3. **Envelope variation** `κ − 1` on that channel, noise-corrected (see [`super`]).
//! 4. For a carrier-dominant signal, a **coherent carrier reference** ĉ: a centred double
//!    10 ms boxcar of the channel (zero phase, so it tracks offset and slow drift). Rotating by
//!    ĉ/|ĉ| splits the sidebands into an **in-phase** part I (amplitude modulation: a real
//!    envelope gives in-phase, mirror-symmetric sidebands) and a **quadrature** part Q (phase or
//!    frequency modulation). Circular noise adds N/2 to both, so `D = var I − var Q` is
//!    noise-free; its ratio to the block spread of `var Q` is the AM test, `D / P_sb` the **I/Q balance**
//!    (+1 AM, −1 narrow-index FM/PM), `sqrt(2D)/A` the tone-equivalent **AM depth** and
//!    `P_sb / A²` the **sideband-to-carrier ratio** (inverse carrier-to-sideband ratio).
//! 5. **On/off keying** of |ĉ|² in 5 ms steps: the fraction of steps below a quarter of the way
//!    from the noise level to the 90th-percentile "on" level.

use std::f64::consts::TAU;

use hk_dsp::{CpuFft, FftBackend};
use num_complex::{Complex, Complex32};

use super::ModeConfig;
use crate::dsp::{FirDecimator, lowpass_taps};

/// Smallest channel half-width for the envelope and sideband measurements, Hz (voice band).
const MIN_HALF_WIDTH_HZ: f64 = 3_500.0;
/// Target PSD bin width on narrow boxes, Hz.
const TARGET_BIN_HZ: f64 = 12.0;
/// Carrier line half-width floor, Hz (covers slow drift over the window).
const LINE_HALF_WIDTH_HZ: f64 = 40.0;
/// Carrier-reference boxcar length, s.
const REFERENCE_S: f64 = 0.010;
/// Keying measurement step, s.
const KEY_STEP_S: f64 = 0.005;
/// Blocks for the in-phase/quadrature t-statistic.
const BLOCKS: usize = 16;

/// Line measurements from the box PSD.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Line {
    /// Line frequency relative to the box centre, Hz (power centroid over the line bins).
    pub offset_hz: f64,
    /// Line power over its noise, dB.
    pub snr_db: f64,
    /// Line power over the box's noise-subtracted signal power.
    pub fraction: f64,
}

/// Carrier-referenced measurements (carrier-dominant signals only).
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Coherent {
    /// `var I − var Q` over its no-AM standard error (block spread of `var Q`).
    pub inphase_t: f64,
    /// `(var I − var Q) / P_sb`, clamped to ±1.
    pub iq_balance: f64,
    /// Tone-equivalent AM depth `sqrt(2·(var I − var Q)) / A` (0 when D ≤ 0).
    pub am_depth: f64,
    /// Smallest tone-equivalent AM depth this measurement would have called significant.
    pub am_depth_floor: f64,
    /// Noise-subtracted sideband power over carrier power, dB (`None` when not above noise).
    pub sideband_to_carrier_db: Option<f64>,
    /// Fraction of 5 ms steps with the carrier keyed off.
    pub keyed_off_fraction: f64,
    /// "On" carrier level over the reference noise, dB.
    pub keyed_on_snr_db: f64,
}

/// C13's view of the band.
#[derive(Clone, Copy, Debug)]
pub(super) struct Band {
    /// OBW99, Hz (`None` when C13 abstained).
    pub obw_hz: Option<f64>,
    /// CFO relative to the box centre, Hz.
    pub cfo_hz: f64,
    /// Noise density, FS²/Hz.
    pub n0: f64,
    /// Largest channel half-width, Hz (keeps adjacent emissions out, T-099).
    pub max_half_hz: Option<f64>,
}

/// All measurements.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Measured {
    pub line: Option<Line>,
    pub envelope_variation: Option<f64>,
    pub half_width_hz: Option<f64>,
    pub coherent: Option<Coherent>,
}

/// Measures `x` (box samples at `fs`, centred on the box).
pub(super) fn measure(
    x: &[Complex32],
    fs: f64,
    box_bw_hz: f64,
    band: Band,
    config: &ModeConfig,
) -> Measured {
    let Band {
        obw_hz,
        cfo_hz,
        n0,
        max_half_hz,
    } = band;
    let mut out = Measured::default();
    if n0.is_nan() || n0 <= 0.0 || x.len() < 1024 {
        return out;
    }
    out.line = line(x, fs, box_bw_hz, n0);
    let carrier = out.line.filter(|l| config.carrier_dominant(l));
    let centre = carrier.map_or(cfo_hz, |l| l.offset_hz);
    let half = (0.6 * obw_hz.unwrap_or(0.0))
        .max(MIN_HALF_WIDTH_HZ)
        .min(0.5 * box_bw_hz.max(2.0 * MIN_HALF_WIDTH_HZ))
        .min(0.4 * fs)
        .min(max_half_hz.unwrap_or(f64::INFINITY));
    out.half_width_hz = Some(half);
    let Some((w, rate, noise)) = channel(x, fs, centre, half, n0) else {
        return out;
    };
    out.envelope_variation = kappa(&w, noise);
    if carrier.is_some() {
        out.coherent = coherent(&w, rate, noise, n0, config.am_min_inphase_t);
    }
    out
}

/// Welch PSD of `x` (Hann, 50 % overlap), FS²/Hz in FFT bin order, with bins of about
/// `target_bin_hz` (256–16 384 bins, at least 4 segments' worth of samples when it can).
fn welch(x: &[Complex32], fs: f64, target_bin_hz: f64) -> Option<Vec<f64>> {
    let target = (fs / target_bin_hz).max(256.0) as usize;
    let mut nfft = target.next_power_of_two().min(16_384);
    while nfft > 256 && nfft * 4 > x.len() {
        nfft /= 2;
    }
    if nfft * 2 > x.len() {
        return None;
    }
    let win: Vec<f32> = (0..nfft)
        .map(|n| (0.5 - 0.5 * (TAU * n as f64 / nfft as f64).cos()) as f32)
        .collect();
    let w2: f64 = win.iter().map(|&v| f64::from(v).powi(2)).sum();
    let mut fft = CpuFft::new(nfft);
    let mut buf = vec![Complex32::default(); nfft];
    let mut psd = vec![0.0f64; nfft];
    let mut segs = 0usize;
    let mut start = 0;
    while start + nfft <= x.len() {
        for ((b, &s), &h) in buf.iter_mut().zip(&x[start..start + nfft]).zip(&win) {
            *b = s * h;
        }
        fft.forward(&mut buf);
        for (p, b) in psd.iter_mut().zip(&buf) {
            *p += f64::from(b.norm_sqr());
        }
        segs += 1;
        start += nfft / 2;
    }
    let scale = 1.0 / (segs as f64 * fs * w2);
    psd.iter_mut().for_each(|p| *p *= scale);
    Some(psd)
}

fn line(x: &[Complex32], fs: f64, box_bw_hz: f64, n0: f64) -> Option<Line> {
    let psd = welch(x, fs, TARGET_BIN_HZ)?;
    let nfft = psd.len();
    let df = fs / nfft as f64;
    // Box bins in ascending frequency: k ∈ [−kb, kb].
    let kb = ((0.5 * box_bw_hz / df).floor() as i64).min(nfft as i64 / 2 - 1);
    let at = |k: i64| psd[k.rem_euclid(nfft as i64) as usize];
    let (mut total, mut best, mut kbest) = (0.0, f64::MIN, 0i64);
    for k in -kb..=kb {
        let p = at(k);
        total += p * df;
        if p > best {
            best = p;
            kbest = k;
        }
    }
    let total = total - (2 * kb + 1) as f64 * df * n0;
    let l = ((LINE_HALF_WIDTH_HZ / df).ceil() as i64).max(3);
    let (mut lp, mut lf) = (0.0, 0.0);
    for k in (kbest - l).max(-kb)..=(kbest + l).min(kb) {
        let p = at(k) - n0;
        lp += p * df;
        lf += p * df * k as f64 * df;
    }
    let noise = (2 * l + 1) as f64 * df * n0;
    if lp <= 0.0 {
        return None;
    }
    Some(Line {
        offset_hz: lf / lp,
        snr_db: 10.0 * (lp / noise).log10(),
        fraction: if total > 0.0 { lp / total } else { 0.0 },
    })
}

/// The emission at the box centre measured between adjacent emissions (T-099).
#[derive(Clone, Copy, Debug)]
pub(super) struct Adjacent {
    /// OBW99 inside the channel limits, Hz.
    pub obw_hz: f64,
    /// Mid-point of that band relative to the box centre, Hz.
    pub center_hz: f64,
    /// Lower channel limit (the valley before the lower neighbour, or the analysed band's edge),
    /// relative to the box centre, Hz.
    pub lower_hz: f64,
    /// Upper channel limit, relative to the box centre, Hz.
    pub upper_hz: f64,
    /// A neighbour rises beyond the lower limit.
    pub lower_neighbour: bool,
    /// A neighbour rises beyond the upper limit.
    pub upper_neighbour: bool,
    /// Shallowest neighbour valley below the emission's peak density, dB.
    pub valley_db: f64,
    /// Lowest smoothed density in the analysed band (a noise upper bound), FS²/Hz.
    pub floor: f64,
}

/// A dip this far below the emission's peak (smoothed density), followed by a rise this far above
/// the dip, separates two emissions, dB.
const ADJACENT_VALLEY_DB: f64 = 10.0;
/// The emission's (and a neighbour's) smoothed peak over the band's lowest density, dB.
const ADJACENT_MIN_PEAK_DB: f64 = 10.0;
/// OBW fraction (as C13).
const OBW_FRACTION: f64 = 0.99;

/// OBW99 of the emission at the box centre, limited to the spectral valleys that separate it from
/// adjacent emissions (T-099). For when C13 abstains because strong neighbours fill the snippet
/// (their power inflates its noise reference or its band reaches the snippet edge).
///
/// The snippet's passband PSD is smoothed over a twentieth of the box. The emission's peak is the
/// highest density within a quarter box of the centre; walking outward, a side ends at a
/// **neighbour** when the density dips [`ADJACENT_VALLEY_DB`] below the emission's peak and then
/// rises the same amount above the dip's minimum (the limit is that minimum), or is **clear** when
/// it dips and never rises again. A side that never dips (the emission reaches the band edge)
/// abstains, and so does a scene with no neighbour on either side (C13's own measure covers an
/// isolated emission). OBW99 is taken inside the limits over the density above the band's lowest
/// smoothed density. Blind: no raster or channel plan.
pub(super) fn adjacent_obw(
    x: &[Complex32],
    fs: f64,
    passband_hz: f64,
    box_bw_hz: f64,
) -> Option<Adjacent> {
    if !(box_bw_hz > 0.0 && passband_hz > 0.0) {
        return None;
    }
    let psd = welch(x, fs, (box_bw_hz / 200.0).max(TARGET_BIN_HZ))?;
    let nfft = psd.len() as i64;
    let df = fs / nfft as f64;
    let kp = ((0.98 * passband_hz / df).floor() as i64).min(nfft / 2 - 1);
    if kp < 16 {
        return None;
    }
    let raw: Vec<f64> = (-kp..=kp)
        .map(|k| psd[k.rem_euclid(nfft) as usize])
        .collect();
    let m = raw.len();
    let h = ((0.025 * box_bw_hz / df).round() as usize).max(2);
    let mut prefix = Vec::with_capacity(m + 1);
    prefix.push(0.0);
    for v in &raw {
        prefix.push(prefix.last().copied().unwrap_or(0.0) + v);
    }
    let d: Vec<f64> = (0..m)
        .map(|i| {
            let (a, b) = (i.saturating_sub(h), (i + h + 1).min(m));
            (prefix[b] - prefix[a]) / (b - a) as f64
        })
        .collect();
    let floor = d.iter().copied().fold(f64::INFINITY, f64::min);
    if !(floor.is_finite() && floor > 0.0) {
        return None;
    }
    let lin = |db: f64| 10f64.powf(db / 10.0);
    let centre = kp as usize;
    let reach = ((0.25 * box_bw_hz / df).round() as usize).clamp(1, centre);
    let k0 = (centre - reach..=centre + reach).max_by(|&a, &b| d[a].total_cmp(&d[b]))?;
    let peak = d[k0];
    if peak < floor * lin(ADJACENT_MIN_PEAK_DB) {
        return None;
    }
    // (limit index, neighbour beyond it, depth below the emission peak in dB)
    let walk = |step: isize| -> Option<(usize, bool, f64)> {
        let (mut i, mut top, mut valley) = (k0, peak, None::<usize>);
        loop {
            let next = i as isize + step;
            if next < 0 || next >= m as isize {
                break;
            }
            i = next as usize;
            match valley {
                None => {
                    top = top.max(d[i]);
                    if d[i] <= top / lin(ADJACENT_VALLEY_DB) {
                        valley = Some(i);
                    }
                }
                Some(v) if d[i] < d[v] => valley = Some(i),
                Some(v) => {
                    if d[i] >= d[v] * lin(ADJACENT_VALLEY_DB)
                        && d[i] >= floor * lin(ADJACENT_MIN_PEAK_DB)
                    {
                        return Some((v, true, 10.0 * (top / d[v]).log10()));
                    }
                }
            }
        }
        valley.map(|_| (if step < 0 { 0 } else { m - 1 }, false, f64::INFINITY))
    };
    let (lo, lower_neighbour, lo_db) = walk(-1)?;
    let (hi, upper_neighbour, hi_db) = walk(1)?;
    if !(lower_neighbour || upper_neighbour) {
        return None;
    }
    let excess: Vec<f64> = raw[lo..=hi].iter().map(|v| (v - floor).max(0.0)).collect();
    let total: f64 = excess.iter().sum();
    if total <= 0.0 {
        return None;
    }
    let tail = 0.5 * (1.0 - OBW_FRACTION) * total;
    let mut acc = 0.0;
    let mut first = 0;
    for (j, e) in excess.iter().enumerate() {
        acc += e;
        if acc >= tail {
            first = j;
            break;
        }
    }
    acc = 0.0;
    let mut last = excess.len() - 1;
    for (j, e) in excess.iter().enumerate().rev() {
        acc += e;
        if acc >= tail {
            last = j;
            break;
        }
    }
    let rel = |j: usize| (j as f64 - kp as f64) * df;
    let (a, b) = (lo + first, lo + last.max(first));
    Some(Adjacent {
        obw_hz: (b - a + 1) as f64 * df,
        center_hz: 0.5 * (rel(a) + rel(b)),
        lower_hz: rel(lo),
        upper_hz: rel(hi),
        lower_neighbour,
        upper_neighbour,
        valley_db: lo_db.min(hi_db),
        floor,
    })
}

/// [`channel`] as `Complex32`, with its output rate.
pub(super) fn channel32(
    x: &[Complex32],
    fs: f64,
    centre: f64,
    half: f64,
) -> Option<(Vec<Complex32>, f64)> {
    let (w, rate, _) = channel(x, fs, centre, half, 1.0)?;
    Some((
        w.iter()
            .map(|z| Complex32::new(z.re as f32, z.im as f32))
            .collect(),
        rate,
    ))
}

/// `x` mixed down by `centre`, low-passed to ±`half` and decimated; with the output rate and
/// the per-sample noise variance.
fn channel(
    x: &[Complex32],
    fs: f64,
    centre: f64,
    half: f64,
    n0: f64,
) -> Option<(Vec<Complex<f64>>, f64, f64)> {
    let stop = (1.4 * half).min(0.49 * fs);
    let pass = half.min(stop - fs / 400.0);
    if pass <= 0.0 {
        return None;
    }
    let taps = lowpass_taps(fs, pass, stop, 50.0).ok()?;
    let sum: f64 = taps.iter().map(|&t| f64::from(t)).sum();
    let sq: f64 = taps.iter().map(|&t| f64::from(t).powi(2)).sum();
    let noise = n0 * fs * sq / (sum * sum);
    let factor = ((fs / (2.8 * stop.max(half))).floor() as usize).max(1);
    let skip = taps.len().div_ceil(factor) + 1;
    let gain = (1.0 / sum) as f32;
    let mut fir = FirDecimator::<Complex32>::new(taps, factor);
    let step = -centre / fs;
    let mut w = Vec::with_capacity(x.len() / factor + 1);
    for (n, &s) in x.iter().enumerate() {
        let ph = TAU * (step * n as f64).fract();
        if let Some(y) = fir.push(s * Complex32::new(ph.cos() as f32, ph.sin() as f32)) {
            let y = y * gain;
            w.push(Complex::new(f64::from(y.re), f64::from(y.im)));
        }
    }
    if w.len() <= 4 * skip {
        return None;
    }
    w.drain(..skip);
    Some((w, fs / factor as f64, noise))
}

/// `κ_s − 1` with the noise removed (module docs of [`super`]).
fn kappa(w: &[Complex<f64>], noise: f64) -> Option<f64> {
    let (mut m2, mut m4) = (0.0, 0.0);
    for v in w {
        let p = v.norm_sqr();
        m2 += p;
        m4 += p * p;
    }
    let (m2, m4) = (m2 / w.len() as f64, m4 / w.len() as f64);
    let s = m2 - noise;
    if s <= noise {
        return None;
    }
    let e4 = m4 - 4.0 * s * noise - 2.0 * noise * noise;
    Some(e4 / (s * s) - 1.0)
}

/// Centred moving average of odd length `m` (edges shortened).
fn centred_mean(v: &[Complex<f64>], m: usize) -> Vec<Complex<f64>> {
    let h = m / 2;
    let mut prefix = Vec::with_capacity(v.len() + 1);
    prefix.push(Complex::new(0.0, 0.0));
    for &s in v {
        let last = *prefix.last().unwrap();
        prefix.push(last + s);
    }
    (0..v.len())
        .map(|n| {
            let (a, b) = (n.saturating_sub(h), (n + h + 1).min(v.len()));
            (prefix[b] - prefix[a]) / (b - a) as f64
        })
        .collect()
}

fn coherent(w: &[Complex<f64>], rate: f64, noise: f64, n0: f64, t_min: f64) -> Option<Coherent> {
    let m = (((REFERENCE_S * rate).round() as usize) | 1).max(3);
    let c = centred_mean(&centred_mean(w, m), m);
    // Noise variance of ĉ: the triangle kernel (length 2m − 1, sum 1) has Σg² = (2m² + 1)/(3m³).
    let mf = m as f64;
    let ref_noise = n0 * rate * (2.0 * mf * mf + 1.0) / (3.0 * mf * mf * mf);
    let (a, b) = (m, w.len().saturating_sub(m));
    if b <= a + BLOCKS * 8 {
        return None;
    }
    let mut i_ = Vec::with_capacity(b - a);
    let mut q_ = Vec::with_capacity(b - a);
    let mut carrier = 0.0;
    for n in a..b {
        let r = c[n].norm();
        carrier += c[n].norm_sqr();
        let z = if r > 0.0 {
            w[n] * c[n].conj() / r
        } else {
            w[n]
        };
        i_.push(z.re);
        q_.push(z.im);
    }
    let a2 = carrier / (b - a) as f64 - ref_noise;
    if a2 <= 0.0 {
        return None;
    }
    let len = i_.len() / BLOCKS;
    let var = |v: &[f64]| {
        let mean = v.iter().sum::<f64>() / v.len() as f64;
        v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / v.len() as f64
    };
    let mut ds = Vec::with_capacity(BLOCKS);
    let mut qs = Vec::with_capacity(BLOCKS);
    let mut ss = 0.0;
    for k in 0..BLOCKS {
        let (vi, vq) = (
            var(&i_[k * len..(k + 1) * len]),
            var(&q_[k * len..(k + 1) * len]),
        );
        ds.push(vi - vq);
        qs.push(vq);
        ss += vi + vq;
    }
    let d = ds.iter().sum::<f64>() / BLOCKS as f64;
    // Standard error of D without in-phase modulation: var I would scatter like var Q, so the
    // block spread of var Q (noise, plus any quadrature modulation) scales it. The spread of D
    // itself would also carry the programme's loudness changes and hide speech AM.
    let q_mean = qs.iter().sum::<f64>() / BLOCKS as f64;
    let q_sd = (qs.iter().map(|x| (x - q_mean).powi(2)).sum::<f64>() / (BLOCKS - 1) as f64).sqrt();
    let se = std::f64::consts::SQRT_2 * q_sd / (BLOCKS as f64).sqrt();
    let t = if se > 0.0 { d / se } else { d.signum() * 1e9 };
    let p_sb = ss / BLOCKS as f64 - noise;
    let balance = (d / p_sb.max(d.abs()).max(f64::MIN_POSITIVE)).clamp(-1.0, 1.0);

    // Keying: |ĉ|² every 5 ms.
    let step = ((KEY_STEP_S * rate).round() as usize).max(1);
    let mut p: Vec<f64> = (a..b).step_by(step).map(|n| c[n].norm_sqr()).collect();
    p.sort_by(f64::total_cmp);
    let on = p[(p.len() * 9 / 10).min(p.len() - 1)];
    let threshold = ref_noise + 0.25 * (on - ref_noise);
    let off = p.iter().filter(|&&v| v < threshold).count() as f64 / p.len() as f64;

    Some(Coherent {
        inphase_t: t,
        iq_balance: balance,
        am_depth: (2.0 * d.max(0.0) / a2).sqrt(),
        am_depth_floor: (2.0 * t_min * se / a2).sqrt(),
        sideband_to_carrier_db: (p_sb > 0.0).then(|| 10.0 * (p_sb / a2).log10()),
        keyed_off_fraction: off,
        keyed_on_snr_db: 10.0 * ((on - ref_noise).max(f64::MIN_POSITIVE) / ref_noise).log10(),
    })
}
