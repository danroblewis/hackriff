//! The broadcast-FM **multiplex** measurement and the pre-classification rule it feeds (T-970).
//!
//! # Why a rule, and not another density dimension
//!
//! The cascade in [`crate::classifier`] scores a snippet against class-conditional densities over
//! `features@N`, and every one of those dimensions is a *statistic of the waveform's shape*. A
//! broadcast FM station carries something far more specific than a shape: a **19 kHz stereo pilot
//! tone**, and — where the station transmits RDS — a **57 kHz suppressed-carrier subcarrier** at
//! exactly three times it, both of them in the FM discriminator's output rather than in the RF
//! spectrum. Nothing else in `hk-mod@1` produces a pure 19 kHz line under FM discrimination, so
//! the measurement is not a weak piece of evidence to be fused — it is decisive on its own, and it
//! is decisive at SNRs where the density cascade's gates hold every family back.
//!
//! That last clause is the defect this module fixes. The explorer's 2026-09-25 window measured
//! stations with a **locked pilot and a CRC-valid RDS decode** coming back `unknown` at confidence
//! 0.999, because C13 either could not measure an in-band SNR at all (`no_snr`) or measured one
//! under `analog`'s 10 dB gate (`low_snr`) — and ADR-0016 §2 rightly puts an unmeasured family's
//! share on `unknown` rather than on a neighbour. The gate is about the *densities*: it says the
//! shape statistics are not trustworthy there. It says nothing about a tone at a known frequency,
//! whose detectability is set by the integration time and not by the in-band SNR of the whole
//! 200 kHz channel. So the rule runs **beside** the gate, on its own measurement, and its claim is
//! priced by that measurement's own false-alarm probability.
//!
//! # Never the band plan
//!
//! Everything here is measured from the samples: the emission's occupied bandwidth, the
//! discriminator's RMS deviation, and three spectral tests on the multiplex. No frequency is
//! looked up, no allocation is consulted, and the rule fires identically on a station 150 kHz off
//! the 200 kHz raster. The band plan remains an **explanation** offered afterwards by C17
//! ([`crate::fuse`]), which this rule does not read and cannot be reached by.
//!
//! # What is measured
//!
//! The snippet is FM-discriminated (`arg(x[n]·conj(x[n−1]))·fs/2π`, Hz), then a Welch periodogram
//! with **non-overlapping** Hann segments is taken over the result, so the `K` segments are
//! independent and a bin is `θ·χ²_{2K}/(2K)` under the no-line null. Three tests read it:
//!
//! | test | statistic | null band |
//! |---|---|---|
//! | pilot | peak bin in 19 kHz ± [`PILOT_SEARCH_HZ`] | median of 16–22 kHz outside the search |
//! | RDS | mean of 57 kHz ± [`RDS_HALF_HZ`] | median of [`UPPER_NULL_LO_HZ`]–[`UPPER_NULL_HI_HZ`] |
//! | stereo L−R | mean of 24–53 kHz | the same upper null |
//!
//! Each returns a **false-alarm probability**: the chance the null alone produced a statistic that
//! large, with the pilot's corrected for searching [`PILOT_SEARCH_HZ`]'s worth of bins. Those
//! probabilities are what the rule's confidence is built from, so the number a `Classification`
//! carries is the measurement's own calibration rather than a tuned constant.

use num_complex::Complex32;

use hk_dsp::{WelchConfig, WindowKind, welch};

use crate::openset::chi2_sf;

