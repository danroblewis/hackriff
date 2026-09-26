//! ATSC 1.0 (8VSB) channel recognition from a tuned window (T-979).
//!
//! # Why a 6 MHz television channel needs its own recogniser
//!
//! An 8VSB emission is a **flat, noise-like plateau 5.381 MHz wide with one CW pilot near its
//! lower edge**. To a per-frame CFAR detector that is not one signal: the plateau's own ripple
//! crosses the threshold in dozens of places, so one channel arrives as 15–60 narrow and medium
//! candidates, none of which is 6 MHz wide and none of which is explicable. That is exactly what
//! the 2026-09-25 explorer window saw across UHF 470–608 MHz in San Francisco: 13 pilots found
//! blind, 13 channels shattered, and the fragments inside channels 29/30 suggested as
//! `fm-broadcast` at 0.6 because a 200 kHz-ish continuous fragment has the broadcast-FM *shape*.
//!
//! So the recognition is done on the **shape of the whole channel**, not on its fragments:
//!
//! 1. an averaged (Welch) spectrum of the tuned window;
//! 2. a contiguous run above the floor whose **half-power width** is [`FLAT_BANDWIDTH_HZ`]
//!    (5.381 MHz) — the Nyquist band of the 8VSB symbol rate, whose raised-cosine transitions are
//!    centred on the band edges, so the −3 dB points *are* the band edges;
//! 3. that run is **flat** (an 8VSB signal is noise-like: no carrier, no discrete structure);
//! 4. a **CW pilot sitting on that lower edge**, measured with
//!    [`crate::clock::tone_frequency_iq`] to a fraction of a bin. The 6 MHz channel's own lower
//!    edge is then [`PILOT_OFFSET_HZ`] (309.440 559 kHz) below the pilot.
//!
//! Every one of those four is measured from the signal. Nothing is looked up, and no band plan is
//! consulted to *find* a channel: `find_channels` gives the same answer at 470 MHz, at 1.2 GHz or
//! on a replay whose centre frequency is a lie. What the US channel grid is used for is the
//! opposite direction — see "The pilot is a free ppm measurement" below.
//!
//! # Numbers, and where they come from
//!
//! ATSC A/53 Part 2 (*RF/Transmission System Characteristics*) §5.1.2 and Annex D:
//!
//! - symbol rate `(4.5/286) × 684 MHz` = 10.762 237 762 MBd ([`SYMBOL_RATE_BD`]) — the 4.5 MHz /
//!   286 tie to the NTSC line rate that the standard inherited;
//! - the Nyquist (flat) band is half the symbol rate, 5.381 118 881 MHz ([`FLAT_BANDWIDTH_HZ`]),
//!   centred in the 6 MHz channel, i.e. from `lower edge + 0.309 44 MHz` to
//!   `lower edge + 5.690 56 MHz`;
//! - the pilot is the DC term of the 8VSB baseband, left at the lower Nyquist edge by the vestigial
//!   sideband: `lower channel edge + 309.440 559 kHz` ([`PILOT_OFFSET_HZ`]), 11.3 dB below average
//!   signal power — but concentrated in one FFT bin, so tens of dB above the plateau's *density*.
//!
//! # The pilot is a free ppm measurement (C05)
//!
//! The pilot is a transmitted CW line whose frequency is fixed by a standard, so reading it is a
//! measurement of **this receiver's clock**, in the same sense as [`crate::clock::ppm_from_line`]
//! on the 19 kHz FM stereo pilot and as the LMR raster of T-560: an a-priori standard, never a
//! truth table. The explorer's 13 pilots all read ≈ 2.4 kHz low at 470–602 MHz, i.e. **−4 ppm**
//! as an offset, which is a receiver oscillator **+4 ppm fast** — one number, thirteen agreeing
//! measurements, free with the detection.
//!
//! [`AtscChannel::ppm`] is that reading (the *offset* convention of [`crate::clock`]: how this
//! receiver reads frequencies). It is produced **only** when the measured channel edge lands on
//! the US UHF grid within [`AtscConfig::grid_tolerance_hz`], because a ppm needs a nominal, and
//! the nominal is the grid line. The grid never decides whether a channel was found.

