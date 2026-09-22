//! 4-level FSK (C4FM) symbol recovery for trunking control channels (T-267, C23).
//!
//! [`super::demod::FskDemod`] slices to **two** levels (a 2-means mid-point), so it cannot read a
//! C4FM control channel. This is the 4-level path: quadrature discriminator → integrate → level
//! centring → symbol timing by phase search → 4-level slicer → dibits.
//!
//! It deliberately stops at dibits. Deciding whether those dibits are a control channel is
//! [`hk_detect::trunk`]'s job, and keeping the two apart is what lets the sync false-alarm rate
//! be measured over millions of symbols without any DSP in the loop.
//!
//! **Lock quality is not confirmation.** [`super::demod::FskLock`] already carries the warning
//! that "lock metrics can look good on noise: confirm with a sync word or a CRC", and that is
//! doubly true here: a 4-level slicer applied to noise produces a perfectly well-formed dibit
//! stream. Nothing in this module reports a signal as a control channel, and
//! [`C4fmSymbols::level_margin`] exists for diagnostics, never as a gate.

use hk_dsp::DesignError;
use num_complex::Complex32;
use std::f64::consts::TAU;

use crate::dsp::lowpass_taps;
use crate::fsk::demod::filter_same;

/// Demodulator id and version.
pub const C4FM_DEMOD_VERSION: &str = "hk-demod/c23-c4fm@0.1.0";

/// P25 Phase 1 / DMR symbol rate, Bd (docs/04 §7.2: 4800 sym/s = 9600 bit/s).
pub const C4FM_SYMBOL_RATE_BD: f64 = 4800.0;
/// Outer C4FM deviation, Hz (docs/04 §7.2: peaks at ±600 and ±1800 Hz).
pub const C4FM_OUTER_DEVIATION_HZ: f64 = 1800.0;
/// Inner C4FM deviation, Hz.
pub const C4FM_INNER_DEVIATION_HZ: f64 = 600.0;

/// Ideal levels normalised to the outer deviation, in dibit order 0..=3.
///
/// C4FM dibit mapping (docs/04 §7.2): `01` → +1800, `00` → +600, `10` → −600, `11` → −1800, so
/// index 0 (`00`) is +1/3, index 1 (`01`) is +1, index 2 (`10`) is −1/3, index 3 (`11`) is −1.
const IDEAL: [f64; 4] = [1.0 / 3.0, 1.0, -1.0 / 3.0, -1.0];

/// Settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct C4fmConfig {
    /// Symbol rate, Bd.
    pub symbol_rate_bd: f64,
    /// Integrate-and-dump window as a fraction of a symbol.
    pub integrate_fraction: f64,
    /// Channel filter stopband, dB.
    pub filter_stopband_db: f64,
    /// Symbol-timing phases searched over one symbol period.
    pub timing_phases: usize,
    /// Fewest samples per symbol.
    pub min_sps: f64,
    /// Fewest symbols to return.
    pub min_symbols: usize,
}

impl Default for C4fmConfig {
    fn default() -> Self {
        Self {
            symbol_rate_bd: C4FM_SYMBOL_RATE_BD,
            integrate_fraction: 0.5,
            filter_stopband_db: 40.0,
            timing_phases: 16,
            min_sps: 3.0,
            min_symbols: 64,
        }
    }
}

/// Recovered symbols.
#[derive(Clone, Debug, PartialEq)]
pub struct C4fmSymbols {
    /// Dibits, values 0–3, in C4FM mapping order.
    pub dibits: Vec<u8>,
    /// Symbol rate used, Bd.
    pub rate_bd: f64,
    /// Estimated outer deviation, Hz (nominally 1800).
    pub outer_deviation_hz: f64,
    /// Level centre removed before slicing, Hz — the residual carrier offset.
    pub residual_cfo_hz: f64,
    /// Mean distance from the sliced ideal level, 0 (perfect) to 1 (meaningless).
    ///
    /// Diagnostics only. A 4-level slicer on pure noise still returns a full dibit stream with a
    /// plausible margin, which is precisely why confirmation is sync + CRC and not this number.
    pub level_margin: f64,
    /// Symbol-timing phase chosen, in samples.
    pub timing_phase: f64,
}

/// Why a demodulation produced nothing.
#[derive(Clone, Debug, PartialEq)]
pub enum C4fmError {
    /// Sample rate too low for the symbol rate.
    TooFewSamplesPerSymbol(f64),
    /// Fewer symbols than [`C4fmConfig::min_symbols`].
    TooFewSymbols(usize),
    /// The request made no sense.
    InvalidRequest(String),
    /// Channel filter design failed.
    Design(DesignError),
}

/// The 4-level demodulator.
#[derive(Clone, Debug, Default)]
pub struct C4fmDemod {
    config: C4fmConfig,
}

