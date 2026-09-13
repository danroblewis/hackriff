//! 2-FSK / GFSK burst demodulator (C20): quadrature discriminator → integrate → symbol timing
//! (seeded by the T-011 transition least squares, tracked by a Gardner loop) → hard bits and
//! LLR-like soft values, with lock quality.

use std::f64::consts::TAU;
use std::ops::Range;

use hk_dsp::DesignError;
use hk_estimate::ChannelSnippet;
use hk_estimate::blind::transitions::{LsGuards, rate_transitions_ls, transitions};
use num_complex::Complex32;
use serde::{Deserialize, Serialize};

use crate::dsp::lowpass_taps;

/// Demodulator id and version (`Demodulation.demod_version`).
pub const FSK_DEMOD_VERSION: &str = "hk-demod/c20-fsk@0.1.0";

/// Settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FskDemodConfig {
    /// Integrate-and-dump window as a fraction of a symbol (a moving average of the
    /// discriminator; < 1 keeps GFSK symbol centres clear of the neighbours' ISI).
    pub integrate_fraction: f64,
    /// Gardner loop noise bandwidth × symbol period.
    pub loop_bandwidth: f64,
    /// Gardner loop damping.
    pub loop_damping: f64,
    /// On-signal gate: 2-symbol power average over the pad noise power.
    pub power_gate: f64,
    /// Channel filter stopband, dB.
    pub filter_stopband_db: f64,
    /// Fewest samples per symbol.
    pub min_sps: f64,
    /// Fewest symbols.
    pub min_symbols: usize,
    /// Symbols of margin around the requested range.
    pub edge_symbols: f64,
}

impl Default for FskDemodConfig {
    fn default() -> Self {
        Self {
            integrate_fraction: 0.7,
            loop_bandwidth: 0.01,
            loop_damping: 0.707,
            power_gate: 2.0,
            filter_stopband_db: 40.0,
            min_sps: 3.0,
            min_symbols: 16,
            edge_symbols: 2.0,
        }
    }
}

/// What the demodulator is asked to do.
#[derive(Clone, Debug, PartialEq)]
pub struct FskDemodRequest {
    /// Symbol rate, Bd.
    pub rate_bd: f64,
    /// Deviation, Hz (sets the channel filter; `None`: from the bandwidth).
    pub deviation_hz: Option<f64>,
    /// Mixing frequency relative to the snippet centre, Hz (C13/C14 CFO).
    pub cfo_hz: f64,
    /// Burst samples in the snippet.
    pub range: Range<usize>,
    /// Detection bandwidth, Hz.
    pub bandwidth_hz: Option<f64>,
}

/// How the timing seed was obtained.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TimingSeedMethod {
    /// T-011 transition least squares (rate) + transition phase fit.
    TransitionLs,
    /// Feed-forward eye search over 16 phases (too few clean transitions).
    EyeSearch,
}

/// Lock quality (C20 `LockQuality`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FskLock {
    /// Seed method.
    pub seed_method: TimingSeedMethod,
    /// Rate after the LS refinement, Bd.
    pub seed_rate_bd: f64,
    /// Mean tracked rate, Bd.
    pub tracked_rate_bd: f64,
    /// RMS Gardner timing error on transitions, unit intervals.
    pub timing_rms_ui: f64,
    /// Eye opening at ±2σ, 0–1.
    pub eye_opening: f64,
    /// Distance between the two level means, Hz.
    pub level_separation_hz: f64,
    /// Pooled level standard deviation, Hz.
    pub noise_sigma_hz: f64,
    /// Timing slips against the seed lattice.
    pub slips: usize,
    /// Timing RMS < 0.15 UI and eye opening > 0.2. Lock metrics can look good on noise:
    /// confirm with a sync word or a CRC.
    pub locked: bool,
    /// `eye_opening · (1 − timing_rms_ui / 0.3)`, 0–1.
    pub lock_quality: f64,
}