use num_complex::Complex32;

use crate::clock::{ppm_from_line, tone_frequency_iq};
use crate::dsp::{Workspace, choose_nfft};
use crate::estimate::{Estimate, Method, Reason};

/// 8VSB symbol rate, Bd: `(4.5 / 286) × 684 MHz` (ATSC A/53 Part 2 §5.1.2).
pub const SYMBOL_RATE_BD: f64 = 4.5e6 / 286.0 * 684.0;

/// Width of the flat (Nyquist) band of an 8VSB emission, Hz: half the symbol rate, 5.381 118 881
/// MHz, measured between the half-power points of its raised-cosine transitions.
pub const FLAT_BANDWIDTH_HZ: f64 = SYMBOL_RATE_BD / 2.0;

/// The pilot's offset above the lower edge of the 6 MHz channel, Hz (ATSC A/53 Part 2 §5.1.2):
/// `(6 MHz − FLAT_BANDWIDTH_HZ) / 2` = 309.440 559 kHz.
pub const PILOT_OFFSET_HZ: f64 = (CHANNEL_BANDWIDTH_HZ - FLAT_BANDWIDTH_HZ) / 2.0;

/// One television channel, Hz (47 CFR 73.603; ITU-R BT.1701 System A/M 6 MHz raster).
pub const CHANNEL_BANDWIDTH_HZ: f64 = 6e6;

/// Lower edge of US UHF television channel 14, Hz (47 CFR 73.603(a), 73.699 Figure 1).
pub const US_UHF_CH14_LO_HZ: f64 = 470e6;

/// Lowest US UHF television channel of the post-repack band plan.
pub const US_UHF_FIRST_CHANNEL: u16 = 14;

/// Highest US UHF television channel of the post-repack band plan (602–608 MHz). Channel 37
/// (608–614 MHz) is reserved for radio astronomy and WMTS, and 38–51 were reallocated to the
/// 600 MHz band by the 2017 incentive-auction repack.
pub const US_UHF_LAST_CHANNEL: u16 = 36;

/// Lower edge of US UHF television channel `n`, Hz, for `n` in
/// [`US_UHF_FIRST_CHANNEL`]..=[`US_UHF_LAST_CHANNEL`].
pub fn us_uhf_channel_lo_hz(n: u16) -> Option<f64> {
    (US_UHF_FIRST_CHANNEL..=US_UHF_LAST_CHANNEL)
        .contains(&n)
        .then(|| US_UHF_CH14_LO_HZ + f64::from(n - US_UHF_FIRST_CHANNEL) * CHANNEL_BANDWIDTH_HZ)
}

/// The US UHF channel whose lower edge is nearest `f_lo_hz`, and that edge, when one is within
/// `tolerance_hz`.
pub fn us_uhf_channel_at(f_lo_hz: f64, tolerance_hz: f64) -> Option<(u16, f64)> {
    let k = ((f_lo_hz - US_UHF_CH14_LO_HZ) / CHANNEL_BANDWIDTH_HZ).round();
    let n = u16::try_from(US_UHF_FIRST_CHANNEL as i64 + k as i64).ok()?;
    let lo = us_uhf_channel_lo_hz(n)?;
    ((f_lo_hz - lo).abs() <= tolerance_hz).then_some((n, lo))
}

/// What [`find_channels`] must see before it calls an emission 8VSB.
///
/// Every bound is a property of the ATSC waveform or of the measurement, never of a location: the
/// same configuration recognises a channel anywhere in the spectrum.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AtscConfig {
    /// Smallest plateau-over-floor ratio counted as an emission, dB.
    pub min_plateau_snr_db: f64,
    /// Largest fractional departure of the measured half-power width from
    /// [`FLAT_BANDWIDTH_HZ`], 0–1.
    pub width_tolerance: f64,
    /// Largest in-band ripple (p90 − p10 of the smoothed plateau) still called flat, dB. An 8VSB
    /// plateau is flat to a fraction of a dB over the air; the budget here is for multipath.
    pub max_flatness_db: f64,
    /// Half-width of the pilot search about the nominal offset above the *measured* lower edge,
    /// Hz. It covers the edge measurement's own error plus any receiver clock error.
    pub pilot_search_hz: f64,
    /// Smallest pilot-over-plateau ratio, dB, in the analysis bin.
    pub min_pilot_excess_db: f64,
    /// Largest distance from the US UHF grid at which a channel number and a ppm are reported, Hz.
    pub grid_tolerance_hz: f64,
    /// Smoothing applied to the spectrum before the plateau search, Hz. It must exceed the pilot's
    /// width so the pilot cannot be mistaken for a plateau edge, and stay far below the Nyquist
    /// transition so the edges survive.
    pub smooth_hz: f64,
}

