//! **How many levels, at what rate, at what deviation — measured from the signal** (T-546, C13/C14).
//!
//! [`super::c4fm`] *demodulates* a four-level FM channel at a symbol rate it is **told**. This
//! module *measures* one: it is given baseband and a sample rate and nothing else, and it answers
//! with a symbol rate, a level count and an outer deviation, each with the evidence behind it.
//!
//! # Why this exists, and why it is not the demodulator
//!
//! T-545 found the parameter gap **inverted**: a confirmed digital control channel carried
//! `estimated_params: null` while bursty analogue FM neighbours were labelled `"2fsk"` at
//! 2390 Hz deviation, and no code path in the repository could produce the answer `4`. A
//! demodulator that is configured with 4800 Bd and reports 4800 Bd back has measured nothing —
//! `C4fmSymbols::rate_bd` is the *setting*, which is why it is not the input to
//! `EstimatedParams`. Everything here is measured, and what cannot be measured is reported as
//! [`Levels::Indeterminate`] rather than defaulted (docs/api.md: `estimated_params` is measured
//! values only, never a fabricated default).
//!
//! # How each figure is measured
//!
//! 1. **Symbol rate — a cyclostationary clock line.** The frequency discriminator of a
//!    symbol-clocked FM signal changes level only at symbol boundaries, so the squared first
//!    difference of the discriminator is a pulse train at the baud rate and its spectrum carries
//!    a **line** there. The line is found in a band bounded only by the sample rate, parabolically
//!    interpolated, and scored against the *median* of that band — a rate is reported only when
//!    the line stands [`MIN_CLOCK_BITS`] above it. A spectral line is also far sharper than an eye
//!    sweep: over a 0.5 s window the line's own width is ~2 Hz at 4800 Bd, where an eye-opening
//!    objective would have to be swept at that same resolution to find its peak at all.
//! 2. **Level count — is the inner pair populated, and is there a valley between the levels?**
//!    Symbols are sampled at the timing phase that best opens the eye and normalised by the outer
//!    level. The first question separates two levels from four: **what fraction of symbols sits
//!    near the inner level rather than the outer one?** Four-level FM puts about half its symbols
//!    there; two-level FM puts none. (Residual-to-the-nearest-ideal cannot answer it, because a
//!    four-level ideal set fits two-level data perfectly — the inner level simply goes unused.)
//!    The second question separates a discrete alphabet from a *continuous* one, which populates
//!    the inner pair just as well: see [`MAX_VALLEY_RATIO`].
//! 3. **Outer deviation** is the 80th percentile of the |discriminator| at symbol centres, in Hz,
//!    which lands on the outer pair for both a two-level and a four-level alphabet.
//! 4. **The filter is then re-set from the measurements and the levels re-read** — the closed
//!    loop the product asks for ("tune from the processed output"). The caller's channel is
//!    deliberately wider than the modulation, so a second pass at the measured Carson bandwidth
//!    (outer deviation + half a symbol rate) clears the noise that would otherwise sit between
//!    the levels and close the eye.
//!
//! # What it deliberately refuses to do
//!
//! It never says "C4FM", "P25" or "2-FSK". It says *four levels at 4800 Bd with ±1800 Hz outer
//! deviation*, and naming that is somebody else's job — the same separation
//! [`super::c4fm`] keeps between dibits and control channels. An emission with no clock line, or
//! with a level population matching neither hypothesis, is [`Levels::Indeterminate`]: **an
//! analogue FM burst has no symbol alphabet at all, and a confident two-level answer for one is
//! worse than no answer** (the open-set stance; T-614 owns the estimator that does exactly that).
//!
//! **One limit, measured and recorded rather than hidden.** A *tone*-modulated analogue carrier is
//! genuinely periodic, and sampling its sinusoidal discriminator at twice the tone frequency lands
//! every sample near an extreme — so it can still read as two levels. The valley test cannot
//! separate that case, because the values really do sit on two levels. The **four**-level answer
//! never fires on it (the test `a_pure_tone_is_never_called_four_level_though_it_can_still_look_\
//! two_level` pins that), and voice-carrying analogue FM — the case that actually occurs on the
//! air, and the one T-545 found mislabelled — abstains correctly. The *burst* estimator that
//! T-545 caught labelling analogue FM `2fsk` closes the tone case on its own path (T-614): a
//! sliced tone's bits repeat, and [`super::receiver::periodic_bits`] vetoes the alphabet claim.