/// Stereo pilot, Hz. Fixed by broadcast regulation at 19 kHz ± 2 Hz, but the rule only needs it to
/// be *some* line inside [`PILOT_SEARCH_HZ`] of it, so a receiver clock error of tens of ppm and a
/// station at the edge of tolerance both still fire.
pub const PILOT_HZ: f64 = 19_000.0;
/// Half-width of the pilot search, Hz. Wide enough for any receiver-clock error the HackRF shows
/// (the 2026-09-15 capture's own clock runs −6.8 ppm, i.e. 0.13 Hz at 19 kHz) and for the
/// discriminator's own frequency resolution; the false-alarm probability is corrected for every
/// bin searched, so widening it costs confidence rather than buying it.
pub const PILOT_SEARCH_HZ: f64 = 400.0;
/// The pilot's null band, Hz: mono audio stops at 15 kHz and the L−R band starts at 23 kHz, so
/// 16–22 kHz is the discriminator's own floor either side of the tone.
pub const PILOT_NULL: (f64, f64) = (16_000.0, 22_000.0);
/// RDS subcarrier, Hz: 3 × [`PILOT_HZ`], suppressed carrier, so this is a **band** power test and
/// never a line test.
pub const RDS_HZ: f64 = 57_000.0;
/// Half-width of the RDS band, Hz (1187.5 bd biphase: the main lobes reach ±2.4 kHz, the core
/// energy ±1.2 kHz).
pub const RDS_HALF_HZ: f64 = 1_600.0;
/// The L−R stereo subcarrier band, Hz: DSB-SC about 38 kHz.
pub const STEREO_BAND: (f64, f64) = (24_000.0, 53_000.0);
/// The null band above the multiplex, Hz. A standard MPX carries nothing above 60 kHz except the
/// optional SCA/DARC subcarriers at 67 and 76 kHz, which a **median** over this band ignores.
pub const UPPER_NULL_LO_HZ: f64 = 60_000.0;
/// See [`UPPER_NULL_LO_HZ`].
pub const UPPER_NULL_HI_HZ: f64 = 80_000.0;

/// Half-width of the harmonic search, Hz: a comb tooth sits at exactly twice or three times the
/// line, so the window only has to cover the transform's own resolution.
const HARMONIC_SEARCH_HZ: f64 = 150.0;
/// Half-width of the harmonic null, Hz. Inside a station's own subcarrier bands, so a suppressed
/// subcarrier's smooth shoulder is the floor a tooth would have to stand above.
const HARMONIC_NULL_HZ: f64 = 500.0;

/// Target periodogram bin width, Hz. 100 Hz resolves the pilot as a line (its own width after the
/// discriminator is a few Hz) while keeping the segment count high enough for a tight null.
const TARGET_BIN_HZ: f64 = 100.0;
/// Largest transform the measurement will use.
const MAX_FFT_LEN: usize = 16_384;
/// Fewest independent segments the null estimate is trusted over.
const MIN_SEGMENTS: usize = 8;
/// Longest stretch of the snippet the measurement reads, seconds. The pilot is a steady tone, so
/// beyond this the test only trades wall time for a null it already has (see [`MIN_SEGMENTS`]).
const MAX_ANALYSIS_S: f64 = 0.5;

/// Lowest sample rate the multiplex can be measured at, Hz: the upper null band has to fit under
/// Nyquist with a margin.
pub const MIN_MPX_RATE_HZ: f64 = 2.0 * (UPPER_NULL_HI_HZ + 5_000.0);

/// What the FM multiplex of one snippet looks like. Every field is a measurement or an abstention;
/// nothing here is a threshold decision (that is [`wfm_rule`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MpxEvidence {
    /// RMS of the discriminator output, Hz — the emission's frequency deviation.
    pub deviation_rms_hz: Option<f64>,
    /// Frequency of the strongest bin inside the pilot search, Hz.
    pub pilot_hz: Option<f64>,
    /// That bin over the pilot null band's floor, dB.
    pub pilot_db: Option<f64>,
    /// Probability the pilot null alone produced a peak that large **anywhere in the search**.
    pub pilot_p_fa: Option<f64>,
    /// The 57 kHz band's mean over the upper null's floor, dB.
    pub rds_db: Option<f64>,
    /// Probability the upper null alone produced a 57 kHz band that strong.
    pub rds_p_fa: Option<f64>,
    /// The L−R band's mean over the upper null's floor, dB.
    pub stereo_db: Option<f64>,
    /// Probability the upper null alone produced an L−R band that strong.
    pub stereo_p_fa: Option<f64>,
    /// The strongest **line** at twice or three times the measured pilot, over its own local
    /// floor, dB. A broadcast multiplex suppresses both subcarriers, so this is ≈ 0 dB for a
    /// station and large for any emission whose discriminator is a harmonic comb.
    pub harmonic_line_db: Option<f64>,
    /// Independent Welch segments the null was estimated over.
    pub segments: usize,
}