impl Default for AtscConfig {
    fn default() -> Self {
        Self {
            min_plateau_snr_db: 6.0,
            width_tolerance: 0.12,
            max_flatness_db: 4.0,
            pilot_search_hz: 60e3,
            min_pilot_excess_db: 8.0,
            grid_tolerance_hz: 50e3,
            smooth_hz: 50e3,
        }
    }
}

/// One recognised 8VSB channel. Every field but [`Self::us_channel`] and [`Self::ppm`] is measured
/// from the signal alone.
#[derive(Clone, Debug, PartialEq)]
pub struct AtscChannel {
    /// Measured pilot frequency, absolute Hz, with its one-sigma uncertainty.
    pub pilot: Estimate,
    /// Lower edge of the 6 MHz channel implied by the measured pilot, Hz.
    pub f_lo_hz: f64,
    /// Upper edge, Hz (`f_lo_hz + `[`CHANNEL_BANDWIDTH_HZ`]).
    pub f_hi_hz: f64,
    /// Centre of the 6 MHz channel, Hz.
    pub f_center_hz: f64,
    /// Measured half-power width of the flat band, Hz (nominally [`FLAT_BANDWIDTH_HZ`]).
    pub flat_bandwidth_hz: f64,
    /// Plateau median over the noise floor, dB.
    pub plateau_snr_db: f64,
    /// Pilot bin over the plateau median, dB.
    pub pilot_excess_db: f64,
    /// In-band ripple, p90 − p10 of the smoothed plateau, dB.
    pub flatness_db: f64,
    /// US UHF channel number, when the measured edge lands on the grid (see the module docs).
    pub us_channel: Option<u16>,
    /// Receiver clock error read off the pilot, ppm (offset convention: how this receiver reads
    /// frequencies). Abstains when no grid line supplies a nominal.
    pub ppm: Estimate,
}

impl AtscChannel {
    /// The nominal pilot of [`Self::us_channel`], Hz, when the channel is on the grid.
    pub fn nominal_pilot_hz(&self) -> Option<f64> {
        let lo = us_uhf_channel_lo_hz(self.us_channel?)?;
        Some(lo + PILOT_OFFSET_HZ)
    }

    /// Measured pilot minus the nominal one, Hz — the explorer's "≈ 2.4 kHz low".
    pub fn pilot_offset_hz(&self) -> Option<f64> {
        Some(self.pilot.value()? - self.nominal_pilot_hz()?)
    }
}