use hk_dsp::{CpuFft, FftBackend};
use num_complex::Complex32;
use std::f64::consts::TAU;

use crate::dsp::lowpass_taps;
use crate::fsk::demod::filter_same;

use super::c4fm::{boxcar, interp, quantile_abs};

/// Measurement id and version, recorded as `demod_version` on what this produces.
pub const STRUCTURE_VERSION: &str = "hk-demod/fm-structure@0.1.0";

/// Least significance, in bits, a clock line must stand above its band's median to be reported.
///
/// **A priori.** The statistic is a peak-to-median ratio over a few hundred independent bins; for
/// structureless input that ratio's distribution has essentially no mass past 8× (3 bits), and
/// every symbol-clocked emission this is aimed at exceeds 30×. 3 bits is the floor because an
/// unreported rate costs an estimate, while a *wrong* one is inherited by every explanation built
/// on it.
pub const MIN_CLOCK_BITS: f64 = 3.0;

/// Fraction of symbols that must sit on the inner pair before four levels are claimed.
///
/// A balanced four-level alphabet puts **half** its symbols there; a two-level alphabet puts
/// **none**. The band `[0.20, 0.80]` therefore admits an alphabet that is substantially unbalanced
/// (a control channel's idle pattern is not random) while excluding both degenerate answers, and
/// nothing between 0.10 and 0.20 is claimed either way.
pub const INNER_FRACTION_FOUR: [f64; 2] = [0.20, 0.80];

/// Most symbols that may sit on the inner pair and still be called two-level.
pub const INNER_FRACTION_TWO: f64 = 0.10;

/// Largest **valley ratio** that still counts as a discrete four-level alphabet.
///
/// **A priori, and the difference between a symbol alphabet and a continuous one.** The inner
/// fraction alone cannot tell them apart: a *continuous*-valued discriminator — analogue FM
/// carrying voice — spreads over the deviation range and therefore puts about half its samples
/// "on the inner pair" exactly as a balanced four-level alphabet does. What separates them is
/// that a four-level alphabet leaves a **valley** between its levels and a continuous one does
/// not.
///
/// The test is three equal, adjacent windows of width `2/9` over the normalised `|v|`, centred on
/// the inner level (`1/3`), the midpoint between the levels (`2/3`) and the outer level (`1`) —
/// they tile `[2/9, 10/9]` exactly, with no gap and no overlap, so no tuning constant chooses
/// them. The ratio is the midpoint window's share against the mean of the two level windows:
///
/// - a **uniform** spread puts equal mass in all three, giving **1.0**;
/// - a four-level alphabet puts almost nothing at the midpoint, giving **~0**;
/// - and a real, half-open eye — pulse-shaped C4FM over the air, where the mean distance to a
///   level is a quarter of the level spacing — still sits well under 0.5, because the statistic
///   asks about the *shape* of the distribution rather than its width.
///
/// Measured on the two populations that have to be told apart
/// (`the_valley_threshold_sits_between_the_two_populations_that_have_to_be_told_apart`): a clean
/// four-level alphabet **0.00**, a clocked *continuous*-valued modulation at the same rate and
/// deviation **0.96**. Their inner fractions are 0.49 and 0.52 — indistinguishable, which is the
/// whole reason a second question is asked.
///
/// **0.5 is the honest halfway point:** the valley must hold at most half the density the modes
/// do. It is not a number read off a run — a mean-residual threshold *would* have had to be,
/// because the fixture's eye measures 0.126 against a continuous spread's 0.167 and no principled
/// constant separates those two.
///
/// **This is the abstention, and it is why [`Levels::Indeterminate`] is a variant rather than a
/// fallback to 2.** An analogue FM burst has no symbol alphabet at all; a confident answer for
/// one is the failure mode the open-set stance exists to prevent (T-614).
pub const MAX_VALLEY_RATIO: f64 = 0.5;

/// Lowest symbol rate searched for, Bd. Below this, a 12.5 kHz-class channel is not carrying
/// symbols at a rate any of this project's demodulators can consume.
pub const MIN_SYMBOL_RATE_BD: f64 = 300.0;

/// How many discrete levels the emission's discriminator was measured to hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Levels {
    /// Two levels: the inner pair is empty.
    Two,
    /// Four levels: the inner pair carries a substantial share of the symbols.
    Four,
    /// **Neither hypothesis fits the measurement.** Not a failure and not a default: an analogue
    /// FM emission has no symbol alphabet, and this is what saying so looks like.
    Indeterminate,
}