impl C4fmDemod {
    /// A demodulator with `config`.
    pub fn new(config: C4fmConfig) -> Self {
        Self { config }
    }

    /// Settings.
    pub fn config(&self) -> &C4fmConfig {
        &self.config
    }

    /// Recovers dibits from baseband `samples` at `sample_rate_hz`, mixing by `cfo_hz` first.
    pub fn demodulate(
        &self,
        samples: &[Complex32],
        sample_rate_hz: f64,
        cfo_hz: f64,
    ) -> Result<C4fmSymbols, C4fmError> {
        let cfg = &self.config;
        let fs = sample_rate_hz;
        let rate = cfg.symbol_rate_bd;
        if !(fs > 0.0 && rate > 0.0) {
            return Err(C4fmError::InvalidRequest(format!("fs {fs} rate {rate}")));
        }
        let sps = fs / rate;
        if sps < cfg.min_sps {
            return Err(C4fmError::TooFewSamplesPerSymbol(sps));
        }
        let n = samples.len();
        if n < 8 {
            return Err(C4fmError::InvalidRequest("too few samples".into()));
        }

        // Mix to baseband.
        let w = -TAU * cfo_hz / fs;
        let mixed: Vec<Complex32> = samples
            .iter()
            .enumerate()
            .map(|(k, s)| {
                let ph = (w * k as f64) % TAU;
                s * Complex32::new(ph.cos() as f32, ph.sin() as f32)
            })
            .collect();

        // Channel filter: pass the outer deviation plus half the symbol rate, stop above the
        // first spectral null, so an adjacent 12.5 kHz channel does not leak in.
        let pass = C4FM_OUTER_DEVIATION_HZ + 0.5 * rate;
        let stop = C4FM_OUTER_DEVIATION_HZ + 1.1 * rate;
        let xf = if stop < 0.49 * fs {
            let taps =
                lowpass_taps(fs, pass, stop, cfg.filter_stopband_db).map_err(C4fmError::Design)?;
            filter_same(&mixed, &taps)
        } else {
            mixed
        };

        // Quadrature discriminator, in Hz.
        let mut fi = vec![0.0f64; n];
        for k in 1..n {
            let z = xf[k] * xf[k - 1].conj();
            fi[k] = f64::from(z.im).atan2(f64::from(z.re)) * fs / TAU;
        }
        fi[0] = fi[1];

        // Integrate and dump: a boxcar over a fraction of a symbol keeps symbol centres clean.
        let w_len = ((cfg.integrate_fraction * sps).round() as usize).max(1);
        let y = boxcar(&fi, w_len);

        // Level centre. The four C4FM levels are symmetric, and both the frame sync and random
        // traffic are balanced across them, so the mean is the residual carrier offset.
        let centre = y.iter().sum::<f64>() / y.len() as f64;
        let yc: Vec<f64> = y.iter().map(|v| v - centre).collect();

        // Outer deviation from a high quantile of |y|: with roughly balanced symbols the outer
        // pair is about half the population, so the 80th percentile sits on it.
        let outer = quantile_abs(&yc, 0.80).max(1e-9);

        // Symbol timing: search phases over one symbol, scoring each by how close its sampled
        // values land to the nearest ideal level.
        let n_sym = ((n as f64 - sps) / sps).floor() as usize;
        if n_sym < cfg.min_symbols {
            return Err(C4fmError::TooFewSymbols(n_sym));
        }
        let phases = cfg.timing_phases.max(1);
        let mut best = (f64::INFINITY, 0.0f64);
        for p in 0..phases {
            let ph = sps * p as f64 / phases as f64;
            let mut acc = 0.0;
            let mut cnt = 0usize;
            let mut k = 0usize;
            while k < n_sym {
                let t = ph + sps * k as f64;
                if t >= (n - 1) as f64 {
                    break;
                }
                let v = interp(&yc, t) / outer;
                acc += nearest_ideal(v).1;
                cnt += 1;
                k += 1;
            }
            if cnt > 0 {
                let score = acc / cnt as f64;
                if score < best.0 {
                    best = (score, ph);
                }
            }
        }
        let phase = best.1;

        // Slice at the chosen phase.
        let mut dibits = Vec::with_capacity(n_sym);
        let mut dist = 0.0;
        for k in 0..n_sym {
            let t = phase + sps * k as f64;
            if t >= (n - 1) as f64 {
                break;
            }
            let v = interp(&yc, t) / outer;
            let (d, e) = nearest_ideal(v);
            dibits.push(d);
            dist += e;
        }
        if dibits.len() < cfg.min_symbols {
            return Err(C4fmError::TooFewSymbols(dibits.len()));
        }
        let level_margin = dist / dibits.len() as f64;

        Ok(C4fmSymbols {
            dibits,
            rate_bd: rate,
            outer_deviation_hz: outer,
            residual_cfo_hz: centre,
            level_margin,
            timing_phase: phase,
        })
    }
}