/// Demodulated symbols of one burst.
#[derive(Clone, Debug, PartialEq)]
pub struct FskSymbols {
    /// Symbol rate requested, Bd.
    pub rate_bd: f64,
    /// Hard bits (1 = +deviation).
    pub bits: Vec<u8>,
    /// LLR-like soft values `2·A·(s − mid)/σ²` (positive = 1).
    pub soft: Vec<f32>,
    /// Snippet sample position of each symbol centre.
    pub positions: Vec<f64>,
    /// Source stream index of each symbol centre (the time map).
    pub source_index: Vec<f64>,
    /// Deviation at settled symbols, Hz.
    pub deviation_hz: Option<f64>,
    /// Level mid-point relative to the mixing frequency, Hz (residual CFO).
    pub residual_cfo_hz: f64,
    /// Mixing frequency relative to the snippet centre, Hz.
    pub cfo_applied_hz: f64,
    /// Per-symbol SNR `A²/σ²`, dB.
    pub symbol_snr_db: Option<f64>,
    /// Lock.
    pub lock: FskLock,
}

impl FskSymbols {
    /// Burst offset from the snippet centre, Hz.
    pub fn cfo_hz(&self) -> f64 {
        self.cfo_applied_hz + self.residual_cfo_hz
    }
}

/// Why a demodulation produced nothing.
#[derive(Clone, Debug, PartialEq)]
pub enum FskDemodError {
    /// Fewer than `min_sps` samples per symbol.
    TooFewSamplesPerSymbol(f64),
    /// Invalid rate or range.
    InvalidRequest(String),
    /// No samples above the power gate.
    NoSignal,
    /// Fewer than `min_symbols` symbols.
    TooShort(usize),
    /// Channel filter design failed.
    Design(DesignError),
}

impl std::fmt::Display for FskDemodError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for FskDemodError {}

/// The demodulator.
#[derive(Clone, Debug, Default)]
pub struct FskDemod {
    config: FskDemodConfig,
}

fn moving_avg(v: &[f64], len: usize) -> Vec<f64> {
    let n = v.len();
    if len <= 1 || n == 0 {
        return v.to_vec();
    }
    let mut prefix = Vec::with_capacity(n + 1);
    prefix.push(0.0);
    for &x in v {
        prefix.push(prefix.last().unwrap() + x);
    }
    let back = (len - 1) / 2;
    let fwd = len / 2;
    (0..n)
        .map(|i| {
            let lo = i.saturating_sub(back);
            let hi = (i + fwd + 1).min(n);
            (prefix[hi] - prefix[lo]) / (hi - lo) as f64
        })
        .collect()
}

/// Catmull-Rom interpolation of `v` at fractional position `t`.
fn interp(v: &[f64], t: f64) -> f64 {
    let n = v.len();
    if n == 0 {
        return 0.0;
    }
    let t = t.clamp(0.0, (n - 1) as f64);
    let i = t.floor() as usize;
    let f = t - i as f64;
    let at = |k: isize| v[(k.clamp(0, n as isize - 1)) as usize];
    let (p0, p1, p2, p3) = (
        at(i as isize - 1),
        at(i as isize),
        at(i as isize + 1),
        at(i as isize + 2),
    );
    p1 + 0.5
        * f
        * (p2 - p0 + f * (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3 + f * (3.0 * (p1 - p2) + p3 - p0)))
}