impl Levels {
    /// The modulation order, when one was measured. `None` for [`Levels::Indeterminate`] — the
    /// abstention has to survive into the data model or it was never made.
    pub fn order(self) -> Option<u32> {
        match self {
            Levels::Two => Some(2),
            Levels::Four => Some(4),
            Levels::Indeterminate => None,
        }
    }

    /// The modulation label for [`hk_model::EstimatedParams`], when one was measured.
    ///
    /// Four-level FM at a symbol clock **is** C4FM (docs/04 §7.2, docs/19 §2.1): the name states
    /// the measured structure and nothing about which service or protocol uses it.
    pub fn label(self) -> Option<&'static str> {
        match self {
            Levels::Two => Some("2fsk"),
            Levels::Four => Some("c4fm"),
            Levels::Indeterminate => None,
        }
    }
}

/// What was measured about an FM emission's symbol structure.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FmStructure {
    /// Measured symbol rate, Bd — the interpolated cyclostationary clock line.
    pub symbol_rate_bd: f64,
    /// Significance of that line over its band's median, in bits.
    pub clock_bits: f64,
    /// Level count.
    pub levels: Levels,
    /// Fraction of symbols on the inner pair, 0–1 — what decided [`Self::levels`].
    pub inner_fraction: f64,
    /// Outer deviation, Hz.
    pub outer_deviation_hz: f64,
    /// Residual carrier offset removed before slicing, Hz.
    pub residual_cfo_hz: f64,
    /// Mean distance of a symbol from the nearest level of the chosen alphabet, 0–1.
    pub level_fit: f64,
    /// Density at the midpoint between the levels, against the mean density at the levels — the
    /// **valley** that a discrete alphabet has and a continuous modulation does not
    /// ([`MAX_VALLEY_RATIO`]). 1 is no valley at all.
    pub valley_ratio: f64,
    /// Symbols the level statistics were measured over.
    pub symbols: usize,
    /// Symbol-timing phase chosen, in samples.
    pub timing_phase: f64,
}

/// Why a measurement produced nothing.
#[derive(Clone, Debug, PartialEq)]
pub enum StructureError {
    /// Too few samples to transform.
    TooFewSamples(usize),
    /// The request made no sense.
    InvalidRequest(String),
    /// No clock line stood [`MIN_CLOCK_BITS`] above the band: **there is no symbol rate to
    /// report**, which is a finding rather than an error — an unclocked emission (analogue FM, a
    /// bare carrier, noise) lands here.
    NoClockLine {
        /// The best line found anyway, Bd.
        best_bd: f64,
        /// Its significance, bits.
        bits: f64,
    },
}