/// Recognises the 8VSB channels in one tuned window of IQ (see the [module docs](self)).
///
/// `center_hz` is the window's RF centre and `fs` its sample rate; the returned frequencies are
/// absolute. The window must be wider than one flat band plus some floor to measure against —
/// with less, the function returns nothing rather than a guess.
pub fn find_channels(
    x: &[Complex32],
    fs: f64,
    center_hz: f64,
    cfg: &AtscConfig,
) -> Vec<AtscChannel> {
    if !(fs.is_finite() && fs > FLAT_BANDWIDTH_HZ * 1.15 && center_hz.is_finite()) {
        return Vec::new();
    }
    let Some(nfft) = choose_nfft(x.len()) else {
        return Vec::new();
    };
    let mut work = Workspace::default();
    let psd = work.welch(x, fs, nfft);
    let df = psd.df();
    // The plateau edges must survive smoothing, and the pilot must not survive it.
    let half = ((cfg.smooth_hz / df / 2.0).round() as usize).max(1);
    let db: Vec<f64> = psd
        .p
        .iter()
        .map(|&p| 10.0 * p.max(1e-300).log10())
        .collect();
    let smooth = smooth_db(&db, half);
    let floor_db = percentile(&smooth, 0.10);

    let mut out = Vec::new();
    for run in runs_above(&smooth, floor_db + cfg.min_plateau_snr_db) {
        // A run that reaches the edge of the analysed band is truncated, not measured.
        if run.start == 0 || run.end >= smooth.len() {
            continue;
        }
        let plateau_db = median(&smooth[run.clone()]);
        let Some((lo_bin, hi_bin)) = half_power_edges(&smooth, run.clone(), plateau_db) else {
            continue;
        };
        let width = (hi_bin - lo_bin) * df;
        if (width - FLAT_BANDWIDTH_HZ).abs() > FLAT_BANDWIDTH_HZ * cfg.width_tolerance {
            continue;
        }
        // Flatness is judged strictly inside the transitions and away from the pilot.
        let guard = (PILOT_OFFSET_HZ * 1.5 / df).round() as usize;
        let inner =
            (lo_bin.ceil() as usize + guard)..(hi_bin.floor() as usize).saturating_sub(guard);
        if inner.len() < 16 {
            continue;
        }
        let flatness_db =
            percentile(&smooth[inner.clone()], 0.90) - percentile(&smooth[inner], 0.10);
        if flatness_db > cfg.max_flatness_db {
            continue;
        }
        // The pilot: a CW line on the measured lower edge of the flat band.
        let expect = psd.freq(0) + lo_bin * df;
        let pilot_bb = tone_frequency_iq(
            x,
            fs,
            expect - cfg.pilot_search_hz,
            expect + cfg.pilot_search_hz,
        );
        let Some(pilot_hz_bb) = pilot_bb.value() else {
            continue;
        };
        let pilot_bin = ((pilot_hz_bb - psd.freq(0)) / df).round() as usize;
        let Some(&pilot_db) = db.get(pilot_bin) else {
            continue;
        };
        let pilot_excess_db = pilot_db - plateau_db;
        if pilot_excess_db < cfg.min_pilot_excess_db {
            continue;
        }
        let pilot = shift(&pilot_bb, center_hz);
        let f_lo_hz = pilot_hz_bb + center_hz - PILOT_OFFSET_HZ;
        let grid = us_uhf_channel_at(f_lo_hz, cfg.grid_tolerance_hz);
        let ppm = match grid {
            Some((_, lo)) => ppm_from_line(&pilot, lo + PILOT_OFFSET_HZ),
            None => Estimate::abstain(Method::ClockPpm, Reason::NoCalibration),
        };
        out.push(AtscChannel {
            pilot,
            f_lo_hz,
            f_hi_hz: f_lo_hz + CHANNEL_BANDWIDTH_HZ,
            f_center_hz: f_lo_hz + CHANNEL_BANDWIDTH_HZ / 2.0,
            flat_bandwidth_hz: width,
            plateau_snr_db: plateau_db - floor_db,
            pilot_excess_db,
            flatness_db,
            us_channel: grid.map(|(n, _)| n),
            ppm,
        });
    }
    out.sort_by(|a, b| a.f_lo_hz.total_cmp(&b.f_lo_hz));
    out
}

/// `e` with `offset` added to its value (an absolute frequency from a baseband one).
fn shift(e: &Estimate, offset: f64) -> Estimate {
    match *e {
        Estimate::Measured {
            value,
            sigma,
            evidence,
            ..
        } => {
            Estimate::measured(value + offset, sigma, Method::ToneFrequency).with_evidence(evidence)
        }
        Estimate::Abstained {
            reason, evidence, ..
        } => Estimate::abstain(Method::ToneFrequency, reason).with_evidence(evidence),
    }
}

/// Centred boxcar mean over `2·half + 1` bins, edges shortened.
fn smooth_db(db: &[f64], half: usize) -> Vec<f64> {
    (0..db.len())
        .map(|i| {
            let lo = i.saturating_sub(half);
            let hi = (i + half + 1).min(db.len());
            db[lo..hi].iter().sum::<f64>() / (hi - lo) as f64
        })
        .collect()
}