/// Two-means of `v`: (low mean, high mean, low σ, high σ).
fn kmeans2(v: &[f64]) -> (f64, f64, f64, f64) {
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let q = |p: f64| s[((s.len() - 1) as f64 * p) as usize];
    let (mut lo, mut hi) = (q(0.15), q(0.85));
    for _ in 0..30 {
        let mid = 0.5 * (lo + hi);
        let (mut sl, mut nl, mut sh, mut nh) = (0.0, 0usize, 0.0, 0usize);
        for &x in v {
            if x > mid {
                sh += x;
                nh += 1;
            } else {
                sl += x;
                nl += 1;
            }
        }
        let (nlo, nhi) = (
            if nl > 0 { sl / nl as f64 } else { lo },
            if nh > 0 { sh / nh as f64 } else { hi },
        );
        if (nlo - lo).abs() < 1e-9 && (nhi - hi).abs() < 1e-9 {
            break;
        }
        lo = nlo;
        hi = nhi;
    }
    let mid = 0.5 * (lo + hi);
    let sd = |pred: &dyn Fn(f64) -> bool, m: f64| {
        let xs: Vec<f64> = v.iter().copied().filter(|&x| pred(x)).collect();
        if xs.len() < 2 {
            0.0
        } else {
            (xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (xs.len() - 1) as f64).sqrt()
        }
    };
    let sl = sd(&|x| x <= mid, lo);
    let sh = sd(&|x| x > mid, hi);
    (lo, hi, sl, sh)
}

fn filter_same(x: &[Complex32], taps: &[f32]) -> Vec<Complex32> {
    let n = x.len();
    let d = taps.len() / 2;
    (0..n)
        .map(|i| {
            let mut acc = Complex32::new(0.0, 0.0);
            for (k, &t) in taps.iter().enumerate() {
                let j = i + k;
                if j >= d && j - d < n {
                    acc += x[j - d] * t;
                }
            }
            acc
        })
        .collect()
}

impl FskDemod {
    /// A demodulator with `config`.
    pub fn new(config: FskDemodConfig) -> Self {
        Self { config }
    }

    /// Settings.
    pub fn config(&self) -> &FskDemodConfig {
        &self.config
    }

    /// Demodulates one burst of `snip`. Steps: mix by `cfo_hz`; channel low-pass
    /// (passband deviation + Rs/2, stopband deviation + 1.1·Rs); discriminator; moving average
    /// over `integrate_fraction` symbols; on-signal gate from the pad noise; 2-means level
    /// mid-point removed (residual CFO); timing seed from the T-011 transition LS rate and a
    /// transition phase fit (or a 16-phase eye search); Gardner loop from the first to the last
    /// on-signal symbol; 2-means slicer, soft values, deviation at settled symbols.
    pub fn demodulate(
        &self,
        snip: &ChannelSnippet,
        req: &FskDemodRequest,
    ) -> Result<FskSymbols, FskDemodError> {
        let cfg = &self.config;
        let fs = snip.sample_rate_hz;
        let rate = req.rate_bd;
        if !(rate.is_finite() && rate > 0.0 && fs > 0.0) {
            return Err(FskDemodError::InvalidRequest(format!("rate {rate}")));
        }
        let sps = fs / rate;
        if sps < cfg.min_sps {
            return Err(FskDemodError::TooFewSamplesPerSymbol(sps));
        }
        let n = snip.samples.len();
        if n < 4 || req.range.start >= req.range.end || req.range.start >= n {
            return Err(FskDemodError::InvalidRequest("empty range".into()));
        }

        // Mix and filter.
        let w = -TAU * req.cfo_hz / fs;
        let mixed: Vec<Complex32> = snip
            .samples
            .iter()
            .enumerate()
            .map(|(k, s)| {
                let ph = (w * k as f64) % TAU;
                s * Complex32::new(ph.cos() as f32, ph.sin() as f32)
            })
            .collect();
        let dev = req
            .deviation_hz
            .filter(|d| d.is_finite() && *d > 0.0)
            .unwrap_or_else(|| {
                req.bandwidth_hz
                    .map_or(0.5 * rate, |b| (0.5 * b - 0.5 * rate).max(0.25 * rate))
            });
        let pass = dev + 0.5 * rate;
        let stop = dev + 1.1 * rate;
        let (xf, taps_len) = if stop < 0.49 * fs {
            let taps = lowpass_taps(fs, pass, stop, cfg.filter_stopband_db)
                .map_err(FskDemodError::Design)?;
            (filter_same(&mixed, &taps), taps.len())
        } else {
            (mixed, 0)
        };

        // Discriminator, integrate, power.
        let mut fi = vec![0.0f64; n];
        for k in 1..n {
            let z = xf[k] * xf[k - 1].conj();
            fi[k] = f64::from(z.im).atan2(f64::from(z.re)) * fs / TAU;
        }
        fi[0] = fi[1];
        let l_ma = ((cfg.integrate_fraction * sps).round() as usize).max(1);
        let y = moving_avg(&fi, l_ma);
        let pw: Vec<f64> = xf.iter().map(|z| f64::from(z.norm_sqr())).collect();
        let pa = moving_avg(&pw, ((2.0 * sps).round() as usize).max(1));

        // Noise from the pads (skipping the filter transient), else a low percentile.
        let guard = taps_len / 2 + (2.0 * sps) as usize;
        let edge = (cfg.edge_symbols * sps) as usize;
        let pre = guard..req.range.start.saturating_sub(edge);
        let post = (req.range.end + edge).min(n)..n.saturating_sub(guard);
        let mean = |r: Range<usize>| {
            (r.end > r.start + 32).then(|| pw[r.clone()].iter().sum::<f64>() / r.len() as f64)
        };
        let noise = match (mean(pre.clone()), mean(post.clone())) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => {
                let mut s = pa.clone();
                s.sort_by(f64::total_cmp);
                s[s.len() / 10]
            }
        };
        let win = req.range.start.saturating_sub(edge)..(req.range.end + edge).min(n);
        let on: Vec<bool> = (0..n)
            .map(|k| win.contains(&k) && pa[k] > cfg.power_gate * noise)
            .collect();
        let on_first = on.iter().position(|&b| b).ok_or(FskDemodError::NoSignal)?;
        let on_last = on.iter().rposition(|&b| b).ok_or(FskDemodError::NoSignal)?;
        if ((on_last - on_first) as f64) < cfg.min_symbols as f64 * sps {
            return Err(FskDemodError::TooShort(
                ((on_last - on_first) as f64 / sps) as usize,
            ));
        }