/// Measures the symbol structure of baseband `samples` at `sample_rate_hz`.
///
/// `channel_bandwidth_hz` is **the width of the channel the caller already selected** (S0's
/// output, not a guess about the modulation): it filters the discriminator's input and bounds the
/// clock search, since a symbol rate wider than its own channel is not physical. `None` leaves
/// both to Nyquist. Nothing else about the emission is supplied, and in particular **no expected
/// symbol rate, deviation or level count**.
///
/// The filter is not optional in practice. A trunking DDC is deliberately wider than the channel
/// so the demodulator can apply its own selectivity, which leaves the neighbouring 12.5 kHz
/// channels inside the passband — and an unfiltered discriminator then measures *their* clock
/// instead, or no clock at all.
pub fn measure(
    samples: &[Complex32],
    sample_rate_hz: f64,
    channel_bandwidth_hz: Option<f64>,
) -> Result<FmStructure, StructureError> {
    let fs = sample_rate_hz;
    if !(fs.is_finite() && fs > 0.0) {
        return Err(StructureError::InvalidRequest(format!("fs {fs}")));
    }
    let n = samples.len();
    if n < 1024 {
        return Err(StructureError::TooFewSamples(n));
    }

    // ---- 1. Channel selectivity, then the quadrature discriminator in Hz. The passband is half
    // the caller's channel width — the channel's own edge — so an adjacent channel's energy
    // cannot contribute a clock line of its own.
    let channel = channel_bandwidth_hz.filter(|b| b.is_finite() && *b > 0.0);
    let fi = discriminate(samples, fs, channel.map(|bw| (0.5 * bw, 0.75 * bw)))?;

    // ---- 2. The clock line. The squared first difference is a pulse train at the baud rate.
    let hi = channel
        .filter(|v| *v > MIN_SYMBOL_RATE_BD)
        .unwrap_or(f64::INFINITY)
        .min(0.4 * fs);
    if hi <= MIN_SYMBOL_RATE_BD {
        return Err(StructureError::InvalidRequest(format!(
            "no searchable symbol-rate band below {hi} Bd"
        )));
    }
    let line = clock_line(&fi, fs, MIN_SYMBOL_RATE_BD, hi)?;
    if line.bits < MIN_CLOCK_BITS {
        return Err(StructureError::NoClockLine {
            best_bd: line.rate_bd,
            bits: line.bits,
        });
    }
    let rate = line.rate_bd;
    let sps = fs / rate;

    // ---- 3. Re-filter from what was just measured, then take symbol values at the best timing
    // phase. **This is the closed loop the product asks for**: the first filter was set from the
    // channel the caller selected, which for a trunking DDC is deliberately wider than the
    // modulation; a second pass at the *modulation's* own occupancy — outer deviation plus half a
    // symbol rate, the Carson figure, both now measured — removes the noise and adjacent-channel
    // skirt that sits between the levels. Without it a real pulse-shaped channel's eye reads as
    // half-closed and the level count abstains on a signal it can plainly see.
    let rough = {
        let w = ((0.5 * sps).round() as usize).max(1);
        let y = boxcar(&fi, w);
        let c = y.iter().sum::<f64>() / y.len() as f64;
        quantile_abs(&y.iter().map(|v| v - c).collect::<Vec<_>>(), 0.80).max(1e-9)
    };
    let fi = discriminate(
        samples,
        fs,
        Some((rough + 0.6 * rate, rough + 1.1 * rate)).filter(|&(p, st)| {
            // Only when it is actually narrower than what stage 1 used, and realisable.
            channel.is_none_or(|bw| p < 0.5 * bw) && st < 0.49 * fs
        }),
    )
    .unwrap_or(fi);
    let w_len = ((0.5 * sps).round() as usize).max(1);
    let y = boxcar(&fi, w_len);
    let residual_cfo_hz = y.iter().sum::<f64>() / y.len() as f64;
    let yc: Vec<f64> = y.iter().map(|v| v - residual_cfo_hz).collect();
    let outer = quantile_abs(&yc, 0.80).max(1e-9);
    let n_sym = ((n as f64 - sps) / sps).floor().max(0.0) as usize;
    if n_sym < 64 {
        return Err(StructureError::TooFewSamples(n));
    }

    // The phase is chosen by the eye: the four-level ideal set is used as the scoring alphabet
    // because it contains the two-level one, so neither hypothesis is favoured by the choice.
    const PHASES: usize = 32;
    let mut best = (f64::INFINITY, 0.0f64, Vec::new());
    for p in 0..PHASES {
        let ph = sps * p as f64 / PHASES as f64;
        let mut vals = Vec::with_capacity(n_sym);
        let mut acc = 0.0;
        for k in 0..n_sym {
            let t = ph + sps * k as f64;
            if t >= (n - 1) as f64 {
                break;
            }
            let v = interp(&yc, t) / outer;
            acc += four_level_distance(v);
            vals.push(v);
        }
        if vals.is_empty() {
            continue;
        }
        let score = acc / vals.len() as f64;
        if score < best.0 {
            best = (score, ph, vals);
        }
    }
    let (_, timing_phase, vals) = best;
    if vals.len() < 64 {
        return Err(StructureError::TooFewSamples(n));
    }

    // ---- 4. Level count. The inner pair's population is the question; the midpoint between the
    // inner (1/3) and outer (1) ideals is 2/3, so a symbol below it is an inner one.
    const INNER_OUTER_MID: f64 = 2.0 / 3.0;
    let inner = vals.iter().filter(|v| v.abs() < INNER_OUTER_MID).count();
    let n_vals = vals.len() as f64;
    let inner_fraction = inner as f64 / n_vals;
    let valley = valley_ratio(&vals);
    let four_fit = vals.iter().map(|&v| four_level_distance(v)).sum::<f64>() / n_vals;
    let two_fit = vals.iter().map(|&v| (v.abs() - 1.0).abs()).sum::<f64>() / n_vals;
    // Two questions, and both must answer yes: is the inner pair populated the way that alphabet
    // would populate it, AND is there a VALLEY between the levels? Skipping the second is how a
    // continuous-valued analogue emission gets called four-level, because it populates the inner
    // pair just as well; using the residual *width* instead of the valley's *shape* is how a real
    // half-open eye gets called analogue.
    let [lo, hi_f] = INNER_FRACTION_FOUR;
    let (levels, level_fit) = if (lo..=hi_f).contains(&inner_fraction) && valley <= MAX_VALLEY_RATIO
    {
        (Levels::Four, four_fit)
    } else if inner_fraction <= INNER_FRACTION_TWO {
        // A two-level alphabet needs no valley test: a continuous spread puts two thirds of
        // its samples below the inner/outer midpoint, so an empty inner pair IS the valley.
        (Levels::Two, two_fit)
    } else {
        (Levels::Indeterminate, four_fit)
    };

    Ok(FmStructure {
        symbol_rate_bd: rate,
        clock_bits: line.bits,
        levels,
        inner_fraction,
        outer_deviation_hz: outer,
        residual_cfo_hz,
        level_fit,
        valley_ratio: valley,
        symbols: vals.len(),
        timing_phase,
    })
}

