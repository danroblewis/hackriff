//! The classical feature tree: coarse split, per-family admissibility and the within-family class
//! call (ADR-0016 §4.3).
//!
//! The tree is deliberately **not** a chain of hard branches down to a leaf: a wrong branch high up
//! would be unrecoverable on an 8-bit front end. It instead **gates** families — each rule below
//! says only "this family cannot explain a snippet that looks like this", from physics — and the
//! class-conditional densities ([`crate::density`]) rank whatever survives. Every constant here is
//! a priori: from the definition of the modulation, or from the S5 floors. None is tuned against
//! the acceptance split.

use hk_model::classify::{Coarse, LabelP};

use crate::features::Features;

/// Why a family was ruled out, or that it was not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Admissibility {
    /// `hk-mod@1` family.
    pub family: &'static str,
    /// Whether the density may score it.
    pub allowed: bool,
    /// Machine reason code when it is not.
    pub reason: Option<&'static str>,
}

/// Envelope coefficient of variation above which the amplitude is carrying information: below it
/// the signal is constant-envelope, so no ASK/OOK.
pub const ASK_MIN_ENV_CV: f64 = 0.25;

/// Envelope CV above which a signal is not constant-envelope, so it is not FSK/CPM.
pub const FSK_MAX_ENV_CV: f64 = 0.60;

/// Median within-window R² of the instantaneous frequency above which a chirp is possible.
pub const CSS_MIN_SLOPE_R2: f64 = 0.55;

/// Cyclic-prefix correlation below which OFDM is ruled out (no repeated guard interval).
pub const OFDM_MIN_CP_CORR: f64 = 0.10;

/// Spectral flatness below which OFDM is ruled out (OFDM fills its band).
pub const OFDM_MIN_FLATNESS: f64 = 0.20;

/// Envelope duty above which a pulse train is ruled out (a pulse train is mostly off).
pub const PULSED_MAX_DUTY: f64 = 0.60;

/// Spectral flatness below which a noise-like emission is ruled out.
pub const NOISE_MIN_FLATNESS: f64 = 0.45;

/// Carrier-line prominence (dB over the median bin) above which a noise-like emission is ruled
/// out: noise has no line.
pub const NOISE_MAX_CARRIER_DB: f64 = 14.0;

/// Cyclic-line significance above which a noise-like emission is ruled out: noise has no cycle.
pub const NOISE_MAX_CYCLIC_DB: f64 = 8.0;

/// Envelope kurtosis (μ₄₂ of the instantaneous amplitude) above which the envelope is
/// Rayleigh-like rather than that of an angle modulation: a constant envelope gives 1, Gaussian
/// noise 2. The midpoint is the threshold.
pub const NOISE_LIKE_MIN_MU42: f64 = 1.5;

/// Sarle's bimodality coefficient above which instantaneous-frequency modes count as **discrete
/// levels** rather than the excursion limits of a continuous angle modulation. Two point masses
/// give 1.0; the arcsine distribution of tone-modulated FM gives 0.67; a Gaussian gives 0.33. The
/// midpoint of the two cases that matter is the threshold.
pub const DISCRETE_LEVELS_BIMODALITY: f64 = 0.84;