        // Level mid-point (residual CFO).
        let stride = ((sps / 4.0) as usize).max(1);
        let vals: Vec<f64> = (on_first..=on_last)
            .step_by(stride)
            .filter(|&k| on[k])
            .map(|k| y[k])
            .collect();
        let (lo, hi, _, _) = kmeans2(&vals);
        let mid = 0.5 * (lo + hi);
        let amp = (0.5 * (hi - lo)).max(1e-9);
        let yc: Vec<f64> = y.iter().map(|v| v - mid).collect();

        // Timing seed: T-011 LS rate, then the transition phase.
        let guards = LsGuards {
            min_symbols: cfg.min_symbols as u64,
            ..Default::default()
        };
        let fit = rate_transitions_ls(&yc, fs, 0.0, sps, Some(&on), &guards);
        let mut period = match fit.rate_bd {
            Some(r) if fit.ok && (r / rate - 1.0).abs() < 0.01 => fs / r,
            _ => sps,
        };
        let tr: Vec<f64> = transitions(&yc, 0.0)
            .into_iter()
            .filter(|&t| on[(t as usize).min(n - 1)])
            .collect();
        let mut seed_method = TimingSeedMethod::EyeSearch;
        let mut centre = None;
        if tr.len() >= 8 {
            let (s, c) = tr.iter().fold((0.0, 0.0), |(s, c), &t| {
                let ph = TAU * (t / period).fract();
                (s + ph.sin(), c + ph.cos())
            });
            let mut a = (s.atan2(c) / TAU).rem_euclid(1.0) * period;
            let mut kept = tr.len();
            let mut rms = f64::INFINITY;
            for _ in 0..3 {
                let rows: Vec<(f64, f64)> = tr
                    .iter()
                    .map(|&t| ((t - a) / period).round())
                    .zip(tr.iter().copied())
                    .filter(|&(k, t)| (t - a - k * period).abs() < 0.3 * period)
                    .collect();
                kept = rows.len();
                if kept < 8 {
                    break;
                }
                let m = kept as f64;
                let (sk, st) = rows.iter().fold((0.0, 0.0), |(x, y), r| (x + r.0, y + r.1));
                let (mk, mt) = (sk / m, st / m);
                let (sxy, sxx) = rows.iter().fold((0.0, 0.0), |(p, q), r| {
                    (p + (r.0 - mk) * (r.1 - mt), q + (r.0 - mk).powi(2))
                });
                if sxx > 0.0 {
                    let slope = sxy / sxx;
                    if (slope / period - 1.0).abs() < 0.01 {
                        period = slope;
                    }
                }
                a = mt - period * mk;
                rms = (rows
                    .iter()
                    .map(|r| (r.1 - a - r.0 * period).powi(2))
                    .sum::<f64>()
                    / m)
                    .sqrt();
            }
            if kept as f64 >= 0.7 * tr.len() as f64 && rms < 0.2 * period {
                seed_method = TimingSeedMethod::TransitionLs;
                centre = Some(a + 0.5 * period);
            }
        }
        let centre = centre.unwrap_or_else(|| {
            (0..16)
                .map(|i| {
                    let ph = on_first as f64 + period * i as f64 / 16.0;
                    let (mut acc, mut cnt) = (0.0, 0usize);
                    let mut t = ph;
                    while t <= on_last as f64 {
                        if on[t as usize] {
                            acc += interp(&yc, t).abs();
                            cnt += 1;
                        }
                        t += period;
                    }
                    (ph, if cnt > 0 { acc / cnt as f64 } else { 0.0 })
                })
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .map_or(on_first as f64, |p| p.0)
        });
        let seed_period = period;