/// The quadrature discriminator of `samples`, in Hz, optionally through a `(pass, stop)` lowpass.
///
/// A filter that cannot be realised at this sample rate is skipped rather than failing: the
/// measurement is then noisier, which the level fit reports, instead of absent.
fn discriminate(
    samples: &[Complex32],
    fs: f64,
    band: Option<(f64, f64)>,
) -> Result<Vec<f64>, StructureError> {
    let filtered = match band {
        Some((pass, stop)) if pass > 0.0 && stop > pass && stop < 0.49 * fs => {
            match lowpass_taps(fs, pass, stop, 40.0) {
                Ok(taps) => filter_same(samples, &taps),
                Err(_) => samples.to_vec(),
            }
        }
        _ => samples.to_vec(),
    };
    let n = filtered.len();
    if n < 2 {
        return Err(StructureError::TooFewSamples(n));
    }
    let mut fi = vec![0.0f64; n];
    for k in 1..n {
        let z = filtered[k] * filtered[k - 1].conj();
        fi[k] = f64::from(z.im).atan2(f64::from(z.re)) * fs / TAU;
    }
    fi[0] = fi[1];
    Ok(fi)
}

/// The interpolated clock line and its significance.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ClockLine {
    rate_bd: f64,
    bits: f64,
}

/// Finds the strongest spectral line of the squared discriminator difference in
/// `[lo_bd, hi_bd]`, and scores it against the median of that band.
fn clock_line(fi: &[f64], fs: f64, lo_bd: f64, hi_bd: f64) -> Result<ClockLine, StructureError> {
    // A power of two at or below the available length; the transform is over the whole window, so
    // the line's resolution is fs/N and a 0.5 s window resolves 4800 Bd to a couple of hertz.
    let n = fi.len().next_power_of_two() / if fi.len().is_power_of_two() { 1 } else { 2 };
    if n < 1024 {
        return Err(StructureError::TooFewSamples(fi.len()));
    }
    let mut d: Vec<f64> = (0..n)
        .map(|k| {
            let step = if k == 0 { 0.0 } else { fi[k] - fi[k - 1] };
            step * step
        })
        .collect();
    let mean = d.iter().sum::<f64>() / n as f64;
    // Hann, to keep a strong line from smearing across the band it is being compared against.
    let mut buf: Vec<Complex32> = d
        .iter_mut()
        .enumerate()
        .map(|(k, v)| {
            let w = 0.5 - 0.5 * (TAU * k as f64 / n as f64).cos();
            Complex32::new(((*v - mean) * w) as f32, 0.0)
        })
        .collect();
    let mut fft = CpuFft::new(n);
    fft.forward(&mut buf);

    let bin_hz = fs / n as f64;
    let lo = ((lo_bd / bin_hz).ceil() as usize).max(1);
    let hi = ((hi_bd / bin_hz).floor() as usize).min(n / 2 - 1);
    if hi <= lo + 8 {
        return Err(StructureError::InvalidRequest(format!(
            "symbol-rate band {lo_bd}-{hi_bd} Bd is under 8 bins wide at {bin_hz:.1} Hz"
        )));
    }
    let mag: Vec<f64> = buf[lo..=hi].iter().map(|c| f64::from(c.norm())).collect();
    let (k, &peak) = mag
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .expect("the band is at least 8 bins wide");
    let mut sorted = mag.clone();
    sorted.sort_by(f64::total_cmp);
    let median = sorted[sorted.len() / 2];
    let bits = if median > 0.0 && peak > 0.0 {
        (peak / median).log2()
    } else {
        0.0
    };
    // Parabolic interpolation of the peak: the line rarely sits on a bin centre, and a half-bin
    // error at a 2 Hz resolution is what keeps the answer inside a 1 % tolerance.
    let delta = if k > 0 && k + 1 < mag.len() {
        let (a, b, c) = (mag[k - 1], mag[k], mag[k + 1]);
        let denom = a - 2.0 * b + c;
        if denom.abs() > f64::EPSILON {
            (0.5 * (a - c) / denom).clamp(-0.5, 0.5)
        } else {
            0.0
        }
    } else {
        0.0
    };
    Ok(ClockLine {
        rate_bd: ((lo + k) as f64 + delta) * bin_hz,
        bits,
    })
}