impl MpxEvidence {
    /// Whether the multiplex could be measured at all.
    pub fn measured(&self) -> bool {
        self.segments >= MIN_SEGMENTS
    }
}

/// Measures the FM multiplex of a normalised snippet. `None` when the snippet cannot carry one —
/// too low a sample rate to see 57 kHz, or too few samples for an independent null.
///
/// The snippet is the classifier's own input: CFO-corrected and power-normalised, which is exactly
/// the geometry an FM discriminator wants.
pub fn mpx_evidence(samples: &[Complex32], sample_rate_hz: f64) -> Option<MpxEvidence> {
    if !sample_rate_hz.is_finite() || sample_rate_hz < MIN_MPX_RATE_HZ {
        return None;
    }
    let fft_len = ((sample_rate_hz / TARGET_BIN_HZ) as usize)
        .next_power_of_two()
        .clamp(1024, MAX_FFT_LEN);
    let want = ((MAX_ANALYSIS_S * sample_rate_hz) as usize).max(fft_len * MIN_SEGMENTS);
    let n = samples.len().min(want);
    if n < fft_len * MIN_SEGMENTS + 1 {
        return None;
    }

    // FM discrimination: the phase advance per sample, in Hz. This is the multiplex.
    let scale = sample_rate_hz / std::f64::consts::TAU;
    let disc: Vec<Complex32> = samples[..n]
        .windows(2)
        .map(|w| {
            let d = w[1] * w[0].conj();
            Complex32::new((f64::from(d.arg()) * scale) as f32, 0.0)
        })
        .collect();
    let deviation_rms_hz = {
        let sum: f64 = disc.iter().map(|d| f64::from(d.re) * f64::from(d.re)).sum();
        (sum / disc.len() as f64).sqrt()
    };

    // Non-overlapping segments, so `K` really is the number of independent looks and the χ² tail
    // below is the tail of the statistic actually computed.
    let cfg = WelchConfig {
        fft_len,
        overlap: 0,
        window: WindowKind::Hann,
        holds: false,
        spectral_kurtosis: false,
    };
    let segments = disc.len() / fft_len;
    if segments < MIN_SEGMENTS {
        return None;
    }
    let s = welch(&disc, sample_rate_hz, 0.0, &cfg).ok()?;
    let psd: Vec<f64> = s.psd.iter().map(|v| f64::from(*v)).collect();
    let bin_hz = s.bin_width_hz();
    // The discriminator output is real, so the periodogram is conjugate-symmetric; only the
    // positive half is read.
    let at = |f: f64| -> Option<usize> {
        let b = s.center_bin() as f64 + f / bin_hz;
        let b = b.round();
        (b >= 0.0 && b < psd.len() as f64).then_some(b as usize)
    };
    let band = |lo: f64, hi: f64| -> Vec<f64> {
        let (a, b) = (at(lo), at(hi));
        match (a, b) {
            (Some(a), Some(b)) if b > a => psd[a..=b].to_vec(),
            _ => Vec::new(),
        }
    };

    let mut ev = MpxEvidence {
        deviation_rms_hz: Some(deviation_rms_hz),
        segments,
        ..MpxEvidence::default()
    };

    // A line test: the strongest bin within `half_hz` of `centre_hz`, against the median of the
    // null band either side of the search. Returns (frequency, dB over the null, p_fa).
    let line = |centre_hz: f64, half_hz: f64, null: (f64, f64)| -> Option<(f64, f64, f64)> {
        let (lo, hi) = (at(centre_hz - half_hz)?, at(centre_hz + half_hz)?);
        let floor: Vec<f64> = (at(null.0)?..=at(null.1)?)
            .filter(|i| *i < psd.len() && !(lo..=hi).contains(i))
            .map(|i| psd[i])
            .collect();
        let floor = median(&floor).filter(|f| *f > 0.0)?;
        let (peak_bin, peak) = (lo..=hi)
            .map(|i| (i, psd[i]))
            .fold((lo, 0.0_f64), |a, b| if b.1 > a.1 { b } else { a });
        let theta = floor / median_factor(2 * segments);
        Some((
            s.bin_offset_hz(peak_bin),
            10.0 * (peak / theta).log10(),
            search_p_fa(peak / theta, segments, 1, hi - lo + 1),
        ))
    };

    // --- pilot: a line test against its own local floor.
    if let Some((f, db, p)) = line(PILOT_HZ, PILOT_SEARCH_HZ, PILOT_NULL) {
        ev.pilot_hz = Some(f);
        ev.pilot_db = Some(db);
        ev.pilot_p_fa = Some(p);
    }

    // --- harmonic suppression: **the** test that separates a stereo pilot from a comb.
    //
    // A broadcast multiplex's 38 kHz stereo subcarrier and 57 kHz RDS subcarrier are both
    // **suppressed** by design (ITU-R BS.450 / EN 50067: DSB-SC, carrier down 40 dB and more), so
    // the only line in the multiplex is the pilot itself. An emission whose discriminator output
    // is a periodic sawtooth or staircase — a repeating chirp, a Costas hop — produces a harmonic
    // **comb**, and a comb tooth can land in the pilot search just as well as a pilot can. The two
    // are told apart by what is at twice and three times the line: nothing, or two more teeth.
    // Measured on the dev grid at 25 dB, the synthetic broadcast multiplex reads ≈ 0 dB here and
    // the repeating chirp and Costas hop read tens of dB.
    if let Some(pilot_hz) = ev.pilot_hz.filter(|f| *f > 0.0) {
        let mut worst = f64::NEG_INFINITY;
        for n in [2.0_f64, 3.0] {
            let c = n * pilot_hz;
            if let Some((_, db, _)) = line(
                c,
                HARMONIC_SEARCH_HZ,
                (c - HARMONIC_NULL_HZ, c + HARMONIC_NULL_HZ),
            ) {
                worst = worst.max(db);
            }
        }
        if worst.is_finite() {
            ev.harmonic_line_db = Some(worst);
        }
    }

    // --- the upper null, shared by the two suppressed-carrier band tests.
    let upper = band(UPPER_NULL_LO_HZ, UPPER_NULL_HI_HZ);
    if let Some(floor) = median(&upper).filter(|f| *f > 0.0) {
        let theta = floor / median_factor(2 * segments);
        let band_test = |lo: f64, hi: f64| -> Option<(f64, f64)> {
            let bins = band(lo, hi);
            if bins.is_empty() {
                return None;
            }
            let mean = bins.iter().sum::<f64>() / bins.len() as f64;
            Some((
                10.0 * (mean / theta).log10(),
                search_p_fa(mean / theta, segments, bins.len(), 1),
            ))
        };
        if let Some((db, p)) = band_test(RDS_HZ - RDS_HALF_HZ, RDS_HZ + RDS_HALF_HZ) {
            ev.rds_db = Some(db);
            ev.rds_p_fa = Some(p);
        }
        if let Some((db, p)) = band_test(STEREO_BAND.0, STEREO_BAND.1) {
            ev.stereo_db = Some(db);
            ev.stereo_p_fa = Some(p);
        }
    }
    Some(ev)
}