        // Gardner loop.
        let zeta = cfg.loop_damping;
        let theta = cfg.loop_bandwidth / (zeta + 1.0 / (4.0 * zeta));
        let den = 1.0 + 2.0 * zeta * theta + theta * theta;
        let (g1, g2) = (4.0 * zeta * theta / den, 4.0 * theta * theta / den);
        let l_eff = l_ma as f64;
        let k0 = ((on_first as f64 - centre) / period).ceil();
        let mut t = centre + k0 * period;
        while t - 0.5 * period < 1.0 {
            t += period;
        }
        let t_first = t;
        let mut prev: Option<f64> = None;
        let (mut positions, mut values, mut errs) = (Vec::new(), Vec::new(), Vec::new());
        let mut slips = 0usize;
        let mut last_slot = 0i64;
        let mut k = 0usize;
        while t <= on_last as f64 + 0.5 * period && t < (n - 2) as f64 {
            let yk = interp(&yc, t);
            let mut e = 0.0;
            if let Some(yp) = prev {
                let diff = yk - yp;
                if diff.abs() > amp {
                    let ymid = interp(&yc, t - 0.5 * period);
                    e = (diff.signum() * ymid * l_eff / (2.0 * amp))
                        .clamp(-0.25 * period, 0.25 * period);
                    errs.push(e);
                }
            }
            positions.push(t);
            values.push(yk);
            prev = Some(yk);
            let drift = t - (t_first + k as f64 * seed_period);
            let slot = (drift / seed_period).round() as i64;
            if slot != last_slot {
                slips += 1;
                last_slot = slot;
            }
            t += period - g1 * e;
            period = (period - g2 * e).clamp(0.98 * seed_period, 1.02 * seed_period);
            k += 1;
        }
        // Keep first..last on-signal symbol.
        let first = positions
            .iter()
            .position(|&p| on[(p as usize).min(n - 1)])
            .unwrap_or(0);
        let last = positions
            .iter()
            .rposition(|&p| on[(p as usize).min(n - 1)])
            .unwrap_or(0);
        if last < first + cfg.min_symbols {
            return Err(FskDemodError::TooShort(last.saturating_sub(first)));
        }
        let positions = positions[first..=last].to_vec();
        let values = values[first..=last].to_vec();