/// Maximal contiguous runs of bins at or above `threshold`.
fn runs_above(v: &[f64], threshold: f64) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, &x) in v.iter().enumerate() {
        match (x >= threshold, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                out.push(s..i);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push(s..v.len());
    }
    out
}

/// The two half-power (−3 dB from `plateau_db`) crossings of `run`, in fractional bins, found by
/// walking outwards from the run and interpolating linearly across the crossing.
fn half_power_edges(v: &[f64], run: std::ops::Range<usize>, plateau_db: f64) -> Option<(f64, f64)> {
    let target = plateau_db - 3.0;
    let lo = (run.start..run.end).find(|&i| v[i] >= target)?;
    let hi = (run.start..run.end).rev().find(|&i| v[i] >= target)?;
    let interp = |inside: usize, outside: usize| -> f64 {
        let (a, b) = (v[outside], v[inside]);
        if (b - a).abs() < f64::EPSILON {
            inside as f64
        } else {
            outside as f64 + (target - a) / (b - a) * (inside as f64 - outside as f64)
        }
    };
    let lo_edge = if lo == 0 {
        lo as f64
    } else {
        interp(lo, lo - 1)
    };
    let hi_edge = if hi + 1 >= v.len() {
        hi as f64
    } else {
        interp(hi, hi + 1)
    };
    (hi_edge > lo_edge).then_some((lo_edge, hi_edge))
}

/// Linear-interpolation-free percentile of a slice (nearest rank).
fn percentile(v: &[f64], q: f64) -> f64 {
    if v.is_empty() {
        return f64::NEG_INFINITY;
    }
    let mut s: Vec<f64> = v.to_vec();
    s.sort_by(f64::total_cmp);
    let i = ((s.len() - 1) as f64 * q).round() as usize;
    s[i]
}