/// Median of a slice, or `None` when it is empty.
fn median(xs: &[f64]) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    let mut v = xs.to_vec();
    v.sort_by(f64::total_cmp);
    Some(v[v.len() / 2])
}

/// Median of `χ²_ν/ν`, the factor by which a median underestimates the mean of a Welch bin
/// (Wilson–Hilferty). Dividing the measured median by it recovers the null's mean, so the χ² tail
/// below is not quietly anti-conservative.
fn median_factor(dof: usize) -> f64 {
    let v = dof as f64;
    (1.0 - 2.0 / (9.0 * v)).powi(3)
}

/// The probability that the null alone produced a statistic at least `ratio` times its own mean,
/// when the statistic averages `m_bins` bins of `k` independent segments and `n_search` such
/// statistics were searched over.
fn search_p_fa(ratio: f64, k: usize, m_bins: usize, n_search: usize) -> f64 {
    if !ratio.is_finite() || ratio <= 0.0 {
        return 1.0;
    }
    let dof = 2 * k * m_bins;
    let p = chi2_sf(ratio * dof as f64, dof);
    // 1 − (1 − p)^n, written so it stays exact for the tiny p this test lives in. A non-finite
    // result is reported as "certainly the null", never as evidence.
    let fa = (-(n_search as f64) * (-p).ln_1p()).exp_m1();
    if fa.is_finite() {
        fa.clamp(0.0, 1.0)
    } else {
        1.0
    }
}