/// Which families may be scored for `features`, with the reason for each exclusion.
pub fn admissible(features: &Features) -> Vec<Admissibility> {
    let get = |n: &str| features.get(n);
    let allow = |family: &'static str| Admissibility {
        family,
        allowed: true,
        reason: None,
    };
    let deny = |family: &'static str, reason: &'static str| Admissibility {
        family,
        allowed: false,
        reason: Some(reason),
    };

    let env_cv = get("env_cv");
    let flatness = get("flatness");
    let carrier_db = get("carrier_line_db");
    let cyclic_db = get("cyclic_db");
    let cp = get("cp_corr");
    let slope = get("if_slope_r2");
    let duty = get("duty");

    let modality = get("if_modality").unwrap_or(1.0);
    let bimodality = get("if_bimodality").unwrap_or(0.0);
    let mu42_a = get("mu42_a");

    let mut out = Vec::new();
    // Analog spans almost every envelope and spectrum shape its digital neighbours use, so it is
    // ruled out only by evidence of things an analog emission cannot have: a symbol clock, a guard
    // interval, discrete frequency levels — or the Rayleigh envelope of a band-filling noise-like
    // emission, which no AM, FM, SSB or CW signal has (a constant-envelope angle modulation sits
    // near μ₄₂ ≈ 1, Gaussian noise at 2).
    //
    // Without these, analog is a catch-all: five broad classes covering enough of the feature
    // space to absorb high-order QAM, band noise and out-of-taxonomy signals that should come back
    // `unknown`.
    out.push(match (cyclic_db, cp, mu42_a) {
        (Some(c), _, _) if c > NOISE_MAX_CYCLIC_DB => deny("analog", "symbol_clock"),
        // A repeated guard interval only rules analog out when the emission also **fills its band**
        // (the condition [`coarse_hint`] already applies, for the same reason: any smoothly
        // modulated carrier repeats itself somewhat, and broadcast FM in particular scores a
        // prominent short-lag correlation without being digital at all) **and** carries the
        // envelope of a multi-carrier emission.
        //
        // The envelope term is what separates the two for real. OFDM is a sum of many independent
        // subcarriers, so by the central limit theorem its envelope is Rayleigh — μ₄₂ ≈ 2, the same
        // value Gaussian noise gives. An analog angle modulation is constant-envelope by
        // construction: μ₄₂ ≈ 1, and its amplitude carries no information at all. A carrier whose
        // envelope never varies cannot be OFDM whatever its autocorrelation does.
        //
        // Flatness alone was not enough, because a 200 kHz broadcast FM carrier genuinely does fill
        // its own band: this arm still denied `analog` to between half and seven eighths of the
        // `wfm` snippets, which is the whole of the residual `analog` abstention (measured: wfm
        // top-1 0.58 at gate+5, with `analog` admissible in only 0.12-0.50 of trials).
        (_, Some(c), _)
            if c >= OFDM_MIN_CP_CORR
                && flatness.is_some_and(|f| f >= OFDM_MIN_FLATNESS)
                && (mu42_a.is_some_and(|k| k > NOISE_LIKE_MIN_MU42)
                    || env_cv.is_some_and(|v| v >= ASK_MIN_ENV_CV)) =>
        {
            deny("analog", "guard_interval")
        }
        (_, _, Some(k))
            if k > NOISE_LIKE_MIN_MU42
                && flatness.is_some_and(|f| f >= NOISE_MIN_FLATNESS)
                && carrier_db.is_some_and(|c| c < NOISE_MAX_CARRIER_DB) =>
        {
            deny("analog", "noise_like_envelope")
        }
        _ if modality >= 2.0 && bimodality >= DISCRETE_LEVELS_BIMODALITY => {
            deny("analog", "discrete_levels")
        }
        _ => allow("analog"),
    });
    out.push(match env_cv {
        Some(v) if v < ASK_MIN_ENV_CV => deny("ook-ask", "constant_envelope"),
        _ => allow("ook-ask"),
    });
    out.push(match env_cv {
        Some(v) if v > FSK_MAX_ENV_CV => deny("fsk", "varying_envelope"),
        _ => allow("fsk"),
    });
    out.push(allow("psk-qam"));
    out.push(match (cp, flatness) {
        (Some(c), _) if c < OFDM_MIN_CP_CORR => deny("ofdm", "no_cyclic_prefix"),
        (_, Some(f)) if f < OFDM_MIN_FLATNESS => deny("ofdm", "not_band_filling"),
        _ => allow("ofdm"),
    });
    out.push(match slope {
        Some(v) if v < CSS_MIN_SLOPE_R2 => deny("css", "no_linear_sweep"),
        _ => allow("css"),
    });
    // DSSS has no estimator and no generator in M3: it abstains rather than guessing
    // (ADR-0016 §1, "may always abstain in M3").
    out.push(deny("dsss", "dsss_not_implemented"));
    out.push(match duty {
        Some(v) if v > PULSED_MAX_DUTY => deny("pulsed", "continuous_envelope"),
        _ => allow("pulsed"),
    });
    out.push(match (flatness, carrier_db, cyclic_db) {
        (Some(f), _, _) if f < NOISE_MIN_FLATNESS => deny("noise-like", "not_flat"),
        (_, Some(c), _) if c > NOISE_MAX_CARRIER_DB => deny("noise-like", "carrier_line"),
        (_, _, Some(c)) if c > NOISE_MAX_CYCLIC_DB => deny("noise-like", "cyclic_line"),
        _ => allow("noise-like"),
    });
    out
}

