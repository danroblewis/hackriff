//! The classical feature tree: coarse split, per-family admissibility and the within-family class
//! call (ADR-0016 §4.3).
//!
//! The tree is deliberately **not** a chain of hard branches down to a leaf: a wrong branch high up
//! would be unrecoverable on an 8-bit front end. It instead **gates** families — each rule below
//! says only "this family cannot explain a snippet that looks like this", from physics — and the
//! class-conditional densities ([`crate::density`]) rank whatever survives. Every constant here is
//! a priori: from the definition of the modulation, or from the S5 floors. None is tuned against
//! the acceptance split.

use hk_model::classify::{Coarse, HK_MOD_V1, LabelP};

use crate::density::DensityModel;
use crate::features::Features;
use crate::verify::MAX_LOG_LR;

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

// There was a `NOISE_MAX_CYCLIC_DB = 8.0` here, "cyclic-line significance above which a noise-like
// emission is ruled out: noise has no cycle". It is gone rather than retuned (T-238): once C14
// actually ran and `cyclic_db` stopped abstaining, the dev grid showed band-limited Gaussian noise
// itself at 11.5 dB and every taxonomy class between 11.3 and 59.2 dB, with the analog classes
// (ssb 38.2, am 34.9) *above* the digital ones (2fsk 26.3, ofdm 12.3). No threshold on this feature
// separates the cases it was written to separate, so it is not a gate at any value. `cyclic_db`
// remains a fitted density dimension, where a per-class mean and sigma can use it honestly.

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
    let cp = get("cp_corr");
    let slope = get("if_slope_r2");
    let duty = get("duty");

    let modality = get("if_modality").unwrap_or(1.0);
    let bimodality = get("if_bimodality").unwrap_or(0.0);
    let mu42_a = get("mu42_a");

    let mut out = Vec::new();
    // Analog spans almost every envelope and spectrum shape its digital neighbours use, so it is
    // ruled out only by evidence of things an analog emission cannot have: a guard interval,
    // discrete frequency levels — or the Rayleigh envelope of a band-filling noise-like emission,
    // which no AM, FM, SSB or CW signal has (a constant-envelope angle modulation sits near
    // μ₄₂ ≈ 1, Gaussian noise at 2).
    //
    // Without these, analog is a catch-all: five broad classes covering enough of the feature
    // space to absorb high-order QAM, band noise and out-of-taxonomy signals that should come back
    // `unknown`.
    //
    // **A cyclic line is not one of them** (T-238). This arm used to deny analog a "symbol_clock"
    // whenever `cyclic_db` exceeded 8 dB, and it was dormant from the day it was written, because
    // nothing ever passed a C14 estimate in and the feature always abstained. Measuring it refutes
    // the premise outright: on the dev grid at 25 dB, C14's whitened line significance is 38.2 dB
    // for `ssb`, 34.9 for `am` and 25.5 for `nbfm`, against 26.3 for `2fsk`, 29.3 for `bpsk` and
    // 12.3 for `ofdm` — the analog range *contains* the digital one, and every class in the
    // taxonomy clears 8 dB (the minimum anywhere is 11.3). A strong cyclic line is real structure,
    // not evidence of keying: broadcast FM has a 19 kHz stereo pilot, and a keyed carrier has its
    // keying. Left live, this denied `analog` to 100 % of inputs and cost 0.25 of top-1 outright.
    //
    // So `cyclic_db` belongs to the densities, which fit a per-class mean and sigma for it, and not
    // to a hard gate that no threshold can make correct. Removing the arm keeps the behaviour the
    // tree has always actually had; it loosens nothing.
    out.push(match (cp, mu42_a) {
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
        (Some(c), _)
            if c >= OFDM_MIN_CP_CORR
                && flatness.is_some_and(|f| f >= OFDM_MIN_FLATNESS)
                && (mu42_a.is_some_and(|k| k > NOISE_LIKE_MIN_MU42)
                    || env_cv.is_some_and(|v| v >= ASK_MIN_ENV_CV)) =>
        {
            deny("analog", "guard_interval")
        }
        (_, Some(k))
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
    // The `cyclic_db > 8 dB` arm that used to sit here is gone for the same measured reason as the
    // analog one above (T-238): band-limited Gaussian noise itself reads 11.5 dB on the dev grid
    // and the held-out noise burst 11.3, so the arm denied `noise-like` to every input including
    // noise. What separates noise from a modulated carrier is its flat spectrum and absent carrier
    // line, which is what the two arms below test.
    out.push(match (flatness, carrier_db) {
        (Some(f), _) if f < NOISE_MIN_FLATNESS => deny("noise-like", "not_flat"),
        (_, Some(c)) if c > NOISE_MAX_CARRIER_DB => deny("noise-like", "carrier_line"),
        _ => allow("noise-like"),
    });
    out
}

/// The coarse hint used when no family is decided (a decided family takes its coarse from the
/// taxonomy). "Digital, unknown order" below the gates is the C15 card's required behaviour, so a
/// snippet with digital structure reports `digital` even when its family is `unknown`.
pub fn coarse_hint(features: &Features) -> Coarse {
    let modality = features.get("if_modality").unwrap_or(1.0);
    let cp = features.get("cp_corr").unwrap_or(0.0);
    let flatness = features.get("flatness");
    let carrier = features.get("carrier_line_db").unwrap_or(0.0);
    let env_cv = features.get("env_cv").unwrap_or(0.0);
    let low = features.get("low_fraction").unwrap_or(0.0);
    let mu42_a = features.get("mu42_a");

    // The envelope term is the same one [`admissible`] uses to keep `analog` from being denied to
    // a band-filling carrier, and it belongs here for the same reason: a spectrum that is flat and
    // lineless is *not* enough to call an emission structureless noise, because a fully modulated
    // carrier genuinely fills its own channel. Broadcast FM at the analysis geometry the pipeline
    // delivers is the case that exposed it — flatness 0.62 with its carrier line right at the
    // 14 dB threshold — and it was hinted `noise-like`, which is the one thing a modulated carrier
    // must never be called. Noise is Rayleigh (mu42_a ~ 2); an angle modulation is constant
    // envelope (~1). Without a measured envelope the hint is left as it was.
    let noise_like = flatness.is_some_and(|f| f >= NOISE_MIN_FLATNESS)
        && carrier <= NOISE_MAX_CARRIER_DB
        && cp < OFDM_MIN_CP_CORR
        && mu42_a.is_none_or(|k| k > NOISE_LIKE_MIN_MU42);
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
    // A cyclic line is deliberately not a term here (T-238): it is present on every class in the
    // taxonomy, analog included, so it would hint `digital` for everything. See `admissible`.
    let digital = discrete_levels || guard_interval || (low > 0.15 && env_cv > ASK_MIN_ENV_CV);
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

// There was a `PSK_QAM_CUMULANTS` table here — the textbook normalised cumulant pairs
// `(|Ĉ40|, Ĉ42)` of bpsk (2, −2), qpsk (1, −1), 8psk (0, −1), qam16 (0.68, −0.68) and
// qam64 (0.62, −0.62) — scored by distance. It is gone rather than retuned (T-243), because those
// values hold for **symbol-rate** samples and this stage never receives any: at
// `synth::SAMPLES_PER_OBW` the instants between symbols are pulse-shaped mixtures of neighbours,
// close to Gaussian, which drives the fourth-order cumulants towards zero for *every* class
// (`features::tests` pins this, and the dev grid measures |Ĉ40| at 0.03–0.07 for qpsk, 8psk, qam16
// and qam64 alike, against theoretical values spanning 0 to 2).
//
// A distance to five fixed points from a measurement that always lands near the origin does not
// rank constellations; it ranks the points by how close each one happens to sit to zero. That is
// exactly what it did: 8psk (0, −1) is nearest, so the tree called 8psk on everything, and bpsk
// (2, −2) is furthest, so bpsk scored `exp(−16)`, clamped to the 1e-6 floor, i.e. **0.00 after
// normalising**. A prior of exactly zero is unrecoverable — it is below the verifier's
// `CANDIDATE_MIN_P`, so bpsk was not even a hypothesis, and no later stage could multiply its way
// back (measured, T-200/T-213: bpsk, qpsk and qam16 top-1 all 0.000 at 15, 20 and 25 dB).
//
// What replaces it is [`psk_qam_classes`]: the class-conditional densities, which are fitted on
// what these features **actually** measure at this geometry rather than on what they would measure
// at symbol rate.

/// The within-family class of `family`, or `None` when the features cannot separate its classes.
///
/// `model` supplies the class-conditional densities (the `psk-qam` and `analog` orders are both
/// read from them), and `mod_index_h` the C14 modulation index (FSK needs it to name MSK).
///
/// It **no longer takes the C13 occupied bandwidth** (T-249). That parameter existed for one
/// arm — the `wfm`/`nbfm` split, which the removed `analog_classes` made by testing
/// `obw_hz > 100e3` — and nothing else read it. The densities separate the two on their own
/// (measured: `wfm` 36/36 and `nbfm` 35/36 by density arg-max on the blind acceptance seeds),
/// through the several dimensions on which a broadcast multiplex and a voice channel differ, so
/// carrying an unused bandwidth into this call would only invite a second, disagreeing opinion
/// about which of them a snippet is.
pub fn class_guess(
    family: &str,
    features: &Features,
    model: &DensityModel,
    mod_index_h: Option<f64>,
) -> Option<ClassGuess> {
    let scores: Vec<(&str, f64)> = match family {
        "analog" => density_classes("analog", features, model)?,
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
        "psk-qam" => density_classes("psk-qam", features, model)?,
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

// There was an `analog_classes` hand-written score table here — five conjunctions of feature
// thresholds, one per analog class, each scoring 0.7–0.8 when it fired and 0.05–0.1 when it did
// not. It is gone rather than retuned (T-249), for the same reason the `PSK_QAM_CUMULANTS` table
// above went: **the conjunctions were unreachable or degenerate at the geometry this stage
// actually runs at**, and the evidence that names these classes correctly was already fitted and
// sitting unused in the class-conditional densities.
//
// What it did, measured on the blind acceptance grid at and above the analog gate (10 dB):
// `cw` top-1 **0.000** with wrong-label **1.000**, and `ssb` top-1 **0.000** with wrong-label
// 1.000 / 0.917. Both were called `am`. The family call was correct throughout — the system knew
// it was analog and then named the wrong class inside it, confidently.
//
// **`cw` could not be named at all.** Its conjunction required `sigma_af < 0.02`, written from the
// noiseless physics ("a keyed carrier has essentially no frequency excursion"). The instantaneous
// -frequency estimator is **noise-limited**, and at the analog gate the noise is far above that
// bound: measured σ_af on the keyed carrier is 0.170–0.207 at 10 dB, 0.068–0.092 at 15 dB and
// 0.038–0.050 at 20 dB — a clean 1/√ρ law (halving per 6 dB), so the threshold is not reached
// until roughly 28 dB SNR, 18 dB above the gate. The conjunct therefore *never* fired, `cw` scored
// the 0.05 floor against `am`'s and `nbfm`'s 0.1, and every `cw` snippet in the grid was named
// something else. A constant taken from the modulation's definition is only admissible if the
// measurement can resolve it; this one was below its own noise floor.
//
// **`ssb` lost an exact tie to `am`.** `am`'s conjunction tested `carrier_line_db > 14.0` as its
// "there is a carrier" term, but `carrier_line_db` is a CFAR *strongest-line* statistic
// ([`crate::features`], T-404), not a carrier detector: a suppressed-carrier SSB emission's
// loudest audio tone reads 28–40 dB on it. So at 15 and 20 dB both conjunctions fired, `ssb`
// scored 0.7 and `am` scored 0.7, and the tie broke alphabetically — which is why the baseline's
// `ssb` top-2 was 0.75/0.58 while its top-1 was 0.000: the right answer was there, one place down,
// losing a coin toss.
//
// What replaces both is [`density_classes`] over the same fitted densities `psk-qam` already used.
// They are fitted per class on the **dev** split at this exact geometry, so they describe what
// these features really measure on a keyed carrier and on a suppressed-carrier voice signal rather
// than what a textbook says they would. Measured before any change, as the arg-max of the shipped
// densities over the five analog classes on the blind acceptance seeds: **168/180 = 0.933**, with
// `cw` 36/36 and `ssb` 36/36 correct and margins of 25–60 nats. The separating evidence was
// already on disk; only the call site was not reading it.

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

/// The within-family order of `family`, from the class-conditional densities (ADR-0016 §4.3)
/// rather than from a hand-written table of feature thresholds. `psk-qam` uses it because the
/// textbook cumulant pairs that oversampling destroyed cannot rank constellations (see the note
/// above); `analog` uses it because the conjunctions it replaced could not name `cw` or `ssb` at
/// all (T-249, see the note above `fsk_classes`).
///
/// **Why this is the right evidence.** The densities are fitted on the dev grid at *this* analysis
/// geometry, so each class's dimensions describe what the feature really measures on that
/// modulation rather than what theory says a symbol-rate sample would. For `psk-qam`, two
/// dimensions carry the separation the cumulant table could not:
/// - `c20_norm` — the **second**-order structure, which survives pulse shaping because a real
///   constellation keeps `E[x²] ≠ 0` however it is filtered. On the dev grid it reaches 0.97 on
///   `bpsk` while every rotationally symmetric class stays under 0.10, which is what makes `bpsk`
///   nameable at all.
/// - `c42_norm` — measured at −0.78 for `qpsk`/`8psk` against −0.47 for `qam64`, separating the
///   PSK orders from the dense QAM ones.
///
/// For `analog` the separation is spread across the envelope, keying and spectral dimensions
/// jointly rather than resting on any one threshold, which is exactly why the conjunctions failed
/// and a fitted density does not: measured arg-max over the five analog classes on the blind
/// acceptance seeds is 168/180, with `cw` and `ssb` each 36/36.
///
/// **Why the spread is bounded.** The ratio between two classes is clamped to [`MAX_LOG_LR`], the
/// same 19:1 bound the post-sync verifier applies to its own likelihoods and for the same reason:
/// this is a diagonal-Gaussian model with no channel, no front end and no interference in it, so
/// it may order the candidates but may not claim certainty from them. Two consequences matter, and
/// both are properties of the arithmetic rather than of a threshold:
/// - **No class is ever given exactly zero.** A class whose evidence underflowed, or that the
///   snippet measured too few dimensions to score at all, sits at the 19:1 floor — low, but still
///   a hypothesis the verifier and any later stage can recover. That is the failure this replaces.
/// - **The call cannot be confidently wrong.** With four rivals at the floor the top class reaches
///   at most `1/(1 + 4/19)` ≈ 0.83, so such a class call never reports p ≥ 0.9.
fn density_classes(
    family: &str,
    features: &Features,
    model: &DensityModel,
) -> Option<Vec<(&'static str, f64)>> {
    let classes = HK_MOD_V1.family(family)?.classes;
    let scored: Vec<(&'static str, Option<f64>)> = classes
        .iter()
        .map(|c| (*c, model.score_class(c, features).map(|s| s.log_evidence)))
        .collect();
    // Log evidence, never `evidence` itself: it underflows to exactly 0 for a poor fit, and the
    // ratio of two underflowed scores is the zero this task exists to remove.
    let best = scored
        .iter()
        .filter_map(|(_, ll)| *ll)
        .fold(f64::NEG_INFINITY, f64::max);
    if !best.is_finite() {
        return None;
    }
    Some(
        scored
            .into_iter()
            .map(|(class, ll)| {
                let ll = ll.unwrap_or(f64::NEG_INFINITY).max(best - MAX_LOG_LR);
                (class, (ll - best).exp())
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
            if let Some(g) = class_guess(fam.name, &f, DensityModel::builtin(), Some(0.5)) {
                assert!(fam.classes.contains(&g.label.as_str()), "{}", g.label);
                for lp in &g.dist {
                    assert!(fam.classes.contains(&lp.label.as_str()), "{}", lp.label);
                }
                let sum: f64 = g.dist.iter().map(|l| l.p).sum();
                assert!((sum - 1.0).abs() < 1e-9, "{} sums to {sum}", fam.name);
            }
        }
        // **The order within psk-qam is decided here, and no class may be given exactly zero**
        // (T-243). The textbook cumulants that separate BPSK from QPSK need symbol-rate samples and
        // flatten towards zero on the oversampled snippet this stage receives (see
        // `features::tests`), so the order is read from the class-conditional densities instead.
        // What the rules must guarantee is that every class of the family stays a live hypothesis:
        // a probability of exactly zero is unrecoverable — it sits below the verifier's
        // `CANDIDATE_MIN_P`, so no later stage can move it — and that is what put `bpsk` at 0.000
        // top-1 at every SNR.
        let bpsk = class_guess(
            "psk-qam",
            &feats(Class::Bpsk, 30.0, 4),
            DensityModel::builtin(),
            None,
        )
        .unwrap();
        assert_eq!(bpsk.label, "bpsk", "{:?}", bpsk.dist);
        for class in ["bpsk", "qpsk", "8psk", "qam16", "qam64"] {
            let p = bpsk
                .dist
                .iter()
                .find(|lp| lp.label == class)
                .unwrap_or_else(|| panic!("{class} missing from {:?}", bpsk.dist))
                .p;
            assert!(p > 0.0, "{class} was given exactly zero: {:?}", bpsk.dist);
        }
        // The 19:1 clamp bounds the top of a five-class call at 1/(1 + 4/19) ≈ 0.83, so this stage
        // cannot produce a confident wrong answer whatever the densities say.
        assert!(bpsk.p < 0.9, "psk-qam class call reported p {}", bpsk.p);
        let wfm = class_guess(
            "analog",
            &feats(Class::Wfm, 30.0, 4),
            DensityModel::builtin(),
            None,
        )
        .unwrap();
        assert_eq!(wfm.label, "wfm");
        let nbfm = class_guess(
            "analog",
            &feats(Class::Nbfm, 30.0, 4),
            DensityModel::builtin(),
            None,
        )
        .unwrap();
        assert_eq!(nbfm.label, "nbfm");
        assert_eq!(
            class_guess(
                "not-a-family",
                &feats(Class::Ook, 30.0, 4),
                DensityModel::builtin(),
                None
            ),
            None
        );
    }
}