/// A broadcast FM emission occupies 120–200 kHz inside its 200 kHz channel; the rule accepts a
/// wider window than that so a detection box measured a little loose or a little tight still
/// qualifies, because the *decisive* evidence is the multiplex, not the width.
pub const STATION_OBW_HZ: (f64, f64) = (90e3, 340e3);
/// Smallest RMS deviation, Hz, that is an FM broadcast multiplex rather than a narrowband mode
/// sharing the window. Broadcast peak deviation is 75 kHz; the RMS over programme material and
/// subcarriers sits well above this floor, while NBFM at 2.5–5 kHz deviation sits far below it.
pub const MIN_DEVIATION_RMS_HZ: f64 = 4e3;

/// Largest false-alarm probability the pilot test may have for the rule to fire. At this level a
/// run over every 200 kHz window of the whole 88–108 MHz band (100 windows) expects far less than
/// one false pilot, which is the population the rule actually meets.
pub const PILOT_MAX_P_FA: f64 = 1e-9;
/// Largest false-alarm probability for the 57 kHz band to count as RDS present.
pub const RDS_MAX_P_FA: f64 = 1e-6;
/// Largest false-alarm probability for the L−R band to count as stereo present.
pub const STEREO_MAX_P_FA: f64 = 1e-6;
/// How far the pilot must stand above the strongest line at twice or three times it, dB, for the
/// line to be a suppressed-subcarrier multiplex's pilot rather than one tooth of a harmonic comb
/// (see [`MpxEvidence::harmonic_line_db`]).
///
/// **The threshold is a physical constant, not a tuned one.** An emission whose discriminator
/// output is periodic — a repeating linear chirp, a Costas hop — produces a sawtooth or staircase,
/// and the harmonics of such a waveform fall as `1/n`, so its second harmonic is
/// `20·log10(2) = 6.02 dB` down and its third `9.54 dB`. Whichever tooth lands in the pilot
/// search, the strongest of its own 2× and 3× teeth is therefore about **6 dB** below it, whatever
/// the SNR. Measured on the dev grid over 4-25 dB: the repeating chirp reads 5.6-5.8 dB and the
/// held-out Costas hop 5.6-5.8 dB — flat, as the `1/n` law says — while the broadcast multiplex,
/// whose 38 kHz and 57 kHz subcarriers are suppressed by construction, reads 11.9-22.8 dB. The
/// threshold sits in that gap.
pub const HARMONIC_SUPPRESSION_DB: f64 = 10.0;

/// The most confidence the rule may claim with the pilot **and** a 57 kHz subcarrier.
///
/// The measurement's own false-alarm probability is many orders below this; what the ceiling
/// prices is the step from *"this snippet's discriminator carries a 19 kHz tone and a 57 kHz
/// suppressed-carrier band"* to the **label** `analog`/`wfm` — the rule's own risk of being
/// pointed at something that is not a station, which no χ² tail can measure. ADR-0016's
/// "nothing is certain" applies to the label, not to the tone.
pub const WFM_PILOT_RDS_CONFIDENCE: f64 = 0.97;
/// The ceiling with the pilot alone (no 57 kHz subcarrier and no measurable L−R band): the same
/// tone, one fewer independent corroboration of what it belongs to.
pub const WFM_PILOT_CONFIDENCE: f64 = 0.90;

/// What [`wfm_rule`] concluded.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WfmCall {
    /// Posterior the rule claims for `analog`/`wfm`.
    pub confidence: f64,
    /// Machine reason: `wfm_pilot_rds`, `wfm_pilot_stereo` or `wfm_pilot`.
    pub reason: &'static str,
}