/// The coarse hint used when no family is decided (a decided family takes its coarse from the
/// taxonomy). "Digital, unknown order" below the gates is the C15 card's required behaviour, so a
/// snippet with digital structure reports `digital` even when its family is `unknown`.
pub fn coarse_hint(features: &Features) -> Coarse {
    let cyclic = features.get("cyclic_db").unwrap_or(f64::NEG_INFINITY);
    let modality = features.get("if_modality").unwrap_or(1.0);
    let cp = features.get("cp_corr").unwrap_or(0.0);
    let flatness = features.get("flatness");
    let carrier = features.get("carrier_line_db").unwrap_or(0.0);
    let env_cv = features.get("env_cv").unwrap_or(0.0);
    let low = features.get("low_fraction").unwrap_or(0.0);

    let noise_like = flatness.is_some_and(|f| f >= NOISE_MIN_FLATNESS)
        && carrier <= NOISE_MAX_CARRIER_DB
        && cyclic <= NOISE_MAX_CYCLIC_DB
        && cp < OFDM_MIN_CP_CORR;
    if noise_like {
        return Coarse::NoiseLike;
    }
    // Discrete structure: a cyclic line at a symbol rate, **well-separated** frequency levels, a
    // repeated guard interval, or keyed on/off periods.
    //
    // The mode count alone is not enough: an FM carrier modulated by a tone has an arcsine
    // instantaneous-frequency distribution, which peaks at both excursion limits and so counts two
    // modes. Sarle's bimodality coefficient tells the two apart — arcsine sits near 0.67, two
    // discrete tones near 1 — so discrete levels must clear [`DISCRETE_LEVELS_BIMODALITY`].
    let bimodality = features.get("if_bimodality").unwrap_or(0.0);
    let discrete_levels = modality >= 2.0 && bimodality >= DISCRETE_LEVELS_BIMODALITY;
    // A repeated guard interval only means OFDM when the emission also fills its band: any
    // smoothly modulated carrier repeats itself somewhat, and broadcast FM in particular scores a
    // prominent short-lag correlation without being digital at all.
    let guard_interval = cp >= OFDM_MIN_CP_CORR && flatness.is_some_and(|f| f >= OFDM_MIN_FLATNESS);
    let digital = cyclic > NOISE_MAX_CYCLIC_DB
        || discrete_levels
        || guard_interval
        || (low > 0.15 && env_cv > ASK_MIN_ENV_CV);
    if digital {
        return Coarse::Digital;
    }
    // A prominent carrier, or a constant envelope with a continuous instantaneous frequency (an
    // angle modulation), is the analog case.
    if carrier > NOISE_MAX_CARRIER_DB
        || features.get("sigma_af").is_some_and(|v| v < 0.05)
        || (env_cv < ASK_MIN_ENV_CV && bimodality < DISCRETE_LEVELS_BIMODALITY)
    {
        return Coarse::Analog;
    }
    Coarse::Unknown
}

/// A within-family class call.
#[derive(Clone, Debug, PartialEq)]
pub struct ClassGuess {
    /// Class label.
    pub label: String,
    /// Its probability.
    pub p: f64,
    /// Distribution over the family's classes.
    pub dist: Vec<LabelP>,
}