        // Slicer and soft values.
        let (lo2, hi2, sl, sh) = kmeans2(&values);
        let mid2 = 0.5 * (lo2 + hi2);
        let a2 = (0.5 * (hi2 - lo2)).max(1e-9);
        let var = (0.5 * (sl * sl + sh * sh)).max(1e-12);
        let bits: Vec<u8> = values.iter().map(|&v| u8::from(v > mid2)).collect();
        let soft: Vec<f32> = values
            .iter()
            .map(|&v| (2.0 * a2 * (v - mid2) / var) as f32)
            .collect();
        let settled: Vec<f64> = (1..values.len().saturating_sub(1))
            .filter(|&i| bits[i - 1] == bits[i] && bits[i] == bits[i + 1])
            .map(|i| (values[i] - mid2).abs())
            .collect();
        let deviation_hz = (settled.len() >= 8).then(|| {
            let mut s = settled.clone();
            s.sort_by(f64::total_cmp);
            s[s.len() / 2]
        });
        // Eye at ±2σ over settled symbols (GFSK ISI on isolated bits is not noise).
        let settled_sd = |high: bool| {
            let xs: Vec<f64> = (1..values.len().saturating_sub(1))
                .filter(|&i| bits[i - 1] == bits[i] && bits[i] == bits[i + 1])
                .filter(|&i| (bits[i] == 1) == high)
                .map(|i| values[i])
                .collect();
            if xs.len() < 4 {
                return if high { sh } else { sl };
            }
            let m = xs.iter().sum::<f64>() / xs.len() as f64;
            (xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (xs.len() - 1) as f64).sqrt()
        };
        let eye = (((hi2 - 2.0 * settled_sd(true)) - (lo2 + 2.0 * settled_sd(false)))
            / (hi2 - lo2).max(1e-9))
        .clamp(0.0, 1.0);
        let timing_rms_ui = if errs.len() >= 4 {
            (errs.iter().map(|e| e * e).sum::<f64>() / errs.len() as f64).sqrt() / seed_period
        } else {
            0.5
        };
        let span = positions.last().unwrap() - positions[0];
        let tracked = if positions.len() > 1 {
            fs * (positions.len() - 1) as f64 / span
        } else {
            rate
        };
        let lock_quality = eye * (1.0 - (timing_rms_ui / 0.3).min(1.0));
        let source_index = positions
            .iter()
            .map(|&p| snip.time.source_index + p * snip.time.source_per_output)
            .collect();
        Ok(FskSymbols {
            rate_bd: rate,
            bits,
            soft,
            positions,
            source_index,
            deviation_hz,
            residual_cfo_hz: mid + mid2,
            cfo_applied_hz: req.cfo_hz,
            symbol_snr_db: Some(10.0 * (a2 * a2 / var).log10()),
            lock: FskLock {
                seed_method,
                seed_rate_bd: fs / seed_period,
                tracked_rate_bd: tracked,
                timing_rms_ui,
                eye_opening: eye,
                level_separation_hz: hi2 - lo2,
                noise_sigma_hz: var.sqrt(),
                slips,
                locked: timing_rms_ui < 0.15 && eye > 0.2,
                lock_quality,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(
            moving_avg(&[1.0, 2.0, 3.0, 4.0], 2),
            vec![1.5, 2.5, 3.5, 4.0]
        );
        let v: Vec<f64> = (0..10).map(|i| i as f64).collect();
        assert!((interp(&v, 3.25) - 3.25).abs() < 1e-12);
        let (lo, hi, _, _) = kmeans2(&[-1.0, -1.1, -0.9, 1.0, 1.2, 0.8]);
        assert!((lo + 1.0).abs() < 1e-9 && (hi - 1.0).abs() < 1e-9);
    }
}