/// The pre-classification rule: is this a broadcast FM station?
///
/// It fires only on all of
/// - a station-shaped occupied bandwidth ([`STATION_OBW_HZ`]),
/// - a deviation an FM broadcast multiplex has ([`MIN_DEVIATION_RMS_HZ`]),
/// - a 19 kHz pilot whose false-alarm probability is below [`PILOT_MAX_P_FA`],
///
/// and then claims more where a 57 kHz RDS subcarrier or an L−R band corroborates it. `obw_hz` is
/// C13's measurement of the emission, never a channel raster.
///
/// **A mono station has no pilot**, so the rule stays silent on one and the density cascade
/// answers alone. That is a limit of the rule, not a claim about the emission: `None` here means
/// "this rule has nothing to say", never "not a station".
pub fn wfm_rule(obw_hz: Option<f64>, ev: &MpxEvidence) -> Option<WfmCall> {
    let obw = obw_hz?;
    if !(STATION_OBW_HZ.0..=STATION_OBW_HZ.1).contains(&obw) {
        return None;
    }
    if ev.deviation_rms_hz? < MIN_DEVIATION_RMS_HZ {
        return None;
    }
    let pilot_p = ev.pilot_p_fa?;
    if pilot_p > PILOT_MAX_P_FA {
        return None;
    }
    // The pilot's own harmonics have to be absent, or this is a comb and not a multiplex.
    if ev.pilot_db? - ev.harmonic_line_db? < HARMONIC_SUPPRESSION_DB {
        return None;
    }
    let rds = ev.rds_p_fa.is_some_and(|p| p <= RDS_MAX_P_FA);
    let stereo = ev.stereo_p_fa.is_some_and(|p| p <= STEREO_MAX_P_FA);
    let (ceiling, reason) = match (rds, stereo) {
        (true, _) => (WFM_PILOT_RDS_CONFIDENCE, "wfm_pilot_rds"),
        (false, true) => (WFM_PILOT_RDS_CONFIDENCE, "wfm_pilot_stereo"),
        (false, false) => (WFM_PILOT_CONFIDENCE, "wfm_pilot"),
    };
    // The measurement's own calibration, held under the ceiling the label risk sets.
    let measured = 1.0 - pilot_p.max(f64::MIN_POSITIVE);
    Some(WfmCall {
        confidence: measured.min(ceiling),
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::{Class, SynthConfig, generate};

    /// A snippet long enough for the measurement's own null: the pilot test needs
    /// [`MIN_SEGMENTS`] independent Welch segments at [`TARGET_BIN_HZ`], i.e. ~80 ms of a
    /// continuous emission. The dev grid's default 16 ms snippet is deliberately *not* enough,
    /// and abstaining there is the honest answer.
    const LONG: usize = 300_000;

    fn evidence(class: Class, snr_db: f64, seed: u64) -> (Option<f64>, Option<MpxEvidence>) {
        let mut cfg = SynthConfig::new(snr_db, seed);
        cfg.samples = LONG;
        let s = generate(class, &cfg);
        (Some(s.obw_hz), mpx_evidence(&s.samples, s.sample_rate_hz))
    }

    #[test]
    fn a_synthetic_broadcast_multiplex_shows_its_pilot_and_rds_subcarrier() {
        let (obw, ev) = evidence(Class::Wfm, 25.0, 4_001);
        let ev = ev.expect("a 0.9 s broadcast multiplex is measurable");
        assert!(ev.measured(), "{ev:?}");
        let pilot = ev.pilot_hz.expect("pilot searched");
        assert!(
            (pilot - PILOT_HZ).abs() <= PILOT_SEARCH_HZ,
            "pilot at {pilot} Hz: {ev:?}"
        );
        assert!(ev.pilot_p_fa.unwrap() <= PILOT_MAX_P_FA, "{ev:?}");
        assert!(ev.rds_p_fa.unwrap() <= RDS_MAX_P_FA, "{ev:?}");
        assert!(
            ev.deviation_rms_hz.unwrap() >= MIN_DEVIATION_RMS_HZ,
            "{ev:?}"
        );
        let call = wfm_rule(obw, &ev).expect("the rule fires on a broadcast multiplex");
        assert_eq!(call.reason, "wfm_pilot_rds");
        assert!(call.confidence <= WFM_PILOT_RDS_CONFIDENCE);
        assert!(call.confidence > 0.9, "{call:?}");
    }

    /// The rule's safety property, and the reason it may sit beside the SNR gate at all: nothing
    /// else in `hk-mod@1` — nor any of the held-out generators outside it — carries a 19 kHz line
    /// under FM discrimination, so nothing else may be called a station by it.
    #[test]
    fn no_other_modulation_fires_the_rule() {
        let mut fired = Vec::new();
        let mut measured = 0;
        for class in Class::TAXONOMY.iter().chain(Class::HELD_OUT) {
            if *class == Class::Wfm {
                continue;
            }
            let (obw, ev) = evidence(*class, 25.0, 4_101);
            let Some(ev) = ev else { continue };
            measured += 1;
            if wfm_rule(obw, &ev).is_some() {
                fired.push((class.label(), ev));
            }
        }
        assert!(
            measured >= 10,
            "only {measured} classes were measurable: the property would be vacuous"
        );
        assert!(fired.is_empty(), "{fired:?}");
    }

    /// The pilot survives the SNR range the gate holds `analog` back over — which is the whole
    /// point of the rule running beside the gate rather than under it.
    /// A repeating chirp and a Costas hop are the two generators whose FM discriminator is
    /// periodic, so they are the ones that put a **comb tooth** in the pilot search. This holds
    /// the `1/n` law [`HARMONIC_SUPPRESSION_DB`] is set from, across the SNR range: the pilot rule
    /// must refuse them at every one, not only where the tooth happens to be weak.
    #[test]
    fn a_harmonic_comb_never_passes_for_a_pilot_at_any_snr() {
        for class in [Class::Chirp, Class::CostasHop] {
            for snr in [4.0, 8.0, 12.0, 16.0, 25.0] {
                let (obw, ev) = evidence(class, snr, 4_101);
                let ev = ev.expect("measurable");
                assert!(
                    wfm_rule(obw, &ev).is_none(),
                    "{} at {snr} dB fired: {ev:?}",
                    class.label()
                );
                if let (Some(p), Some(h)) = (ev.pilot_db, ev.harmonic_line_db) {
                    assert!(
                        p - h < HARMONIC_SUPPRESSION_DB,
                        "{} at {snr} dB: suppression {:.1} dB",
                        class.label(),
                        p - h
                    );
                }
            }
        }
    }

    /// The pilot rule stands **below `analog`'s 10 dB SNR gate and below the 13 dB the `wfm` class
    /// name needs** — which is the whole reason it runs beside the density cascade rather than
    /// under it.
    #[test]
    fn the_pilot_is_still_measurable_below_the_analog_snr_gate() {
        for snr in [8.0, 12.0] {
            let (obw, ev) = evidence(Class::Wfm, snr, 4_101);
            let ev = ev.expect("measurable");
            let call = wfm_rule(obw, &ev).unwrap_or_else(|| panic!("{snr} dB: no call, {ev:?}"));
            assert!(call.confidence > 0.8, "{snr} dB: {call:?}");
        }
    }

    #[test]
    fn a_snippet_that_cannot_carry_a_multiplex_is_an_abstention_not_a_zero() {
        // Nyquist below the 57 kHz subcarrier, and a record too short for an independent null.
        let flat = vec![Complex32::new(0.5, 0.5); 400_000];
        assert!(mpx_evidence(&flat, 48e3).is_none());
        assert!(mpx_evidence(&[Complex32::new(0.5, 0.5); 16], 600e3).is_none());
    }

    #[test]
    fn the_false_alarm_probability_is_calibrated_against_its_own_null() {
        // A statistic at the null's own mean is not evidence of anything.
        assert!(search_p_fa(1.0, 32, 1, 1) > 0.2);
        // and it falls monotonically as the line rises over the floor.
        let mut prev = 1.0;
        for r in [2.0, 4.0, 8.0, 16.0, 32.0] {
            let p = search_p_fa(r, 32, 1, 9);
            assert!(p < prev, "not monotone at {r}");
            prev = p;
        }
        // Searching more bins can only raise the false-alarm probability.
        assert!(search_p_fa(8.0, 32, 1, 9) >= search_p_fa(8.0, 32, 1, 1));
    }
}