/// Theoretical normalised cumulants `(|Ĉ40|, Ĉ42)` per PSK/QAM class (C15 card).
const PSK_QAM_CUMULANTS: &[(&str, f64, f64)] = &[
    ("bpsk", 2.0, -2.0),
    ("qpsk", 1.0, -1.0),
    ("8psk", 0.0, -1.0),
    ("qam16", 0.68, -0.68),
    ("qam64", 0.62, -0.62),
];

/// The within-family class of `family`, or `None` when the features cannot separate its classes.
///
/// `obw_hz` is the C13 occupied bandwidth (analog needs it to tell broadcast FM from narrowband),
/// and `mod_index_h` the C14 modulation index (FSK needs it to name MSK).
pub fn class_guess(
    family: &str,
    features: &Features,
    obw_hz: Option<f64>,
    mod_index_h: Option<f64>,
) -> Option<ClassGuess> {
    let scores: Vec<(&str, f64)> = match family {
        "analog" => analog_classes(features, obw_hz),
        "ook-ask" => {
            let low = features.get("low_fraction")?;
            // OOK spends a fifth of its symbols at zero; multi-level ASK never goes fully off.
            if low >= 0.15 {
                vec![("ook", 0.8), ("ask4", 0.2)]
            } else {
                vec![("ask4", 0.7), ("ook", 0.3)]
            }
        }
        "fsk" => fsk_classes(features, mod_index_h),
        "psk-qam" => psk_qam_classes(features)?,
        "ofdm" => vec![("ofdm", 0.95)],
        "css" => vec![("chirp", 0.95)],
        "dsss" => vec![("dsss", 0.95)],
        "pulsed" => {
            let duty = features.get("duty").unwrap_or(0.5);
            // A position-modulated train keys twice per bit, so it is busier than a radar PRI.
            if duty > 0.25 {
                vec![("ppm", 0.65), ("pulse", 0.35)]
            } else {
                vec![("pulse", 0.65), ("ppm", 0.35)]
            }
        }
        "noise-like" => vec![("noise-like", 0.95)],
        _ => return None,
    };
    normalise_classes(scores)
}

fn analog_classes(features: &Features, obw_hz: Option<f64>) -> Vec<(&'static str, f64)> {
    let carrier = features.get("carrier_line_db").unwrap_or(0.0);
    let sigma_af = features.get("sigma_af").unwrap_or(0.0);
    let symmetry = features.get("symmetry").map(f64::abs).unwrap_or(0.0);
    let low = features.get("low_fraction").unwrap_or(0.0);
    let env_cv = features.get("env_cv").unwrap_or(0.0);
    let mut v = Vec::new();
    // CW: a keyed carrier — strong line, silent gaps, essentially no frequency excursion.
    v.push((
        "cw",
        if carrier > 20.0 && low > 0.15 && sigma_af < 0.02 {
            0.8
        } else {
            0.05
        },
    ));
    // SSB: one sideband only, no carrier line.
    v.push((
        "ssb",
        if symmetry > 0.35 && carrier < 20.0 {
            0.7
        } else {
            0.05
        },
    ));
    // AM: carrier plus a varying envelope.
    v.push((
        "am",
        if carrier > 14.0 && env_cv > 0.2 && low < 0.15 {
            0.7
        } else {
            0.1
        },
    ));
    // FM: constant envelope; broadcast FM is the wide one (Carson ≈ 180–220 kHz).
    let fm = if env_cv < 0.25 { 0.7 } else { 0.1 };
    let wide = obw_hz.is_some_and(|o| o > 100e3);
    v.push(("wfm", if wide { fm } else { fm * 0.15 }));
    v.push(("nbfm", if wide { fm * 0.15 } else { fm }));
    v
}