/// The dibit whose ideal level is nearest `v`, and the distance to it.
fn nearest_ideal(v: f64) -> (u8, f64) {
    let mut best = (0u8, f64::INFINITY);
    for (i, &lvl) in IDEAL.iter().enumerate() {
        let d = (v - lvl).abs();
        if d < best.1 {
            best = (i as u8, d);
        }
    }
    best
}

/// Centred moving average of `w` samples.
pub(crate) fn boxcar(x: &[f64], w: usize) -> Vec<f64> {
    if w <= 1 {
        return x.to_vec();
    }
    let n = x.len();
    let mut pre = vec![0.0; n + 1];
    for i in 0..n {
        pre[i + 1] = pre[i] + x[i];
    }
    let half = w / 2;
    (0..n)
        .map(|i| {
            let lo = i.saturating_sub(half);
            let hi = (i + w - half).min(n);
            (pre[hi] - pre[lo]) / (hi - lo) as f64
        })
        .collect()
}

/// Linear interpolation at fractional index `t`.
pub(crate) fn interp(x: &[f64], t: f64) -> f64 {
    let i = t.floor() as usize;
    let f = t - i as f64;
    if i + 1 >= x.len() {
        return x[x.len() - 1];
    }
    x[i] * (1.0 - f) + x[i + 1] * f
}