/// The valley between the four levels: the midpoint window's share of `|v|` against the mean of
/// the two level windows (see [`MAX_VALLEY_RATIO`]).
///
/// `1.0` when the levels are empty (no modes at all), which is the honest reading of "no valley".
fn valley_ratio(vals: &[f64]) -> f64 {
    const W: f64 = 1.0 / 9.0;
    let count = |c: f64| vals.iter().filter(|v| (v.abs() - c).abs() <= W).count() as f64;
    let inner = count(1.0 / 3.0);
    let outer = count(1.0);
    let mid = count(2.0 / 3.0);
    let peaks = 0.5 * (inner + outer);
    if peaks <= 0.0 { 1.0 } else { mid / peaks }
}

/// Distance from `v` to the nearest of the four ideal levels `±1/3, ±1`.
fn four_level_distance(v: f64) -> f64 {
    const IDEAL: [f64; 4] = [1.0, 1.0 / 3.0, -1.0 / 3.0, -1.0];
    IDEAL
        .iter()
        .map(|l| (v - l).abs())
        .fold(f64::INFINITY, f64::min)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 24_000.0;

    /// A symbol-clocked FM emission with `levels` equiprobable levels at `outer_hz`, plus noise.
    fn fm_symbols(
        levels: usize,
        rate_bd: f64,
        outer_hz: f64,
        secs: f64,
        noise: f64,
    ) -> Vec<Complex32> {
        let n = (FS * secs) as usize;
        let sps = FS / rate_bd;
        let mut state = 0xDEAD_BEEF_1234_5678u64;
        let mut rand = move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        let alphabet: Vec<f64> = match levels {
            2 => vec![-1.0, 1.0],
            _ => vec![-1.0, -1.0 / 3.0, 1.0 / 3.0, 1.0],
        };
        let n_sym = (n as f64 / sps).ceil() as usize + 2;
        let syms: Vec<f64> = (0..n_sym)
            .map(|_| alphabet[(rand() * alphabet.len() as f64) as usize % alphabet.len()])
            .collect();
        let mut phase = 0.0f64;
        (0..n)
            .map(|k| {
                let s = syms[(k as f64 / sps) as usize];
                let f = s * outer_hz + (rand() - 0.5) * noise;
                phase += TAU * f / FS;
                Complex32::new(phase.cos() as f32, phase.sin() as f32)
            })
            .collect()
    }

    /// **The answer T-545 found the estimator could not give.** Four levels, and the rate and
    /// deviation inside the a-priori tolerances the MAUTO suite sets from `docs/19 §2.1`.
    #[test]
    fn four_level_fm_is_measured_as_four_levels_at_its_own_rate_and_deviation() {
        let iq = fm_symbols(4, 4800.0, 1800.0, 0.6, 20.0);
        let m = measure(&iq, FS, Some(12_500.0)).expect("a clocked four-level emission");
        assert_eq!(m.levels, Levels::Four, "{m:?}");
        assert_eq!(m.levels.order(), Some(4));
        assert_eq!(m.levels.label(), Some("c4fm"));
        assert!(
            (m.symbol_rate_bd - 4800.0).abs() <= 48.0,
            "symbol rate {:.1} Bd, wanted 4800 +/- 48 (the 1 % that separates P25 Phase 1 from \
             Phase 2's 6000 Bd)",
            m.symbol_rate_bd,
        );
        assert!(
            (m.outer_deviation_hz - 1800.0).abs() <= 360.0,
            "outer deviation {:.0} Hz, wanted 1800 +/- 360",
            m.outer_deviation_hz,
        );
        assert!(m.clock_bits >= MIN_CLOCK_BITS, "{m:?}");
    }

    /// Two levels are not four: the inner pair is what separates them, and it is empty here.
    #[test]
    fn two_level_fm_is_not_reported_as_four() {
        let iq = fm_symbols(2, 4800.0, 1800.0, 0.6, 20.0);
        let m = measure(&iq, FS, Some(12_500.0)).expect("a clocked two-level emission");
        assert_eq!(m.levels, Levels::Two, "{m:?}");
        assert_eq!(m.levels.order(), Some(2));
        assert!(m.inner_fraction <= INNER_FRACTION_TWO, "{m:?}");
    }

    /// A different rate is measured as a different rate — the estimator is not returning a
    /// constant it was handed.
    #[test]
    fn a_different_symbol_rate_reads_as_that_rate() {
        let iq = fm_symbols(4, 2400.0, 1800.0, 0.6, 20.0);
        let m = measure(&iq, FS, Some(12_500.0)).expect("a clocked emission");
        assert!(
            (m.symbol_rate_bd - 2400.0).abs() <= 24.0,
            "measured {:.1} Bd for a 2400 Bd emission",
            m.symbol_rate_bd,
        );
    }

    /// **The abstention, and the reason it is a variant rather than a fallback to 2.** Analogue
    /// FM carrying voice — a *continuous*-valued modulation with no symbol alphabet — must not
    /// come back with a modulation order, which is T-614's confidently-wrong outcome. It is the
    /// hard case precisely because it populates the inner pair as well as a four-level alphabet
    /// does; only the valley test separates them.
    #[test]
    fn analogue_voice_fm_yields_no_modulation_order_rather_than_a_confident_answer() {
        let n = (FS * 0.6) as usize;
        let mut state = 0xCAFE_F00D_1234_5678u64;
        let mut rand = move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            (state >> 11) as f64 / (1u64 << 53) as f64 - 0.5
        };
        // Band-limited noise as the modulating signal: what voice is, and what a symbol stream
        // is not. One-pole smoothing puts its energy in the audio band.
        let mut audio = 0.0f64;
        let mut phase = 0.0f64;
        let iq: Vec<Complex32> = (0..n)
            .map(|_| {
                audio = 0.92 * audio + 0.08 * rand();
                phase += TAU * (audio * 8.0 * 2_400.0) / FS;
                Complex32::new(phase.cos() as f32, phase.sin() as f32)
            })
            .collect();
        match measure(&iq, FS, Some(12_500.0)) {
            Err(StructureError::NoClockLine { .. }) => {}
            Ok(m) => {
                assert_eq!(
                    m.levels,
                    Levels::Indeterminate,
                    "analogue FM has no symbol alphabet; measuring one is worse than \
                     abstaining: {m:?}",
                );
                assert_eq!(
                    m.levels.order(),
                    None,
                    "no mod_order for an analogue emission"
                );
                assert_eq!(m.levels.label(), None);
            }
            Err(e) => panic!("unexpected {e:?}"),
        }
    }

    /// A **periodic** analogue modulation (a single tone) is the documented limit of this
    /// measurement, recorded rather than hidden.
    ///
    /// A tone-modulated FM carrier really *is* periodic, so reporting a clock line for it is not
    /// wrong. But its discriminator is a sine, and sampling a sine at twice its own frequency
    /// lands every sample near an extreme — which looks exactly like a two-level alphabet, and
    /// the valley test cannot separate them because the values genuinely do sit on two levels.
    /// **What this ticket owns is the four-level answer, and that one must not fire**; the
    /// two-level mislabelling of analogue FM is T-614's, and this test pins the boundary between
    /// the two so a later fix has something to move.
    #[test]
    fn a_pure_tone_is_never_called_four_level_though_it_can_still_look_two_level() {
        let n = (FS * 0.6) as usize;
        let mut phase = 0.0f64;
        let iq: Vec<Complex32> = (0..n)
            .map(|k| {
                let f = 2_000.0 * (TAU * 300.0 * k as f64 / FS).sin();
                phase += TAU * f / FS;
                Complex32::new(phase.cos() as f32, phase.sin() as f32)
            })
            .collect();
        if let Ok(m) = measure(&iq, FS, Some(12_500.0)) {
            assert_ne!(
                m.levels,
                Levels::Four,
                "a tone-modulated carrier has no four-level alphabet: {m:?}",
            );
        }
    }

    /// **The threshold's own evidence.** [`MAX_VALLEY_RATIO`] is set a priori at the halfway
    /// point between a uniform spread (1.0) and a discrete alphabet (~0); this measures where the
    /// two populations actually land, so the constant is defended rather than asserted — and so a
    /// later change to the estimator that closes the gap shows up here rather than silently.
    ///
    /// It also records the **hard** case: a real pulse-shaped C4FM channel over the air has a
    /// half-open eye whose mean distance to a level is ~0.13 against a continuous spread's 0.167.
    /// No constant on *that* statistic separates them, which is why the valley's shape is the
    /// statistic and its width is not.
    #[test]
    fn the_valley_threshold_sits_between_the_two_populations_that_have_to_be_told_apart() {
        let four = measure(
            &fm_symbols(4, 4800.0, 1800.0, 0.6, 20.0),
            FS,
            Some(12_500.0),
        )
        .expect("a clocked four-level emission");

        // **The adversary the valley test exists for**: a signal with the same symbol clock and
        // the same deviation, whose symbol values are drawn CONTINUOUSLY instead of from an
        // alphabet. It clocks, so the rate is found; it populates the inner pair exactly as a
        // four-level alphabet does; and it has no valley. Nothing else in this file separates it,
        // and a two- or four-level answer for it would be the confidently-wrong outcome.
        let n = (FS * 0.6) as usize;
        let sps = FS / 4800.0;
        let mut state = 0xCAFE_F00D_1234_5678u64;
        let mut rand = move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        let syms: Vec<f64> = (0..(n as f64 / sps) as usize + 2)
            .map(|_| 2.0 * rand() - 1.0)
            .collect();
        let mut phase = 0.0f64;
        let iq: Vec<Complex32> = (0..n)
            .map(|k| {
                phase += TAU * (syms[(k as f64 / sps) as usize] * 1800.0) / FS;
                Complex32::new(phase.cos() as f32, phase.sin() as f32)
            })
            .collect();
        let continuous = measure(&iq, FS, Some(12_500.0));

        eprintln!("four-level: {four:?}\ncontinuous: {continuous:?}");
        assert!(
            four.valley_ratio < MAX_VALLEY_RATIO,
            "a four-level alphabet must leave a valley: {four:?}",
        );
        let c = continuous.expect("the adversary clocks: it must reach the level question");
        assert_eq!(
            c.levels,
            Levels::Indeterminate,
            "a continuously-valued modulation has no symbol alphabet: {c:?}",
        );
        assert!(
            c.valley_ratio > MAX_VALLEY_RATIO,
            "a continuous modulation must leave no valley: {c:?}",
        );
        // Both populate the inner pair the same way, which is exactly why the valley is the
        // discriminator and the inner fraction alone is not.
        let [lo, hi] = INNER_FRACTION_FOUR;
        assert!(
            (lo..=hi).contains(&c.inner_fraction),
            "if this stops holding, the test has stopped covering the hard case: {c:?}",
        );
    }

    /// Noise has no clock line, and the measurement says so instead of inventing one.
    #[test]
    fn noise_yields_no_clock_line() {
        let mut state = 0x1234_5678_9ABC_DEF0u64;
        let iq: Vec<Complex32> = (0..(FS as usize))
            .map(|_| {
                let mut r = || {
                    state = state
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1);
                    (state >> 11) as f32 / (1u64 << 53) as f32 - 0.5
                };
                Complex32::new(r(), r())
            })
            .collect();
        match measure(&iq, FS, Some(12_500.0)) {
            Err(StructureError::NoClockLine { bits, .. }) => {
                assert!(bits < MIN_CLOCK_BITS, "noise scored {bits:.2} bits");
            }
            other => panic!("noise must not produce a symbol rate: {other:?}"),
        }
    }

    #[test]
    fn a_degenerate_request_is_refused_rather_than_guessed() {
        let iq = fm_symbols(4, 4800.0, 1800.0, 0.1, 20.0);
        assert!(matches!(
            measure(&iq[..16], FS, None),
            Err(StructureError::TooFewSamples(_))
        ));
        assert!(matches!(
            measure(&iq, 0.0, None),
            Err(StructureError::InvalidRequest(_))
        ));
        assert!(matches!(
            measure(&iq, FS, Some(10.0)),
            // Below MIN_SYMBOL_RATE_BD the bound is ignored and the Nyquist one used, so this
            // still measures; what must never happen is a panic or a fabricated rate.
            Ok(_) | Err(_)
        ));
    }
}