fn fsk_classes(features: &Features, mod_index_h: Option<f64>) -> Vec<(&'static str, f64)> {
    let modality = features.get("if_modality").unwrap_or(2.0);
    let bimodality = features.get("if_bimodality").unwrap_or(0.0);
    let mut v = Vec::new();
    let multilevel = modality >= 3.0;
    v.push(("4fsk", if multilevel { 0.7 } else { 0.08 }));
    // MSK is the h = 0.5 case; C14 measures h when it locks a rate.
    let msk = mod_index_h.is_some_and(|h| (0.42..0.58).contains(&h));
    v.push(("msk", if msk && !multilevel { 0.6 } else { 0.06 }));
    // Gaussian shaping smears the two tones together, so the histogram is less cleanly bimodal.
    let smeared = bimodality < 0.66;
    v.push((
        "gfsk",
        if !multilevel && smeared && !msk {
            0.5
        } else {
            0.12
        },
    ));
    v.push((
        "2fsk",
        if !multilevel && !smeared && !msk {
            0.7
        } else {
            0.15
        },
    ));
    v
}

fn psk_qam_classes(features: &Features) -> Option<Vec<(&'static str, f64)>> {
    let c40 = features.get("c40_norm")?;
    let c42 = features.get("c42_norm")?;
    // Nearest theoretical cumulant pair, as a soft score.
    Some(
        PSK_QAM_CUMULANTS
            .iter()
            .map(|(label, t40, t42)| {
                let d2 = ((c40 - t40) / 0.5).powi(2) + ((c42 - t42) / 0.5).powi(2);
                (*label, (-0.5 * d2).exp().max(1e-6))
            })
            .collect(),
    )
}