/// The `q` quantile of `|x|`.
pub(crate) fn quantile_abs(x: &[f64], q: f64) -> f64 {
    let mut v: Vec<f64> = x.iter().map(|a| a.abs()).collect();
    v.sort_by(f64::total_cmp);
    if v.is_empty() {
        return 0.0;
    }
    v[(((v.len() - 1) as f64) * q) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic uniform source; no RNG dependency in this crate.
    struct SplitMix(u64);
    impl SplitMix {
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn unit(&mut self) -> f64 {
            (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
        }
        /// Box-Muller normal.
        fn normal(&mut self) -> f64 {
            let u1 = self.unit().max(1e-12);
            let u2 = self.unit();
            (-2.0 * u1.ln()).sqrt() * (TAU * u2).cos()
        }
        fn dibits(&mut self, n: usize) -> Vec<u8> {
            (0..n).map(|_| (self.next_u64() & 3) as u8).collect()
        }
    }

    /// Deviation, Hz, for a dibit — the same map the generator uses.
    fn deviation(d: u8) -> f64 {
        IDEAL[d as usize] * C4FM_OUTER_DEVIATION_HZ
    }

    /// Continuous-phase C4FM for `dibits` at `fs`, optionally with noise at `snr_db`.
    fn c4fm(dibits: &[u8], fs: f64, snr_db: Option<f64>, seed: u64) -> Vec<Complex32> {
        let rate = C4FM_SYMBOL_RATE_BD;
        let sps = fs / rate;
        let n = (dibits.len() as f64 * sps) as usize;
        let mut freq = Vec::with_capacity(n);
        for k in 0..n {
            let idx = ((k as f64 / sps) as usize).min(dibits.len() - 1);
            freq.push(deviation(dibits[idx]));
        }
        // One-symbol-wide raised cosine, matching the fixture generator's shaping. Widening this
        // to two symbols closes the eye at the symbol centres and the stream stops being
        // recoverable at all — see the note in `py/hkpy/synth/trunking.py`.
        let w = ((sps / 2.0) as usize).max(1);
        let h: Vec<f64> = (0..=2 * w)
            .map(|i| {
                let k = i as f64 - w as f64;
                0.5 * (1.0 + (std::f64::consts::PI * k / w.max(1) as f64).cos())
            })
            .collect();
        let hs: f64 = h.iter().sum();
        let shaped: Vec<f64> = (0..n)
            .map(|i| {
                let mut acc = 0.0;
                for (j, &t) in h.iter().enumerate() {
                    let idx = i as i64 + j as i64 - w as i64;
                    if idx >= 0 && (idx as usize) < n {
                        acc += freq[idx as usize] * t;
                    }
                }
                acc / hs
            })
            .collect();
        let mut ph = 0.0;
        let mut rng = SplitMix(seed);
        let sigma = snr_db.map(|s| (10f64.powf(-s / 10.0) / 2.0).sqrt());
        (0..n)
            .map(|k| {
                ph += TAU * shaped[k] / fs;
                let mut s = Complex32::new(ph.cos() as f32, ph.sin() as f32);
                if let Some(sg) = sigma {
                    s += Complex32::new((sg * rng.normal()) as f32, (sg * rng.normal()) as f32);
                }
                s
            })
            .collect()
    }

    fn symbol_error_rate(truth: &[u8], got: &[u8]) -> f64 {
        // The demodulator's first symbol may sit one position into the stream; align on the
        // offset that fits best over a short search, then score the overlap.
        let mut best = 1.0f64;
        for off in 0..4usize {
            if got.len() <= off {
                break;
            }
            let m = truth.len().min(got.len() - off);
            if m < 32 {
                continue;
            }
            let bad = (0..m).filter(|&i| truth[i] != got[off + i]).count();
            best = best.min(bad as f64 / m as f64);
        }
        best
    }

    #[test]
    fn clean_c4fm_symbols_are_recovered_exactly() {
        let fs = 48_000.0;
        let truth = SplitMix(11).dibits(600);
        let x = c4fm(&truth, fs, None, 1);
        let got = C4fmDemod::default().demodulate(&x, fs, 0.0).expect("demod");
        let ser = symbol_error_rate(&truth, &got.dibits);
        eprintln!(
            "[T-267] clean: ser {ser:.4} outer {:.0} Hz margin {:.3}",
            got.outer_deviation_hz, got.level_margin
        );
        assert!(ser < 0.01, "clean symbol error rate {ser}");
        // The estimate tracks the deviation *as observed*, which sits below the nominal
        // ±1800 Hz: the transmitter's raised-cosine shaping and the receiver's integrate window
        // both compress the excursion a symbol actually reaches. That compression is why the
        // slicer normalises by this estimate instead of assuming the nominal levels — the ideal
        // levels are ratios (±1, ±1/3), so a compressed scale slices just as well.
        assert!(
            (900.0..=1.2 * C4FM_OUTER_DEVIATION_HZ).contains(&got.outer_deviation_hz),
            "outer deviation {} outside the plausible post-shaping range",
            got.outer_deviation_hz
        );
    }

    #[test]
    fn symbols_survive_a_realistic_snr() {
        let fs = 48_000.0;
        let truth = SplitMix(12).dibits(800);
        let x = c4fm(&truth, fs, Some(20.0), 2);
        let got = C4fmDemod::default().demodulate(&x, fs, 0.0).expect("demod");
        let ser = symbol_error_rate(&truth, &got.dibits);
        eprintln!("[T-267] 20 dB SNR: ser {ser:.4}");
        // A frame sync tolerating 2 of 24 symbol errors needs a symbol error rate well under
        // 8 %; 3 % leaves the sync search comfortable without being a tuned-to-the-run number.
        assert!(ser < 0.03, "symbol error rate at 20 dB SNR {ser}");
    }

    #[test]
    fn a_carrier_offset_is_removed_rather_than_shifting_every_level() {
        let fs = 48_000.0;
        let truth = SplitMix(13).dibits(600);
        let x = c4fm(&truth, fs, None, 3);
        // Apply a 300 Hz offset and let the demodulator find it.
        let off = 300.0;
        let shifted: Vec<Complex32> = x
            .iter()
            .enumerate()
            .map(|(k, s)| {
                let ph = TAU * off * k as f64 / fs;
                s * Complex32::new(ph.cos() as f32, ph.sin() as f32)
            })
            .collect();
        let got = C4fmDemod::default()
            .demodulate(&shifted, fs, 0.0)
            .expect("demod");
        assert!(
            (got.residual_cfo_hz - off).abs() < 120.0,
            "residual cfo {} for a {off} Hz offset",
            got.residual_cfo_hz
        );
        assert!(symbol_error_rate(&truth, &got.dibits) < 0.02);
    }

    #[test]
    fn noise_still_produces_a_well_formed_dibit_stream() {
        // The point of the confirmation rule: a 4-level slicer on noise yields a full stream
        // with a plausible margin. Nothing here may be read as a control channel.
        let fs = 48_000.0;
        let mut rng = SplitMix(14);
        let x: Vec<Complex32> = (0..40_000)
            .map(|_| Complex32::new(rng.normal() as f32, rng.normal() as f32))
            .collect();
        let got = C4fmDemod::default().demodulate(&x, fs, 0.0).expect("demod");
        eprintln!(
            "[T-267] noise: {} dibits, margin {:.3}",
            got.dibits.len(),
            got.level_margin
        );
        assert!(got.dibits.len() > 1000, "noise yields symbols too");
        assert!(got.dibits.iter().all(|&d| d < 4));
    }

    #[test]
    fn too_low_a_sample_rate_is_an_error_not_a_guess() {
        let x = vec![Complex32::new(1.0, 0.0); 1000];
        assert!(matches!(
            C4fmDemod::default().demodulate(&x, 9_000.0, 0.0),
            Err(C4fmError::TooFewSamplesPerSymbol(_))
        ));
    }
}