fn median(v: &[f64]) -> f64 {
    percentile(v, 0.5)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_dsp::fft::{CpuFft, FftBackend};
    use std::f64::consts::TAU;

    /// A deterministic xorshift, so the synthetic plateau is the same on every machine.
    struct Rng(u64);
    impl Rng {
        fn next_f64(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
        /// Box–Muller, one of the pair.
        fn normal(&mut self) -> f64 {
            let u = self.next_f64().max(1e-12);
            let v = self.next_f64();
            (-2.0 * u.ln()).sqrt() * (TAU * v).cos()
        }
    }

    /// An 8VSB-like channel: a flat band of band-limited noise plus the CW pilot at its lower
    /// edge, in white noise.
    ///
    /// The plateau is synthesised in the frequency domain — random-phase unit bins across exactly
    /// the Nyquist band, inverse-transformed — which is what an 8VSB spectrum looks like once the
    /// symbols are gone. The recogniser measures the spectrum, so this is the shape it must answer
    /// to, and nothing about it presumes 470 MHz.
    fn plateau(
        n: usize,
        fs: f64,
        flat_lo_bb: f64,
        width_hz: f64,
        amp: f64,
        seed: u64,
    ) -> Vec<Complex32> {
        let mut rng = Rng(seed);
        let df = fs / n as f64;
        let bin = |f: f64| ((f / df).round() as i64).rem_euclid(n as i64) as usize;
        let mut spec = vec![Complex32::default(); n];
        let k0 = (flat_lo_bb / df).round() as i64;
        let k1 = ((flat_lo_bb + width_hz) / df).round() as i64;
        let lines = (k1 - k0).max(1) as f64;
        let a = (amp * (n as f64) / lines.sqrt()) as f32;
        for k in k0..k1 {
            let ph = rng.next_f64() * TAU;
            spec[bin(k as f64 * df)] = Complex32::new(a * ph.cos() as f32, a * ph.sin() as f32);
        }
        // Inverse transform by the conjugate trick (hk-dsp plans forward transforms only).
        for s in spec.iter_mut() {
            *s = s.conj();
        }
        let mut fft = CpuFft::new(n);
        fft.forward(&mut spec);
        let scale = 1.0 / n as f32;
        spec.iter().map(|s| s.conj() * scale).collect()
    }

    /// `plateau` plus white noise and the pilot, all in linear amplitude.
    fn scene(
        n: usize,
        fs: f64,
        flat_lo_bb: f64,
        noise_db: f64,
        plateau_db: f64,
        pilot_db: f64,
        seed: u64,
    ) -> Vec<Complex32> {
        let mut rng = Rng(seed ^ 0x9e37_79b9);
        let noise_a = 10f64.powf(noise_db / 20.0);
        let pilot_a = 10f64.powf(pilot_db / 20.0);
        let mut x = plateau(
            n,
            fs,
            flat_lo_bb,
            FLAT_BANDWIDTH_HZ,
            10f64.powf(plateau_db / 20.0),
            seed,
        );
        for (i, s) in x.iter_mut().enumerate() {
            let a = TAU * flat_lo_bb / fs * i as f64;
            *s += Complex32::new(
                (noise_a * rng.normal() + pilot_a * a.cos()) as f32,
                (noise_a * rng.normal() + pilot_a * a.sin()) as f32,
            );
        }
        x
    }

    const FS: f64 = 8e6;
    const N: usize = 1 << 16;

    /// The pilot's offset is a *derived* constant, not a typed-in number: it must equal the
    /// 309.440 559 kHz of ATSC A/53 Part 2 §5.1.2 to the Hz.
    #[test]
    fn standard_constants_match_a53() {
        assert!(
            (SYMBOL_RATE_BD - 10_762_237.762_237_762).abs() < 1e-3,
            "{SYMBOL_RATE_BD}"
        );
        assert!(
            (FLAT_BANDWIDTH_HZ - 5_381_118.881_118_881).abs() < 1e-3,
            "{FLAT_BANDWIDTH_HZ}"
        );
        assert!(
            (PILOT_OFFSET_HZ - 309_440.559_440_559).abs() < 1e-3,
            "{PILOT_OFFSET_HZ}"
        );
    }

    /// The US UHF grid: channel 14 starts at 470 MHz, 36 ends at 608 MHz, and 37 is not on it.
    #[test]
    fn us_uhf_grid() {
        assert_eq!(us_uhf_channel_lo_hz(14), Some(470e6));
        assert_eq!(us_uhf_channel_lo_hz(29), Some(560e6));
        assert_eq!(us_uhf_channel_lo_hz(36), Some(602e6));
        assert_eq!(us_uhf_channel_lo_hz(37), None);
        assert_eq!(us_uhf_channel_lo_hz(13), None);
        assert_eq!(us_uhf_channel_at(470.0024e6, 50e3), Some((14, 470e6)));
        assert_eq!(us_uhf_channel_at(470.06e6, 50e3), None, "off the grid");
    }

    /// The whole point: one 8VSB-like channel is **one** recognised channel, 6 MHz wide, whose
    /// pilot is measured to a fraction of a bin and whose flat band measures 5.38 MHz.
    #[test]
    fn one_plateau_and_pilot_is_one_six_megahertz_channel() {
        // Channel 14 seen from a receiver tuned to 473 MHz (the channel centre).
        let center = 473e6;
        let flat_lo_bb = 470e6 + PILOT_OFFSET_HZ - center;
        let x = scene(N, FS, flat_lo_bb, -50.0, -20.0, -26.0, 0x5eed);
        let got = find_channels(&x, FS, center, &AtscConfig::default());
        assert_eq!(got.len(), 1, "{got:#?}");
        let c = &got[0];
        assert!(
            (c.f_lo_hz - 470e6).abs() < 2e3,
            "lower edge {} Hz",
            c.f_lo_hz
        );
        assert!((c.f_hi_hz - c.f_lo_hz - 6e6).abs() < 1.0);
        assert!(
            (c.flat_bandwidth_hz - FLAT_BANDWIDTH_HZ).abs() < 150e3,
            "flat band {} Hz",
            c.flat_bandwidth_hz
        );
        assert_eq!(c.us_channel, Some(14));
        let pilot = c.pilot.value().expect("a measured pilot");
        assert!(
            (pilot - (470e6 + PILOT_OFFSET_HZ)).abs() < 2e3,
            "pilot {pilot} Hz"
        );
    }

    /// A receiver whose oscillator is 4 ppm fast reads every frequency 4 ppm low, and the pilot
    /// says so — the explorer's "≈ 2.4 kHz low at 470–602 MHz" (T-979).
    #[test]
    fn the_pilot_measures_the_receiver_clock() {
        let center = 473e6;
        let ppm = -4.0;
        let true_lo = 470e6;
        let read_lo = true_lo * (1.0 + ppm * 1e-6);
        let flat_lo_bb = read_lo + PILOT_OFFSET_HZ - center;
        let x = scene(N, FS, flat_lo_bb, -50.0, -20.0, -26.0, 0xa11ce);
        let got = find_channels(&x, FS, center, &AtscConfig::default());
        assert_eq!(got.len(), 1, "{got:#?}");
        let c = &got[0];
        assert_eq!(c.us_channel, Some(14), "still on the grid at 4 ppm");
        let offset = c.pilot_offset_hz().expect("an offset against the nominal");
        assert!(
            (offset - (-1.88e3)).abs() < 600.0,
            "pilot offset {offset} Hz (470 MHz × −4 ppm ≈ −1.88 kHz)"
        );
        let read = c.ppm.value().expect("a ppm reading");
        assert!((read - ppm).abs() < 1.5, "read {read} ppm, injected {ppm}");
    }

    /// Nothing is looked up: the same emission 700 MHz away from any US TV channel is still
    /// recognised as an 8VSB channel — it just gets no channel number and no ppm, because a ppm
    /// needs a nominal.
    #[test]
    fn recognition_is_blind_and_the_grid_only_names_it() {
        let center = 1_303e6;
        let flat_lo_bb = 1_300e6 + PILOT_OFFSET_HZ - center;
        let x = scene(N, FS, flat_lo_bb, -50.0, -20.0, -26.0, 0xb0b);
        let got = find_channels(&x, FS, center, &AtscConfig::default());
        assert_eq!(got.len(), 1, "{got:#?}");
        assert_eq!(got[0].us_channel, None);
        assert!(!got[0].ppm.is_measured(), "no nominal, no ppm");
        assert!((got[0].f_lo_hz - 1_300e6).abs() < 2e3);
    }

    /// Noise alone is not a channel.
    #[test]
    fn empty_spectrum_recognises_nothing() {
        let mut rng = Rng(7);
        let x: Vec<Complex32> = (0..N)
            .map(|_| Complex32::new((0.003 * rng.normal()) as f32, (0.003 * rng.normal()) as f32))
            .collect();
        assert!(find_channels(&x, FS, 500e6, &AtscConfig::default()).is_empty());
    }

    /// A 5.38 MHz plateau with **no pilot** is not called ATSC: the pilot is the evidence, and
    /// without it the recogniser abstains rather than guessing from width alone.
    #[test]
    fn a_plateau_without_a_pilot_is_not_atsc() {
        let center = 473e6;
        let flat_lo_bb = 470e6 + PILOT_OFFSET_HZ - center;
        let x = scene(N, FS, flat_lo_bb, -50.0, -20.0, -300.0, 0xc0ffee);
        assert!(find_channels(&x, FS, center, &AtscConfig::default()).is_empty());
    }

    /// A narrowband continuous carrier of broadcast-FM width inside the TV band is not an 8VSB
    /// channel — the shape test is on the whole 5.38 MHz, which is why the fragments the explorer
    /// saw could never have been merged by width alone.
    #[test]
    fn a_narrowband_carrier_in_the_tv_band_is_not_atsc() {
        // 200 kHz of continuous energy — the broadcast-FM channel shape, and the width of the
        // fragments the explorer's detector produced inside channels 29/30.
        let mut x = plateau(N, FS, -100e3, 200e3, 10f64.powf(-20.0 / 20.0), 0xd00d);
        let mut rng = Rng(0xd00d);
        for s in x.iter_mut() {
            *s += Complex32::new((0.003 * rng.normal()) as f32, (0.003 * rng.normal()) as f32);
        }
        assert!(find_channels(&x, FS, 473e6, &AtscConfig::default()).is_empty());
    }
}