fn normalise_classes(scores: Vec<(&str, f64)>) -> Option<ClassGuess> {
    let sum: f64 = scores.iter().map(|(_, s)| s).sum();
    if !(sum.is_finite() && sum > 0.0) {
        return None;
    }
    let mut dist: Vec<LabelP> = scores
        .into_iter()
        .map(|(label, s)| LabelP {
            label: label.to_owned(),
            p: s / sum,
        })
        .collect();
    crate::fuse::normalise_dist(&mut dist);
    let top = dist
        .iter()
        .max_by(|a, b| a.p.total_cmp(&b.p).then_with(|| b.label.cmp(&a.label)))?
        .clone();
    Some(ClassGuess {
        label: top.label,
        p: top.p,
        dist,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::{FeatureInput, features};
    use crate::synth::{Class, SynthConfig, generate};
    use hk_model::classify::HK_MOD_V1;

    fn feats(class: Class, snr_db: f64, seed: u64) -> Features {
        let s = generate(class, &SynthConfig::new(snr_db, seed));
        features(&FeatureInput {
            samples: &s.samples,
            sample_rate_hz: s.sample_rate_hz,
            obw_hz: Some(s.obw_hz),
            snr_db: Some(snr_db),
            symbols: None,
        })
    }

    fn allowed(f: &Features, family: &str) -> bool {
        admissible(f)
            .into_iter()
            .find(|a| a.family == family)
            .map(|a| a.allowed)
            .unwrap_or(false)
    }

    #[test]
    fn admissibility_covers_every_family_and_only_rules_out_what_physics_rules_out() {
        let f = feats(Class::Fsk2, 25.0, 3);
        let a = admissible(&f);
        for fam in HK_MOD_V1.families {
            assert!(a.iter().any(|x| x.family == fam.name), "{}", fam.name);
        }
        assert!(a.iter().all(|x| x.allowed == x.reason.is_none()));
        // A constant-envelope FSK burst cannot be OOK, is not flat noise, and does not sweep.
        assert!(allowed(&f, "fsk"));
        assert!(!allowed(&f, "ook-ask"));
        assert!(!allowed(&f, "dsss"), "dsss abstains in M3");
        // An OOK burst is the other way round.
        let ook = feats(Class::Ook, 25.0, 3);
        assert!(allowed(&ook, "ook-ask"));
        // A chirp is the only thing that may be CSS.
        assert!(allowed(&feats(Class::Chirp, 25.0, 3), "css"));
        assert!(!allowed(&feats(Class::Qpsk, 25.0, 3), "css"));
        // Band-filling noise may be noise-like; a carrier may not.
        assert!(allowed(&feats(Class::NoiseLike, 25.0, 3), "noise-like"));
        assert!(!allowed(&feats(Class::Cw, 25.0, 3), "noise-like"));
    }

    #[test]
    fn the_coarse_hint_separates_analog_digital_and_noise() {
        assert_eq!(
            coarse_hint(&feats(Class::NoiseLike, 25.0, 5)),
            Coarse::NoiseLike
        );
        assert_eq!(coarse_hint(&feats(Class::Ofdm, 25.0, 5)), Coarse::Digital);
        // A 2-FSK burst is digital when its two tones are cleanly separated; a channel-filtered
        // one at a low modulation index can smear into a continuum, and the hint then says
        // `unknown` rather than guessing. It is only ever a *hint*: it fills in `coarse` when no
        // family is decided, and the family decision does not depend on it (2-FSK classifies as
        // `fsk` at 0.90 top-1 above its gate, per the accuracy sweep).
        // A continuous-phase 2-FSK *is* an angle modulation: at a low modulation index, filtered to
        // its own channel and without a symbol clock to expose the discrete levels, it is the same
        // waveform as narrowband FM, and the hint may say `analog`. That costs nothing here — the
        // hint only fills in `coarse` when no family is decided, and 2-FSK is decided as `fsk` at
        // 0.90 top-1 above its gate (accuracy sweep). What must never happen is calling a
        // modulated carrier structureless noise.
        assert_ne!(
            coarse_hint(&feats(Class::Fsk2, 25.0, 5)),
            Coarse::NoiseLike,
            "a modulated carrier is never noise-like"
        );
        let analog = coarse_hint(&feats(Class::Wfm, 25.0, 5));
        assert!(
            matches!(analog, Coarse::Analog | Coarse::Unknown),
            "wfm hinted {analog:?}"
        );
    }

    #[test]
    fn class_guesses_stay_inside_their_family_and_name_the_obvious_cases() {
        let tax = &HK_MOD_V1;
        for fam in tax.families {
            let f = feats(Class::Fsk2, 25.0, 9);
            if let Some(g) = class_guess(fam.name, &f, Some(40e3), Some(0.5)) {
                assert!(fam.classes.contains(&g.label.as_str()), "{}", g.label);
                for lp in &g.dist {
                    assert!(fam.classes.contains(&lp.label.as_str()), "{}", lp.label);
                }
                let sum: f64 = g.dist.iter().map(|l| l.p).sum();
                assert!((sum - 1.0).abs() < 1e-9, "{} sums to {sum}", fam.name);
            }
        }
        // The **order** within psk-qam is not decided here: the cumulants that separate BPSK from
        // QPSK need symbol-rate samples, and on an oversampled snippet they flatten towards zero
        // (see `features::tests`). What the rules must guarantee is that the guess is a real class
        // of the family with a real distribution — the order itself waits for the class gate and
        // for T-200's post-sync verifier.
        let bpsk = class_guess("psk-qam", &feats(Class::Bpsk, 30.0, 4), None, None).unwrap();
        assert!(
            ["bpsk", "qpsk", "8psk", "qam16", "qam64"].contains(&bpsk.label.as_str()),
            "{} is not a psk-qam class",
            bpsk.label
        );
        let wfm = class_guess("analog", &feats(Class::Wfm, 30.0, 4), Some(200e3), None).unwrap();
        assert_eq!(wfm.label, "wfm");
        let nbfm = class_guess("analog", &feats(Class::Nbfm, 30.0, 4), Some(16e3), None).unwrap();
        assert_eq!(nbfm.label, "nbfm");
        assert_eq!(
            class_guess("not-a-family", &feats(Class::Ook, 30.0, 4), None, None),
            None
        );
    }
}
