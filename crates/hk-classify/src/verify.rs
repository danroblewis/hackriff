//! The post-sync likelihood verifier (ADR-0016 §4.5, T-200): ALRT/GLRT **within** the candidate
//! set the feature tree already produced.
//!
//! ```text
//! feature tree ─▶ family + within-family class distribution
//!                          │
//!                          ├─ clock locked (C14 trusted rate)?  no ─▶ nothing happens
//!                          └─ yes ─▶ likelihood test over the candidates with p >= 0.05
//!                                     └▶ re-ranked class distribution, stage `verifier`
//! ```
//!
//! # What it may do, and what it may never do
//!
//! The verifier's role is deliberately narrow. It **re-ranks**, and that is all:
//!
//! - It never introduces a class the tree did not offer. Its hypothesis set is exactly the entries
//!   of [`ClassCall::dist`] at or above [`CANDIDATE_MIN_P`] — a label with no prior mass is not a
//!   candidate, however well it would fit.
//! - It never touches the **family**. `family`, `confidence`, `posterior`, `likelihood`,
//!   `open_set_score` and `coarse` come out exactly as the tree left them. A family-level
//!   abstention stays an abstention: a `Classification` whose family is `unknown` has no class call
//!   at all, so there is nothing here to re-rank and the verifier declines to run
//!   ([`SkipReason::Abstained`]).
//! - It never raises the row's arbitration rank. [`Classification::stage`] stays
//!   [`Stage::FeatureTree`] because the *family* is still the tree's call; only
//!   [`ClassCall::stage`] becomes [`Stage::Verifier`], which is the field that records which stage
//!   decided the class. ADR-0016 §2 ranks a "verifier" row at 2 (lock-verified), above a
//!   demodulator chain's label — but that rank is for evidence about the **family**, and a
//!   class-only re-rank has produced none. Promoting the whole row would let this stage outrank a
//!   chain label it knows nothing about, which is a promotion, not a re-ranking.
//!
//! These are enforced by [`verify`]'s own construction and asserted in
//! `it_can_only_rerank_never_promote_and_never_unabstain`.
//!
//! # Where it runs
//!
//! Only post-sync, which here means **C14 reported a trusted symbol rate**
//! ([`SymbolParameters::rate_trusted`]): without a symbol clock there are no symbol instants to
//! sample at and no pulse grid to fit, so every likelihood below would be measuring the analysis
//! filter rather than the modulation. It also needs the emission at C14's geometry
//! ([`crate::symbols::SYMBOL_SAMPLES_PER_OBW`]), for the same reason C14 does: at the classifier's
//! 2 samples per OBW99 a symbol period is about two samples. Where either is missing the tree's
//! ranking stands, untouched.
//!
//! # The tests themselves
//!
//! - **psk-qam — ALRT with known SNR, GLRT over phase.** Symbol samples are taken through an RRC
//!   matched filter (roll-off, timing **and residual carrier frequency** estimated once, *before*
//!   any hypothesis is considered, so no hypothesis gets a nuisance parameter fitted in its own
//!   favour — see [`remove_residual_cfo`], without which the stage was monotone in alphabet size
//!   because it was ranking a ring rather than a constellation, T-246). Each candidate's
//!   constellation then gives the exact average likelihood
//!   `Σ_k log (1/M) Σ_m exp(−|y_k e^{−jθ} − s_m|²/N₀)`, with `N₀` from the measured SNR and `θ`
//!   from the M-th-power estimate. Marginalising over the symbol alphabet is what makes this an
//!   ALRT, and it is self-normalising: a larger constellation buys its extra flexibility with the
//!   `1/M` prior, so no complexity penalty is needed or applied.
//! - **fsk — GLRT over the frequency-pulse shape.** The instantaneous-frequency trajectory is fitted
//!   as `A·Σ_k a_k·g(t − τ − kT)`, with `g` rectangular (CPFSK: `2fsk`, `4fsk`) or Gaussian
//!   (`gfsk`), symbols hard-decided and `A` by least squares. This is the one thing that actually
//!   separates 2-FSK from GFSK at the same modulation index: the *shape of the transition*, which
//!   the tree can only see indirectly through the smearing of an IF histogram. A GLRT maximises over
//!   its free parameters, so hypotheses are compared by description length ([`mdl_penalty`]) rather
//!   than by raw fit — otherwise `4fsk`, whose alphabet contains the binary one, could never lose
//!   to `2fsk`.
//!
//! # Three classes this stage does not rank, and why (T-422)
//!
//! Both tests above are correct **given what they are told**, and for three labels what they are
//! told is wrong at this geometry. Each declines by returning `None` from its model lookup, which
//! leaves the label out of the hypothesis set entirely; [`bounded_update`] then carries its tree
//! prior through untouched, so declining costs nothing and claims nothing.
//!
//! - **`qam16` and `qam64`** ([`constellation`]) — the ALRT's `N₀` comes from the SNR meter, but the
//!   residual of an interpolated symbol sample here is dominated by filter and timing error, which
//!   does not shrink with SNR. Sweeping *only* the assumed SNR flips both truths together, at the
//!   same value, from "always `qam64`" to "always `qam16`": the ratio is a function of the
//!   assumption, not of the data. It cost `qam16` class top-1 0.833 → 0.083 and bought nothing on
//!   the PSK orders.
//! - **`msk`** ([`fsk_model`]) — its peak deviation was fixed at `π/(2·sps)`, the *transmitter's*
//!   `h = 0.5`. Its own free-deviation twin `2fsk` beat it on genuine MSK 22 times in 22, after the
//!   MDL charge for that freedom, so the constraint is on the wrong quantity; the GLRT's arg-max
//!   was `gfsk` 19 times in 22 and class top-1 fell 0.917 → 0.375.
//!
//! In each case the evidence that *does* name the class is elsewhere and already measured — the
//! fitted densities for the QAM orders, C14's modulation index for MSK — which is the same shape of
//! conclusion T-249 reached about `cw` and `ssb`. Note what let all three hide: `verifier_gain`
//! measures the two pairs ADR-0016 §4.5 names, `2fsk`/`gfsk` and `bpsk`/`qpsk`, and none of the
//! three is in either pair.
//!
//! # Bounded evidence, on purpose
//!
//! A likelihood ratio over hundreds of symbols is astronomically large, and taking it at face value
//! would let this stage report a class at probability 1.00 on the strength of a model that knows
//! nothing about the real channel, the front end, or the interference. That is precisely the
//! failure this project has reverted three times. So the update is a **bounded** Bayesian one: the
//! log-ratio applied to the tree's prior is clamped to [`MAX_LOG_LR`], the candidate block keeps
//! exactly the probability mass the tree gave it, and the result is capped at
//! [`MAX_CONFIDENCE`]. The verifier can therefore overturn a close call, and cannot manufacture
//! certainty.

use hk_estimate::blind::SymbolParameters;
use hk_model::classify::{ClassCall, Classification, LabelP, MAX_CONFIDENCE, Stage, UNKNOWN};
use num_complex::{Complex32, Complex64};

/// Verifier rule-set version (reported alongside [`crate::thresholds::RULES_VERSION`]).
pub const VERIFIER_VERSION: &str = "hk-classify/verify@1";

/// Smallest prior probability a class needs to be a candidate (ADR-0016 §4.5). A label the tree
/// gave less than this is not in the hypothesis set at all.
///
/// **It excludes every candidate the densities floored, and lowering it does not help** (measured,
/// T-422). [`crate::tree::density_classes`] clamps its spread to [`MAX_LOG_LR`], so in a `k`-class
/// family a floored rival normalises to exactly `1/(19 + k − 1)` — 0.0455 for the four `fsk`
/// classes, 0.0435 for the five `analog` ones. Both are below 0.05, so whenever a density is
/// decisive this stage sees a single candidate and skips, and it runs only where the density was
/// already undecided.
///
/// That reads like the bug that costs `gfsk` its remaining errors, and it is not, for a reason
/// worth writing down: **the density floor and this stage's cap are the same 19:1 number**. A
/// candidate sitting at the floor is 19:1 behind, [`bounded_update`] may move it by at most 19:1,
/// so the very best outcome is a dead heat — which `normalise_classes` then breaks alphabetically,
/// against `gfsk` and in favour of `2fsk`, exactly as it broke `ssb` against `am` in T-249. Setting
/// this to `1/23` and re-running the blind grid confirms it: `gfsk` does not move (0.750 at both
/// bins), while `bpsk` falls 1.000 → 0.333 and `qpsk` 0.917 → 0.667, because the psk-qam ALRT then
/// starts arbitrating PSK orders it had been skipping. The floor is protecting those calls, not
/// obstructing `gfsk`. Making a floored candidate recoverable needs the two 19:1 bounds to stop
/// being equal, which is an ADR-0016 §4.3/§4.5 question and not a constant to nudge here.
pub const CANDIDATE_MIN_P: f64 = 0.05;

/// Fewest symbols the verifier will test on. Below this the residual variance of a pulse fit, and
/// the average likelihood of a constellation, are both dominated by their own estimation error.
pub const MIN_SYMBOLS: usize = 48;

/// Most symbols it will use: the bound on its per-event cost, as [`crate::symbols`] bounds C14's.
pub const MAX_SYMBOLS: usize = 256;

/// Samples per symbol the analysis geometry must provide. Below `MIN_SPS` a frequency pulse is not
/// resolved (its shape *is* the test); above `MAX_SPS` the snippet holds too few symbols to be
/// worth the arithmetic.
pub const MIN_SPS: f64 = 3.0;
/// See [`MIN_SPS`].
pub const MAX_SPS: f64 = 64.0;

/// Largest log-likelihood-ratio the verifier may apply to the tree's class prior: `ln 19`, i.e. it
/// may shift the odds of one candidate against another by at most 19:1.
///
/// This is not a tuned number and it is not a threshold on the data — it is a cap on how much this
/// stage is *permitted to claim*, chosen a priori so that the verifier can overturn a genuinely
/// close call (0.50/0.50 becomes 0.95/0.05) while it cannot turn a model fit into certainty. The
/// unclamped ratios are routinely `e^1000` and mean nothing at that magnitude: they are computed
/// under a model with no channel, no front end and no interference in it.
pub const MAX_LOG_LR: f64 = 2.944_438_979_166_44; // ln 19

/// Timing phases searched per FSK hypothesis, over one symbol period.
const TAU_STEPS: usize = 8;

/// RRC roll-offs considered when matching the linear-modulation filter. Chosen *once*, before any
/// hypothesis, by the strength of the symbol-rate timing line — never per hypothesis.
const ALPHA_GRID: [f64; 3] = [0.2, 0.35, 0.5];

/// Gaussian `BT` products searched for the `gfsk` hypothesis: the **deployed** range and nothing
/// wider (GSM 0.3, Bluetooth 0.5).
///
/// The upper end matters more than it looks. A Gaussian at `BT` 0.7 and above is very nearly a
/// rectangle, so admitting it lets the `gfsk` hypothesis impersonate the CPFSK one and win on every
/// input by sheer flexibility — measured: with 0.7 in this grid, `gfsk` beat `2fsk` on 100 % of
/// snippets, by a *larger* margin on genuine 2-FSK (+3.3 to +8.9 nats/symbol) than on genuine GFSK
/// (+2.3 to +3.8). A hypothesis set has to contain distinguishable hypotheses.
const BT_GRID: [f64; 2] = [0.3, 0.5];

/// The `BT` standing for "rectangular, as this receiver sees it".
///
/// A CPFSK transmitter keys its frequency abruptly, but nothing downstream of the antenna ever sees
/// that edge: C13 hands the classifier a snippet filtered to ±0.75 × OBW99 and decimated, which is
/// a channel roughly one symbol rate wide, and that filter smooths the instantaneous-frequency
/// trajectory of *every* emission it passes. Comparing an ideal rectangle against a Gaussian
/// therefore does not compare two transmitters; it measures the receive filter, and the smoother
/// hypothesis wins whatever was transmitted. Giving the CPFSK hypotheses this fixed, mild smoothing
/// makes the test what it is supposed to be: is the transition *sharper* than the channel alone
/// would leave it?
const CHANNEL_BT: f64 = 1.0;

/// SNR range the ALRT's `N₀` is taken from, dB. Outside it the measured in-band SNR says more about
/// the analysis band than about `E_s/N₀`, and an unclamped `N₀` would make the likelihood either
/// degenerate (no noise) or flat (all noise).
const SNR_CLAMP_DB: (f64, f64) = (0.0, 30.0);

/// Everything the verifier measures on, beyond the [`Classification`] it re-ranks.
#[derive(Clone, Copy, Debug)]
pub struct VerifyInput<'a> {
    /// The emission at C14's geometry ([`crate::symbols::SYMBOL_SAMPLES_PER_OBW`]) — the same view
    /// C14 estimated the symbol rate on, not the classifier's 2-samples-per-OBW99 snippet.
    pub samples: &'a [Complex32],
    /// Sample rate of [`VerifyInput::samples`], Hz.
    pub sample_rate_hz: f64,
    /// C14's estimate. The verifier runs only when this reports a **trusted** symbol rate.
    pub symbols: Option<&'a SymbolParameters>,
    /// C13 in-band SNR, dB: the known-SNR half of the ALRT.
    pub snr_db: Option<f64>,
}

/// Why the verifier did not run. Every arm is a case where the likelihood test would be measuring
/// something other than the modulation, so the tree's ranking stands unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// The family call was `unknown`: there is no class distribution, and an abstention is never
    /// turned into a claim.
    Abstained,
    /// The tree reported no within-family class (below the class gate, or the family has none).
    NoClassCall,
    /// Fewer than two candidates at [`CANDIDATE_MIN_P`]: nothing to re-rank.
    SingleCandidate,
    /// No C14 estimate, or its symbol rate is not trusted: not post-sync.
    NoClockLock,
    /// This family has no likelihood model (only `psk-qam` and `fsk` do in M3).
    NoModel,
    /// The snippet is too short, or its geometry gives too few samples per symbol.
    Geometry,
    /// No symbol-geometry view was supplied at all, so the verifier was never offered the call
    /// ([`crate::classifier`]'s own arm — the stage did not even get as far as [`verify`]).
    NoSymbolView,
}

impl SkipReason {
    /// The machine reason code.
    pub const fn as_str(self) -> &'static str {
        match self {
            SkipReason::Abstained => "verifier_abstained_upstream",
            SkipReason::NoClassCall => "verifier_no_class_call",
            SkipReason::SingleCandidate => "verifier_single_candidate",
            SkipReason::NoClockLock => "verifier_no_clock_lock",
            SkipReason::NoModel => "verifier_no_model",
            SkipReason::Geometry => "verifier_geometry",
            SkipReason::NoSymbolView => "verifier_no_symbol_view",
        }
    }
}

/// What one verification did.
#[derive(Clone, Debug, PartialEq)]
pub enum VerifyOutcome {
    /// It did not run; the classification is untouched.
    Skipped(SkipReason),
    /// It ran over `candidates` hypotheses.
    Ran {
        /// Candidate labels tested, with their mean per-symbol log-likelihood (diagnostics).
        scores: Vec<(String, f64)>,
        /// The class label before, and after.
        from: String,
        /// See `from`.
        to: String,
    },
}

impl VerifyOutcome {
    /// Whether the top class changed.
    pub fn reranked(&self) -> bool {
        matches!(self, VerifyOutcome::Ran { from, to, .. } if from != to)
    }

    /// Whether the verifier ran at all.
    pub fn ran(&self) -> bool {
        matches!(self, VerifyOutcome::Ran { .. })
    }
}

/// Runs the post-sync verifier over `c`'s existing class candidates, re-ranking them in place.
///
/// It mutates **only** `c.class`, and only ever by reordering probability within the candidate set
/// the tree produced. See the module docs for what it is forbidden to do.
pub fn verify(c: &mut Classification, input: &VerifyInput<'_>) -> VerifyOutcome {
    if c.family == UNKNOWN {
        return VerifyOutcome::Skipped(SkipReason::Abstained);
    }
    let Some(call) = c.class.as_ref() else {
        return VerifyOutcome::Skipped(SkipReason::NoClassCall);
    };
    // The hypothesis set is exactly what the tree offered, filtered by the ADR's floor. Nothing
    // else may enter it.
    let candidates: Vec<LabelP> = call
        .dist
        .iter()
        .filter(|lp| lp.p >= CANDIDATE_MIN_P)
        .cloned()
        .collect();
    if candidates.len() < 2 {
        return VerifyOutcome::Skipped(SkipReason::SingleCandidate);
    }
    let Some(symbols) = input.symbols.filter(|s| s.rate_trusted()) else {
        return VerifyOutcome::Skipped(SkipReason::NoClockLock);
    };
    let Some(rate_bd) = symbols.symbol_rate_bd.value().filter(|r| *r > 0.0) else {
        return VerifyOutcome::Skipped(SkipReason::NoClockLock);
    };
    if !(input.sample_rate_hz.is_finite() && input.sample_rate_hz > 0.0) {
        return VerifyOutcome::Skipped(SkipReason::Geometry);
    }
    let sps = input.sample_rate_hz / rate_bd;
    if !(MIN_SPS..=MAX_SPS).contains(&sps) {
        return VerifyOutcome::Skipped(SkipReason::Geometry);
    }
    // Cost bound: at most MAX_SYMBOLS symbols, whatever the extent (as `symbols::MAX_WINDOW_SAMPLES`
    // bounds C14).
    let want = (sps * MAX_SYMBOLS as f64).ceil() as usize;
    let x: Vec<Complex64> = input.samples[..input.samples.len().min(want)]
        .iter()
        .map(|s| Complex64::new(f64::from(s.re), f64::from(s.im)))
        .collect();
    if (x.len() as f64 / sps) < MIN_SYMBOLS as f64 {
        return VerifyOutcome::Skipped(SkipReason::Geometry);
    }

    let scores = match c.family.as_str() {
        "psk-qam" => psk_qam_loglikelihoods(&candidates, &x, sps, input.snr_db),
        "fsk" => fsk_loglikelihoods(&candidates, &x, sps),
        _ => return VerifyOutcome::Skipped(SkipReason::NoModel),
    };
    let Some(scores) = scores else {
        return VerifyOutcome::Skipped(SkipReason::Geometry);
    };

    let from = call.label.clone();
    let updated = bounded_update(&call.dist, &scores);
    let to = updated.label.clone();
    if let Some(class) = c.class.as_mut() {
        *class = updated;
    }
    push_reason(
        &mut c.reasons,
        if from == to {
            "verifier_confirmed"
        } else {
            "verifier_reranked"
        },
    );
    VerifyOutcome::Ran { scores, from, to }
}

/// The bounded Bayesian update: the tree's distribution is the prior, the verifier's clamped
/// log-likelihood-ratios are the evidence, and the candidate block keeps exactly the mass it had.
///
/// Labels are never added or removed, and a label the verifier did not score keeps its probability
/// untouched — which is what makes "re-rank only" a property of the arithmetic rather than a
/// comment.
fn bounded_update(prior: &[LabelP], scores: &[(String, f64)]) -> ClassCall {
    let best = scores
        .iter()
        .map(|(_, ll)| *ll)
        .fold(f64::NEG_INFINITY, f64::max);
    let mass: f64 = prior
        .iter()
        .filter(|lp| scores.iter().any(|(l, _)| *l == lp.label))
        .map(|lp| lp.p)
        .sum();

    let mut weighted: Vec<(usize, f64)> = Vec::new();
    for (i, lp) in prior.iter().enumerate() {
        if let Some((_, ll)) = scores.iter().find(|(l, _)| *l == lp.label) {
            // Clamped to [-MAX_LOG_LR, 0]: the verifier may demote a candidate by at most 19:1
            // against the best-fitting one, never more.
            let w = (ll - best).max(-MAX_LOG_LR);
            weighted.push((i, lp.p * w.exp()));
        }
    }
    let total: f64 = weighted.iter().map(|(_, w)| *w).sum();

    let mut dist: Vec<LabelP> = prior.to_vec();
    if total > 0.0 && mass > 0.0 {
        for (i, w) in weighted {
            dist[i].p = mass * w / total;
        }
    }
    cap_class(&mut dist);
    let top = dist
        .iter()
        .max_by(|a, b| a.p.total_cmp(&b.p).then_with(|| b.label.cmp(&a.label)))
        .cloned()
        .unwrap_or(LabelP {
            label: String::new(),
            p: 0.0,
        });
    ClassCall {
        label: top.label,
        p: top.p,
        dist,
        stage: Stage::Verifier,
    }
}

/// Keeps any single class below [`MAX_CONFIDENCE`] and the distribution summing to one. A class
/// call of exactly 1.00 is the shape of every confidently wrong answer this project has reverted.
fn cap_class(dist: &mut [LabelP]) {
    let sum: f64 = dist.iter().map(|lp| lp.p).sum();
    if !(sum.is_finite() && sum > 0.0) {
        return;
    }
    for lp in dist.iter_mut() {
        lp.p /= sum;
    }
    let n = dist.len();
    if n < 2 {
        return;
    }
    if let Some(i) = dist
        .iter()
        .position(|lp| lp.p > MAX_CONFIDENCE)
        .filter(|_| n > 1)
    {
        let excess = dist[i].p - MAX_CONFIDENCE;
        dist[i].p = MAX_CONFIDENCE;
        let rest: f64 = dist
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(_, lp)| lp.p)
            .sum();
        for (j, lp) in dist.iter_mut().enumerate() {
            if j != i {
                lp.p += if rest > 0.0 {
                    excess * lp.p / rest
                } else {
                    excess / (n - 1) as f64
                };
            }
        }
    }
}

fn push_reason(reasons: &mut Vec<String>, reason: &str) {
    if !reasons.iter().any(|r| r == reason) {
        reasons.push(reason.to_owned());
    }
}

// -------------------------------------------------------------------------------------------
// psk-qam: ALRT with known SNR, GLRT over phase.
// -------------------------------------------------------------------------------------------

/// Unit-average-energy constellation of a `psk-qam` class, or `None` for a label with no model.
fn constellation(label: &str) -> Option<Vec<Complex64>> {
    let psk = |m: usize| {
        (0..m)
            .map(|k| {
                let a = std::f64::consts::TAU * k as f64 / m as f64;
                Complex64::new(a.cos(), a.sin())
            })
            .collect::<Vec<_>>()
    };
    Some(match label {
        "bpsk" => psk(2),
        "qpsk" => psk(4),
        "8psk" => psk(8),
        // **The QAM orders are not scored by this stage** (T-422). The ALRT below is a known-SNR
        // test: it takes `N₀` from C13's in-band SNR and uses it as the symbol-decision noise
        // variance. At this analysis geometry that is not what the residual is — a smoothed,
        // interpolated symbol sample carries filter and timing error that does not shrink when the
        // SNR rises — and the `qam16` / `qam64` ratio is decided by the assumption rather than by
        // the data.
        //
        // Measured directly, by sweeping only the assumed SNR over the same 35 blind snippets and
        // changing nothing else:
        //
        // ```text
        // assumed SNR   genuine qam16 -> qam64   genuine qam64 -> qam64
        //     30 dB            21 / 21                  14 / 14
        //     20 dB            21 / 21                  14 / 14
        //     15 dB            21 / 21                  14 / 14
        //     12 dB            10 / 21                  14 / 14
        //     10 dB             0 / 21                  10 / 14
        //      6 dB             0 / 21                   0 / 14
        // ```
        //
        // Both truths flip **together**, at the same assumed SNR, from "always the denser
        // constellation" to "always the sparser one", and no value is right on both. That is the
        // signature of a test with no discriminating power for alphabet order here: above the
        // crossover the tiny `N₀` turns the mixture into a nearest-point distance and the denser
        // lattice wins by construction; below it the `−ln M` alphabet term dominates and the
        // sparser one wins by construction. Neither regime reads the data.
        //
        // What it cost: `qam16` class top-1 **0.833 from the tree's densities down to 0.083** after
        // this stage, wrong-label 0.917, every one of them named `qam64`. What it bought: nothing —
        // `bpsk`, `qpsk` and `8psk` are called identically with and without it (1.000, 0.875,
        // 1.000). The pair ADR-0016 §4.5 names and `verifier_gain` measures is `bpsk`/`qpsk`, which
        // is why this was never seen.
        //
        // Returning `None` leaves both labels out of the hypothesis set, and [`bounded_update`]
        // carries their tree priors through untouched. Restoring the stage for them needs a
        // residual estimated from the symbols rather than assumed from the SNR meter (and then the
        // `1/(π N₀)` normaliser this function's caller may currently omit, because with a common
        // `N₀` it cancels and with a fitted one it does not).
        "qam16" | "qam64" => return None,
        _ => return None,
    })
}

/// Root-raised-cosine taps at `sps` samples per symbol, `span` symbols either side.
fn rrc(sps: f64, alpha: f64, span: usize) -> Vec<f64> {
    let half = (sps * span as f64).round() as isize;
    (-half..=half)
        .map(|i| {
            let t = i as f64 / sps;
            if t.abs() < 1e-9 {
                return 1.0 - alpha + 4.0 * alpha / std::f64::consts::PI;
            }
            let denom = 1.0 - (4.0 * alpha * t).powi(2);
            if denom.abs() < 1e-9 {
                let a = std::f64::consts::PI / (4.0 * alpha);
                return alpha / 2.0_f64.sqrt()
                    * ((1.0 + 2.0 / std::f64::consts::PI) * a.sin()
                        + (1.0 - 2.0 / std::f64::consts::PI) * a.cos());
            }
            let pt = std::f64::consts::PI * t;
            ((pt * (1.0 - alpha)).sin() + 4.0 * alpha * t * (pt * (1.0 + alpha)).cos())
                / (pt * denom)
        })
        .collect()
}

fn convolve(x: &[Complex64], taps: &[f64]) -> Vec<Complex64> {
    let d = taps.len() / 2;
    (0..x.len())
        .map(|i| {
            let mut acc = Complex64::new(0.0, 0.0);
            for (m, w) in taps.iter().enumerate() {
                let j = i as isize + d as isize - m as isize;
                if j >= 0 && (j as usize) < x.len() {
                    acc += x[j as usize] * *w;
                }
            }
            acc
        })
        .collect()
}

/// Oerder–Meyr square-law timing line: its magnitude measures how well the symbol clock shows in
/// the envelope, and its angle gives the sampling phase. Modulation-independent, so it is the right
/// thing to pick the receive filter and the timing with **before** any hypothesis is considered.
fn timing_line(x: &[Complex64], sps: f64) -> Complex64 {
    let mut acc = Complex64::new(0.0, 0.0);
    for (n, s) in x.iter().enumerate() {
        let a = -std::f64::consts::TAU * n as f64 / sps;
        acc += Complex64::new(a.cos(), a.sin()) * s.norm_sqr();
    }
    acc / x.len() as f64
}

/// Symbol-rate samples: RRC-matched, timed, unit mean power. The roll-off and the timing phase are
/// nuisance parameters estimated once here, identically for every hypothesis.
fn symbol_samples(x: &[Complex64], sps: f64) -> Option<Vec<Complex64>> {
    let (filtered, line) = ALPHA_GRID
        .iter()
        .map(|a| {
            let y = convolve(x, &rrc(sps, *a, 6));
            let l = timing_line(&y, sps);
            (y, l)
        })
        .max_by(|a, b| a.1.norm().total_cmp(&b.1.norm()))?;
    let tau = -line.arg() / std::f64::consts::TAU * sps;
    // The matched filter's edge transient, in samples: not part of the emission.
    let edge = (2.0 * sps).ceil();
    // **The sampling grid's origin is the symbol phase `tau`, so the transient has to be skipped
    // in whole symbol periods** (T-291). Adding it in samples — `tau + k*sps + ceil(2*sps)`, which
    // is what this did — displaces *every* symbol in the record by `edge mod sps`: a constant
    // timing error of up to a full symbol, since `sps` is `sample_rate / (C14 symbol rate)` and is
    // an integer essentially never.
    //
    // Measured on a clean control at 30 dB, where 1 N₀ is 3.16 % EVM: 12.4 % at sps 6.1, 12.2 % at
    // 6.2, 8.4 % at 6.3, 10.4 % at 6.73 — and 1.5 % once the guard is aligned, at every one of
    // them. The error tracks `edge mod sps` exactly, which is what identifies it: it is zero at
    // sps 6.0 (guard 12 = 2 symbols) and *also* at sps 6.5 (guard 13 = 2 symbols), so "the period
    // happened to be an integer" is not the explanation — being a whole number of symbols is.
    let skip = ((edge - tau) / sps).ceil().max(0.0);
    let mut out = Vec::new();
    let mut k = 0usize;
    loop {
        let t = tau + (skip + k as f64) * sps;
        if t + 1.0 >= filtered.len() as f64 - edge {
            break;
        }
        // Linear interpolation: the symbol instant is not on the sample grid.
        let i = t.floor().max(0.0) as usize;
        let frac = t - i as f64;
        out.push(filtered[i] * (1.0 - frac) + filtered[i + 1] * frac);
        k += 1;
    }
    if out.len() < MIN_SYMBOLS {
        return None;
    }
    let out = remove_residual_cfo(&out);
    let p = out.iter().map(Complex64::norm_sqr).sum::<f64>() / out.len() as f64;
    if !(p.is_finite() && p > 0.0) {
        return None;
    }
    let g = 1.0 / p.sqrt();
    Some(out.into_iter().map(|s| s * g).collect())
}

/// Order the residual-carrier line is raised to. 8 is the least common multiple of the alphabet
/// sizes this stage scores (2, 4, 8), so one line serves every hypothesis and no hypothesis is
/// fitted a frequency of its own — the same rule the roll-off and the timing phase already obey.
const CFO_ORDER: u32 = 8;

/// How finely the [`CFO_ORDER`]-th power line is searched, as a multiple of the DFT bin width over
/// the symbol record. Four is enough that the parabolic refinement below lands within a few parts
/// in 10^4 of a symbol rate, which is two orders below the smallest residual that matters here.
const CFO_OVERSAMPLE: usize = 4;

/// Removes the residual carrier **frequency** offset from symbol-rate samples.
///
/// # Why this exists (T-246)
///
/// The ALRT below estimates one constant phase `θ` per hypothesis from an M-th-power sum. A
/// constant phase is not what the stage input carries. Measured at the stage input on the blind
/// grid (`t246_stage_input_carries_residual_cfo_not_timing_error`), the symbol clock is *exact* —
/// C14's samples-per-symbol matches the generator's to within ±21 ppm, a total slip of under
/// 0.04 symbols across the 256-symbol window — while the residual carrier offset is **0.4 % to
/// 3.6 % of the symbol rate** (up to 2.3 kHz), which is 1 to 9 whole rotations across that same
/// window. A single constant `θ` cannot remove that: the constellation is smeared into a ring, and
/// a ring fits the densest alphabet best for a reason that has nothing to do with the modulation.
///
/// That is exactly what the monotone behaviour T-243 found was: before this correction the ALRT's
/// arg-max was `8psk` on 24 of 36 blind snippets and on 16 of the 24 that were genuinely `bpsk` or
/// `qpsk`; the own-constellation residual of a *genuine* member ran 1.7–267 N₀ where a synced
/// receiver sits near 1. After it, the residual is 0.67–2.6 N₀ and the arg-max is the truth on
/// **36 of 36**. So the likelihood was never mis-derived; it was being fed a ring.
///
/// # How
///
/// `y^8` collapses BPSK, QPSK and 8-PSK alike to a single point, so the modulation cancels and what
/// is left is a tone at eight times the residual offset. Its frequency is found by maximising
/// `|Σ_k y_k^8 e^{−j2πνk}|` over `ν ∈ [−½, ½)` — a coarse search at [`CFO_OVERSAMPLE`] times the bin
/// width, then one parabolic refinement. Maximising rather than differencing matters: `ν = 0` is in
/// the search space, so a snippet with no offset is left alone instead of being handed the noise of
/// a lag-1 phase estimate (a differential estimator took one 20 dB `bpsk` snippet from 1.7 N₀ to
/// 13.4 N₀ and flipped its call to `8psk`).
///
/// The unambiguous range is `±1/16` of a symbol rate — 6.25 %, comfortably above the 3.6 % worst
/// case observed and above anything a detection box that centred the emission at all would leave.
fn remove_residual_cfo(y: &[Complex64]) -> Vec<Complex64> {
    let z: Vec<Complex64> = y.iter().map(|v| v.powu(CFO_ORDER)).collect();
    let n = z.len();
    let grid = CFO_OVERSAMPLE * n;
    let line = |nu: f64| -> f64 {
        let mut acc = Complex64::new(0.0, 0.0);
        for (k, v) in z.iter().enumerate() {
            let ph = -std::f64::consts::TAU * nu * k as f64;
            acc += *v * Complex64::new(ph.cos(), ph.sin());
        }
        acc.norm()
    };
    let step = 1.0 / grid as f64;
    let mut best_g = 0usize;
    let mut best_m = f64::NEG_INFINITY;
    let mut mags = Vec::with_capacity(grid);
    for g in 0..grid {
        let m = line(g as f64 * step - 0.5);
        if m > best_m {
            best_m = m;
            best_g = g;
        }
        mags.push(m);
    }
    // Parabolic refinement on the three samples around the peak (wrapping, since the grid is
    // periodic in nu).
    let lo = mags[(best_g + grid - 1) % grid];
    let hi = mags[(best_g + 1) % grid];
    let den = lo - 2.0 * best_m + hi;
    let delta = if den.abs() > 0.0 {
        (0.5 * (lo - hi) / den).clamp(-0.5, 0.5)
    } else {
        0.0
    };
    let nu = (best_g as f64 + delta) * step - 0.5;
    if !nu.is_finite() {
        return y.to_vec();
    }
    let f = nu / f64::from(CFO_ORDER);
    y.iter()
        .enumerate()
        .map(|(k, s)| {
            let ph = -std::f64::consts::TAU * f * k as f64;
            *s * Complex64::new(ph.cos(), ph.sin())
        })
        .collect()
}

/// Mean per-symbol log-likelihood of each candidate `psk-qam` class.
fn psk_qam_loglikelihoods(
    candidates: &[LabelP],
    x: &[Complex64],
    sps: f64,
    snr_db: Option<f64>,
) -> Option<Vec<(String, f64)>> {
    let y = symbol_samples(x, sps)?;
    // Known-SNR ALRT: N₀ from the measurement, clamped to the range where an in-band SNR is
    // informative about E_s/N₀ at all.
    let snr = snr_db
        .unwrap_or(SNR_CLAMP_DB.0)
        .clamp(SNR_CLAMP_DB.0, SNR_CLAMP_DB.1);
    let n0 = 10f64.powf(-snr / 10.0);
    // The samples were normalised to unit *received* power, which includes the noise.
    let scale = (1.0 - n0).max(0.1).sqrt();

    let mut out = Vec::new();
    for lp in candidates {
        let Some(points) = constellation(&lp.label) else {
            continue;
        };
        let m = points.len();
        // GLRT over the unknown phase: the M-th power of an M-PSK constellation collapses to a
        // single point, so its argument estimates M·θ. Square QAM's fourth power lands on the
        // negative real axis, hence the π.
        let order = if m == 2 || m == 4 || m == 8 { m } else { 4 };
        let offset = if m == 2 || m == 4 || m == 8 {
            0.0
        } else {
            std::f64::consts::PI
        };
        let sum: Complex64 = y.iter().map(|s| s.powu(order as u32)).sum();
        let theta = if sum.norm() > 0.0 {
            (sum.arg() - offset) / order as f64
        } else {
            0.0
        };
        let rot = Complex64::new(theta.cos(), -theta.sin());
        let mut ll = 0.0;
        for s in &y {
            let r = *s * rot;
            // log (1/M) Σ_m exp(−|r − s_m|²/N₀), by log-sum-exp so it cannot underflow.
            let mut best = f64::NEG_INFINITY;
            let mut terms = Vec::with_capacity(m);
            for p in &points {
                let e = -(r - *p * scale).norm_sqr() / n0;
                best = best.max(e);
                terms.push(e);
            }
            let acc: f64 = terms.iter().map(|e| (e - best).exp()).sum();
            ll += best + acc.ln() - (m as f64).ln();
        }
        out.push((lp.label.clone(), ll / y.len() as f64));
    }
    (out.len() >= 2).then_some(out)
}

// -------------------------------------------------------------------------------------------
// fsk: GLRT over the frequency-pulse shape.
// -------------------------------------------------------------------------------------------

/// The frequency-pulse hypothesis a class stands for.
#[derive(Clone, Copy, Debug)]
struct FskModel {
    /// Alphabet size.
    levels: usize,
    /// `Some(bt)` for a Gaussian frequency pulse, `None` for an ideal rectangular one.
    bt: Option<f64>,
    /// Whether `bt` is a free parameter fitted over [`BT_GRID`] (the `gfsk` hypothesis) or fixed by
    /// the class (the CPFSK ones, at [`CHANNEL_BT`]).
    free_bt: bool,
    /// Peak frequency deviation in rad/sample when the modulation index is fixed by the class
    /// (`msk`: `h = 0.5`), or `None` when it is a free parameter fitted by least squares.
    fixed_peak: Option<f64>,
}

fn fsk_model(label: &str) -> Option<FskModel> {
    Some(match label {
        "2fsk" => FskModel {
            levels: 2,
            bt: Some(CHANNEL_BT),
            free_bt: false,
            fixed_peak: None,
        },
        // **`msk` is not testable here, so this stage does not test it** (T-422).
        //
        // MSK *is* h = 0.5 (ADR-0016 §1), so the hypothesis was `fixed_peak = π/(2·sps)` rad/sample
        // — peak deviation Rs/4 — and fixing it was the whole content of the hypothesis. That value
        // is the **transmitter's** deviation. The GLRT sees the deviation this receiver measures,
        // after C13's ±0.75 × OBW99 channel filter and with `sps` coming from C14's rate estimate,
        // and those are not the same number.
        //
        // The proof does not need the true value, only its own twin: `2fsk` is the identical
        // hypothesis — two levels, rectangular pulse at [`CHANNEL_BT`] — differing *only* in fitting
        // the deviation instead of fixing it. On the blind acceptance grid at gate+5/+10, `2fsk`
        // beat `msk` on genuine MSK on 22 of 22 snippets, by 0.13–0.92 nats/symbol, **after** the
        // [`mdl_penalty`] that charges it half a `ln n` for that extra freedom. A constraint that
        // costs more residual than its own description length is buying is a constraint on the
        // wrong quantity.
        //
        // The consequence, measured: the GLRT's arg-max on genuine MSK was `gfsk` 19 times in 22
        // (`gfsk` fits the filtered trajectory better still), and the verifier re-ranked `msk` away
        // on most of them — class top-1 0.917 from the tree down to 0.375 after this stage. The
        // GLRT remains right on the pair ADR-0016 §4.5 names and `verifier_gain` measures: genuine
        // `2fsk` → `2fsk` 21/23, genuine `gfsk` → `gfsk` 20/21. It is only `msk` it cannot rank.
        //
        // There is no fix inside this stage. Freeing the deviation makes `msk` *identical* to
        // `2fsk`, so the hypothesis would stop existing; the index can only be tested against a
        // known received scale, which this GLRT does not have. Returning `None` leaves `msk` out of
        // the hypothesis set, and [`bounded_update`] then carries its prior through untouched — so
        // the call falls to the one measurement that does resolve h at this geometry, C14's index
        // ([`crate::tree::apply_msk_index`], dev-measured `h ∈ [0.500, 0.533]` on a genuine MSK).
        "msk" => return None,
        "gfsk" => FskModel {
            levels: 2,
            bt: Some(BT_GRID[0]),
            free_bt: true,
            fixed_peak: None,
        },
        "4fsk" => FskModel {
            levels: 4,
            bt: Some(CHANNEL_BT),
            free_bt: false,
            fixed_peak: None,
        },
        _ => return None,
    })
}

/// Instantaneous frequency in rad/sample, with the residual carrier offset removed.
fn instantaneous_frequency(x: &[Complex64]) -> Vec<f64> {
    let mut f: Vec<f64> = x.windows(2).map(|w| (w[1] * w[0].conj()).arg()).collect();
    let mean = f.iter().sum::<f64>() / f.len().max(1) as f64;
    for v in f.iter_mut() {
        *v -= mean;
    }
    f
}

/// The sampled frequency pulse, centred on zero: a rectangle of one symbol, optionally smoothed by
/// a Gaussian of the given `BT` (the standard GFSK definition).
fn frequency_pulse(sps: f64, bt: Option<f64>) -> (Vec<f64>, usize) {
    let half_rect = (sps / 2.0).round().max(1.0) as isize;
    let Some(bt) = bt else {
        let taps: Vec<f64> = (-half_rect..half_rect).map(|_| 1.0).collect();
        let half = taps.len() / 2;
        return (taps, half);
    };
    let sigma = (sps * 0.5 / (std::f64::consts::TAU * bt)).max(0.3);
    let half_g = (3.0 * sigma).ceil() as isize;
    let half = half_rect + half_g;
    let taps: Vec<f64> = (-half..=half)
        .map(|i| {
            // Rectangle convolved with the Gaussian, evaluated by direct summation.
            let mut acc = 0.0;
            let mut norm = 0.0;
            for j in -half_g..=half_g {
                let w = (-0.5 * (j as f64 / sigma).powi(2)).exp();
                norm += w;
                let t = i - j;
                if (-half_rect..half_rect).contains(&t) {
                    acc += w;
                }
            }
            acc / norm
        })
        .collect();
    let mid = taps.len() / 2;
    (taps, mid)
}

/// Mean per-symbol log-likelihood of each candidate `fsk` class, by GLRT over the frequency pulse.
fn fsk_loglikelihoods(
    candidates: &[LabelP],
    x: &[Complex64],
    sps: f64,
) -> Option<Vec<(String, f64)>> {
    let f = instantaneous_frequency(x);
    let guard = (3.0 * sps).ceil() as usize;
    if f.len() < 2 * guard + (MIN_SYMBOLS as f64 * sps) as usize {
        return None;
    }
    // One evaluation window, shared by every hypothesis: a residual compared over different spans
    // would compare nothing.
    let lo = guard;
    let hi = f.len() - guard;
    let n = hi - lo;
    let n_sym = (n as f64 / sps).floor();
    if n_sym < MIN_SYMBOLS as f64 {
        return None;
    }

    let mut out = Vec::new();
    for lp in candidates {
        let Some(model) = fsk_model(&lp.label) else {
            continue;
        };
        let bts: Vec<Option<f64>> = if model.free_bt {
            BT_GRID.iter().map(|b| Some(*b)).collect()
        } else {
            vec![model.bt]
        };
        let mut best = f64::NEG_INFINITY;
        for bt in bts {
            let (pulse, centre) = frequency_pulse(sps, bt);
            let energy: f64 = pulse.iter().map(|w| w * w).sum();
            if energy <= 0.0 {
                continue;
            }
            for step in 0..TAU_STEPS {
                let tau = step as f64 * sps / TAU_STEPS as f64;
                let resid = fit_residual(&f, lo, hi, &pulse, centre, sps, tau, &model, energy);
                let Some(sigma2) = resid else { continue };
                // Gaussian log-likelihood per sample, less the description length of the free
                // parameters and of the symbol alphabet (MDL): without the alphabet term a
                // four-level model, whose alphabet contains the binary one, could never lose.
                let ll = -0.5 * sigma2.max(1e-18).ln() * n as f64;
                let penalty = mdl_penalty(&model, n, n_sym, model.free_bt);
                best = best.max((ll - penalty) / n_sym);
            }
        }
        if best.is_finite() {
            out.push((lp.label.clone(), best));
        }
    }
    (out.len() >= 2).then_some(out)
}

/// Description length, in nats, of what a hypothesis fitted rather than predicted: half a
/// `ln n` per free continuous nuisance parameter (BIC), plus `ln levels` per symbol for the
/// alphabet a GLRT hard-decides for free.
fn mdl_penalty(model: &FskModel, n: usize, n_sym: f64, free_bt: bool) -> f64 {
    let mut free = 1.0; // the timing phase, always fitted
    if model.fixed_peak.is_none() {
        free += 1.0; // the deviation
    }
    if free_bt {
        free += 1.0; // the Gaussian BT
    }
    0.5 * free * (n as f64).ln() + n_sym * (model.levels as f64).ln()
}

/// Residual variance of the best fit of one pulse hypothesis at one timing phase: symbols
/// hard-decided against the level alphabet, amplitude by least squares (or fixed by the class).
#[allow(clippy::too_many_arguments)]
fn fit_residual(
    f: &[f64],
    lo: usize,
    hi: usize,
    pulse: &[f64],
    centre: usize,
    sps: f64,
    tau: f64,
    model: &FskModel,
    energy: f64,
) -> Option<f64> {
    // Symbol centres covering the evaluation window, plus one either side so the pulse tails of
    // the edge symbols are modelled rather than left as residual.
    let first = ((lo as f64 - tau) / sps).floor() as isize - 1;
    let last = ((hi as f64 - tau) / sps).ceil() as isize + 1;
    let mut centres = Vec::new();
    for k in first..=last {
        centres.push(tau + k as f64 * sps);
    }
    if centres.len() < 4 {
        return None;
    }

    // Matched projection of the IF onto the pulse at each symbol centre.
    let project = |c: f64| -> f64 {
        let mut acc = 0.0;
        for (m, w) in pulse.iter().enumerate() {
            let i = (c + m as f64 - centre as f64).round();
            if i >= 0.0 && (i as usize) < f.len() {
                acc += f[i as usize] * w;
            }
        }
        acc / energy
    };
    let s: Vec<f64> = centres.iter().map(|c| project(*c)).collect();

    // Symmetric levels −1 … +1.
    let levels: Vec<f64> = (0..model.levels)
        .map(|i| 2.0 * (i as f64 / (model.levels - 1).max(1) as f64) - 1.0)
        .collect();

    // Amplitude: fixed by the class, or bootstrapped from the projections and refined once.
    let mut amp = match model.fixed_peak {
        Some(a) => a,
        None => {
            let mut mags: Vec<f64> = s.iter().map(|v| v.abs()).collect();
            mags.sort_by(f64::total_cmp);
            let p90 = mags[(mags.len() as f64 * 0.9) as usize % mags.len()];
            if p90 <= 0.0 {
                return None;
            }
            p90
        }
    };
    let mut decided: Vec<f64> = Vec::new();
    for _ in 0..2 {
        decided = s
            .iter()
            .map(|v| {
                let t = v / amp;
                *levels
                    .iter()
                    .min_by(|a, b| (*a - t).abs().total_cmp(&(*b - t).abs()))
                    .unwrap_or(&0.0)
            })
            .collect();
        if model.fixed_peak.is_none() {
            let num: f64 = s.iter().zip(&decided).map(|(v, a)| v * a).sum();
            let den: f64 = decided.iter().map(|a| a * a).sum();
            if den <= 0.0 {
                return None;
            }
            amp = num / den;
            if !(amp.is_finite() && amp.abs() > 0.0) {
                return None;
            }
        }
    }

    // Reconstruct and measure the residual over the shared window.
    let mut model_if = vec![0.0_f64; f.len()];
    for (c, a) in centres.iter().zip(&decided) {
        for (m, w) in pulse.iter().enumerate() {
            let i = (c + m as f64 - centre as f64).round();
            if i >= 0.0 && (i as usize) < model_if.len() {
                model_if[i as usize] += amp * a * w;
            }
        }
    }
    let mut acc = 0.0;
    for i in lo..hi {
        let e = f[i] - model_if[i];
        acc += e * e;
    }
    let sigma2 = acc / (hi - lo) as f64;
    sigma2.is_finite().then_some(sigma2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classifier::{Classifier, ClassifyRequest};
    use crate::symbols::SymbolEstimator;
    use crate::synth::{ACCEPTANCE_SEED_BASE, Class, SynthConfig, generate};
    use hk_model::Timestamp;

    // -------------------------------------------------------------------------------------
    // T-246: the psk-qam ALRT, and the stage input it is fed.
    // -------------------------------------------------------------------------------------

    /// The three PSK orders this stage scores, with the truth generator for each.
    const T246_PSK: [(Class, &str, u32); 3] = [
        (Class::Bpsk, "bpsk", 2),
        (Class::Qpsk, "qpsk", 4),
        (Class::Psk8, "8psk", 8),
    ];

    const T246_SEEDS: u64 = 6;
    const T246_SNRS: [f64; 2] = [20.0, 30.0];

    /// The generator's own symbol rate for `(class, seed)` — [`crate::synth::generate`]'s first
    /// draw, reproduced exactly. Used only to score the receiver's estimate, never fed to it.
    fn t246_true_rate(class: Class, seed: u64) -> f64 {
        let mut rng =
            hk_dsp::synth::Rng::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ class as u64);
        25e3 + 75e3 * rng.unit()
    }

    /// Everything the verifier would see for one snippet, plus the truth it is scored against.
    struct T246Case {
        x: Vec<Complex64>,
        /// Samples per symbol the stage would use: C14's where it locked, the generator's where it
        /// did not, since the likelihood is the thing under test here either way.
        ///
        /// When T-246 measured this, genuine 8-PSK never locked at these SNRs — which is the
        /// finding T-246 handed on as T-589, and T-589 fixed: C14's digital-structure gate could
        /// not see an alphabet that collapses at the eighth power. All twelve lock now, so the
        /// fallback no longer fires on this grid.
        sps: f64,
        sps_true: f64,
        locked: bool,
        fs: f64,
        n0: f64,
    }

    fn t246_case(class: Class, snr_db: f64, seed: u64) -> T246Case {
        let s = generate(class, &SynthConfig::new(snr_db, seed));
        let mut c14 = SymbolEstimator::new();
        let symbols = c14.from_samples(
            &s.symbol_samples,
            s.symbol_sample_rate_hz,
            Some(s.obw_hz),
            Some(snr_db),
        );
        let locked = symbols
            .as_ref()
            .filter(|x| x.rate_trusted())
            .and_then(|x| x.symbol_rate_bd.value())
            .filter(|r| *r > 0.0);
        let fs = s.symbol_sample_rate_hz;
        let sps_true = fs / t246_true_rate(class, seed);
        let sps = locked.map_or(sps_true, |r| fs / r);
        let want = (sps * MAX_SYMBOLS as f64).ceil() as usize;
        let x = s.symbol_samples[..s.symbol_samples.len().min(want)]
            .iter()
            .map(|c| Complex64::new(f64::from(c.re), f64::from(c.im)))
            .collect();
        T246Case {
            x,
            sps,
            sps_true,
            locked: locked.is_some(),
            fs,
            n0: 10f64.powf(-snr_db.clamp(SNR_CLAMP_DB.0, SNR_CLAMP_DB.1) / 10.0),
        }
    }

    /// Frequency of the `m`-th power line of `x`, in Hz, with the line's coherence.
    fn t246_cfo_hz(x: &[Complex64], m: u32, fs: f64) -> (f64, f64) {
        let mut acc = Complex64::new(0.0, 0.0);
        let mut p = 0.0;
        for i in 1..x.len() {
            let a = x[i].powu(m);
            let b = x[i - 1].powu(m);
            acc += a * b.conj();
            p += a.norm_sqr();
        }
        let coh = if p > 0.0 { acc.norm() / p } else { 0.0 };
        (acc.arg() / std::f64::consts::TAU * fs / f64::from(m), coh)
    }

    /// **Step one of T-246, and it decides the rest**: a smeared constellation and a mis-derived
    /// likelihood look identical at the output, so measure which one this stage is handed.
    ///
    /// The answer, on the blind grid: the **symbol clock is exact** and the **carrier is not**.
    #[test]
    fn t246_stage_input_carries_residual_cfo_not_timing_error() {
        let mut worst_ppm: f64 = 0.0;
        let mut worst_slip: f64 = 0.0;
        let mut worst_cfo_norm: f64 = 0.0;
        let mut n_locked = 0;
        let mut n = 0;
        for (class, _, m) in T246_PSK {
            for snr in T246_SNRS {
                for k in 0..T246_SEEDS {
                    let seed = ACCEPTANCE_SEED_BASE + 900 + k;
                    let c = t246_case(class, snr, seed);
                    n += 1;
                    if c.locked {
                        n_locked += 1;
                        let ppm = (c.sps - c.sps_true) / c.sps_true * 1e6;
                        let slip = (c.x.len() as f64 / c.sps).floor() * (c.sps - c.sps_true);
                        worst_ppm = worst_ppm.max(ppm.abs());
                        worst_slip = worst_slip.max(slip.abs());
                    }
                    let (cfo_hz, coh) = t246_cfo_hz(&c.x, m, c.fs);
                    let rate = c.fs / c.sps_true;
                    // Only count a line the estimator can actually see.
                    if coh > 0.5 {
                        worst_cfo_norm = worst_cfo_norm.max((cfo_hz / rate).abs());
                    }
                }
            }
        }
        assert_eq!(n, 36, "grid size");
        // 23 before T-589 (11 bpsk, 12 qpsk, 0 of the 12 genuine 8-PSK); 35 after it.
        assert!(n_locked >= 35, "C14 locked on only {n_locked} of {n}");
        // Timing: the symbol clock is right to parts per million, and the record never slips by a
        // tenth of a symbol end to end. Nothing here can smear a constellation.
        assert!(
            worst_ppm < 100.0,
            "worst symbol-rate error {worst_ppm:.0} ppm — the diagnosis assumed it was negligible"
        );
        assert!(
            worst_slip < 0.1,
            "worst end-to-end slip {worst_slip:.3} symbols"
        );
        // Carrier: whole rotations across the same window. 0.4 % of the symbol rate over 256
        // symbols is one full turn; the worst here is an order above that.
        assert!(
            worst_cfo_norm > 0.004,
            "worst residual CFO {worst_cfo_norm:.5} of the symbol rate — if this is really \
             negligible the T-246 diagnosis is wrong and the likelihood is the suspect again"
        );
        println!(
            "T-246 stage input over {n} snippets ({n_locked} with a C14 lock): \
             worst symbol-rate error {worst_ppm:.0} ppm, worst end-to-end slip {worst_slip:.3} \
             symbols, worst residual CFO {:.2} % of the symbol rate",
            worst_cfo_norm * 100.0
        );
    }

    /// **The defect itself (T-246).** The psk-qam ALRT must rank by the data, not by alphabet size.
    ///
    /// Before [`remove_residual_cfo`] this failed exactly as the ticket describes: the arg-max was
    /// `8psk` on 24 of 36 snippets, including 16 of the 24 that were genuinely `bpsk` or `qpsk`.
    ///
    /// The test is deliberately run **at the likelihood**, with all three orders as candidates, so
    /// it cannot pass by the verifier skipping: `psk_qam_loglikelihoods` either scores all three or
    /// the case is counted as not run and the count assertion below fails.
    #[test]
    fn the_psk_qam_alrt_ranks_by_the_data_not_by_constellation_size() {
        let candidates: Vec<LabelP> = T246_PSK
            .iter()
            .map(|(_, l, _)| LabelP {
                label: (*l).to_owned(),
                p: 1.0 / 3.0,
            })
            .collect();
        let mut ran = 0;
        let mut right = 0;
        let mut largest = 0;
        let mut wrong: Vec<String> = Vec::new();
        for (class, truth, _) in T246_PSK {
            for snr in T246_SNRS {
                for k in 0..T246_SEEDS {
                    let seed = ACCEPTANCE_SEED_BASE + 900 + k;
                    let c = t246_case(class, snr, seed);
                    let Some(scores) = psk_qam_loglikelihoods(
                        &candidates,
                        &c.x,
                        c.sps,
                        Some(-10.0 * c.n0.log10()),
                    ) else {
                        continue;
                    };
                    assert_eq!(scores.len(), 3, "{truth}/{snr}/{seed}: hypotheses scored");
                    ran += 1;
                    let best = scores
                        .iter()
                        .max_by(|a, b| a.1.total_cmp(&b.1))
                        .expect("non-empty");
                    if best.0 == "8psk" {
                        largest += 1;
                    }
                    if best.0 == truth {
                        right += 1;
                    } else {
                        wrong.push(format!(
                            "{truth} {snr:.0} dB seed {seed} -> {} [{}]",
                            best.0,
                            scores
                                .iter()
                                .map(|(l, v)| format!("{l}:{v:.2}"))
                                .collect::<Vec<_>>()
                                .join(" ")
                        ));
                    }
                }
            }
        }
        println!(
            "T-246 ALRT: ran on {ran} of 36 snippets, arg-max correct {right}, \
             arg-max = largest constellation {largest} (12 of those are genuinely 8psk)"
        );
        assert_eq!(ran, 36, "the ALRT must actually have run on every snippet");
        // The monotone failure, stated directly: `8psk` may win only where `8psk` is the truth.
        assert_eq!(
            largest, 12,
            "the ALRT preferred the largest constellation on {largest} of 36 snippets; only the 12 \
             genuine 8-PSK ones may: {wrong:#?}"
        );
        assert_eq!(right, 36, "arg-max wrong on {}: {wrong:#?}", 36 - right);
    }

    /// The same property **through [`verify`]**, in the condition where it actually runs: a tree
    /// call left undecided between two orders, both above [`CANDIDATE_MIN_P`].
    ///
    /// This is the case the ticket warns is masked today — after T-243 a real tree call is decisive
    /// enough that the runner-up falls below the floor and the stage skips with
    /// [`SkipReason::SingleCandidate`], so on/off look identical. Forcing the prior is what makes
    /// the test non-vacuous, and the `Ran` assertion is what proves it did.
    #[test]
    fn a_two_candidate_prior_is_reranked_to_the_truth_not_to_the_larger_alphabet() {
        let mut ran = 0;
        for (class, truth, _) in [T246_PSK[0], T246_PSK[1]] {
            for snr in T246_SNRS {
                for k in 0..T246_SEEDS {
                    let seed = ACCEPTANCE_SEED_BASE + 900 + k;
                    let s = generate(class, &SynthConfig::new(snr, seed));
                    let mut c14 = SymbolEstimator::new();
                    let symbols = c14.from_samples(
                        &s.symbol_samples,
                        s.symbol_sample_rate_hz,
                        Some(s.obw_hz),
                        Some(snr),
                    );
                    let mut req =
                        ClassifyRequest::new(&s.samples, s.sample_rate_hz, Timestamp::UNIX_EPOCH);
                    req.obw_hz = Some(s.obw_hz);
                    req.snr_db = Some(snr);
                    req.symbols = symbols.as_ref();
                    let mut c = Classifier::new().classify(&req);
                    if c.family != "psk-qam" || c.class.is_none() {
                        continue;
                    }
                    // An undecided tree call: the truth and the largest alphabet, level pegging.
                    // Nothing else changes, and both are labels the tree's own family offers.
                    if let Some(call) = c.class.as_mut() {
                        call.dist = vec![
                            LabelP {
                                label: truth.to_owned(),
                                p: 0.5,
                            },
                            LabelP {
                                label: "8psk".to_owned(),
                                p: 0.5,
                            },
                        ];
                        call.label = "8psk".to_owned();
                        call.p = 0.5;
                    }
                    let outcome = verify(
                        &mut c,
                        &VerifyInput {
                            samples: &s.symbol_samples,
                            sample_rate_hz: s.symbol_sample_rate_hz,
                            symbols: symbols.as_ref(),
                            snr_db: Some(snr),
                        },
                    );
                    let VerifyOutcome::Ran { scores, from, to } = &outcome else {
                        // Only a clock lock may excuse a skip here; everything else means the test
                        // stopped exercising the stage.
                        assert_eq!(
                            outcome,
                            VerifyOutcome::Skipped(SkipReason::NoClockLock),
                            "{truth} {snr} {seed}"
                        );
                        continue;
                    };
                    ran += 1;
                    assert_eq!(from, "8psk", "the forced prior");
                    assert_eq!(
                        to, truth,
                        "{truth} {snr} dB seed {seed}: verifier kept/chose {to} with {scores:?}"
                    );
                    let call = c.class.as_ref().expect("class call");
                    assert_eq!(call.stage, Stage::Verifier);
                    assert!(call.p <= MAX_CONFIDENCE + 1e-9);
                }
            }
        }
        println!("T-246: the verifier ran on {ran} of 24 forced two-candidate snippets");
        assert!(
            ran >= 20,
            "the verifier only ran {ran} times — a green run that never executed the stage is the \
             failure this test exists to prevent"
        );
    }

    /// Classifies one generated waveform with the verifier wired in, returning the classification
    /// both before and after it ran.
    fn both(class: Class, snr_db: f64, seed: u64) -> (Classification, Classification) {
        let s = generate(class, &SynthConfig::new(snr_db, seed));
        let mut c14 = SymbolEstimator::new();
        let symbols = c14.from_samples(
            &s.symbol_samples,
            s.symbol_sample_rate_hz,
            Some(s.obw_hz),
            Some(snr_db),
        );
        let mut req = ClassifyRequest::new(&s.samples, s.sample_rate_hz, Timestamp::UNIX_EPOCH);
        req.obw_hz = Some(s.obw_hz);
        req.snr_db = Some(snr_db);
        req.symbols = symbols.as_ref();
        let before = Classifier::new().classify(&req);
        req.symbol_samples = Some(&s.symbol_samples);
        req.symbol_sample_rate_hz = Some(s.symbol_sample_rate_hz);
        let after = Classifier::new().classify(&req);
        (before, after)
    }

    /// **The property the whole task turns on**: the verifier may reorder the classes the tree
    /// offered, and may do nothing else. Asserted over the grid rather than stated in a comment.
    #[test]
    fn it_can_only_rerank_never_promote_and_never_unabstain() {
        let mut ran = 0;
        for class in Class::TAXONOMY {
            let family = class.family().expect("taxonomy class");
            let gate = crate::thresholds::thresholds_of(family)
                .and_then(|t| t.snr_gate_db)
                .unwrap_or(10.0);
            for offset in [-5.0, 0.0, 5.0, 10.0] {
                for trial in 0u64..2 {
                    let seed = ACCEPTANCE_SEED_BASE + 700_000 + trial + 7 * *class as u64;
                    let (before, after) = both(*class, gate + offset, seed);
                    after.validate().expect("still a valid classification");

                    // 1. The family call, and everything that decides it, is untouched.
                    assert_eq!(before.family, after.family, "{}", class.label());
                    assert_eq!(before.confidence, after.confidence);
                    assert_eq!(before.posterior, after.posterior);
                    assert_eq!(before.likelihood, after.likelihood);
                    assert_eq!(before.open_set_score, after.open_set_score);
                    assert_eq!(before.coarse, after.coarse);
                    // 2. The row's arbitration stage is never promoted.
                    assert_eq!(after.stage, Stage::FeatureTree);

                    // 3. An abstention stays one: no class call appears where there was none.
                    match (&before.class, &after.class) {
                        (None, after_class) => assert!(
                            after_class.is_none(),
                            "{}: the verifier invented a class call",
                            class.label()
                        ),
                        (Some(b), Some(a)) => {
                            // 4. The label set is exactly the tree's — nothing added, nothing lost.
                            let mut before_labels: Vec<&str> =
                                b.dist.iter().map(|lp| lp.label.as_str()).collect();
                            let mut after_labels: Vec<&str> =
                                a.dist.iter().map(|lp| lp.label.as_str()).collect();
                            before_labels.sort_unstable();
                            after_labels.sort_unstable();
                            assert_eq!(before_labels, after_labels, "{}", class.label());
                            // 5. The class it names had prior mass: it was a candidate, never a
                            // promotion of something the tree ruled out.
                            let prior = b
                                .dist
                                .iter()
                                .find(|lp| lp.label == a.label)
                                .map(|lp| lp.p)
                                .unwrap_or(0.0);
                            if a.stage == Stage::Verifier {
                                ran += 1;
                                assert!(
                                    prior >= CANDIDATE_MIN_P,
                                    "{}: verifier named {} which the tree gave {prior:.3}",
                                    class.label(),
                                    a.label
                                );
                                // 6. Certainty is never manufactured *by this stage*. (The tree
                                // itself reports p = 1.00 for a one-class family such as `ofdm`,
                                // which is not a claim of certainty but an empty distribution.)
                                assert!(a.p <= MAX_CONFIDENCE + 1e-9, "class p {}", a.p);
                            }
                        }
                        (Some(_), None) => {
                            panic!("{}: the verifier removed a class call", class.label())
                        }
                    }
                }
            }
        }
        assert!(ran > 0, "the verifier never ran on the grid");
    }

    /// The gate itself: without a locked clock the verifier does not run, whatever the samples say.
    #[test]
    fn without_a_clock_lock_the_trees_ranking_stands() {
        let s = generate(
            Class::Fsk2,
            &SynthConfig::new(25.0, ACCEPTANCE_SEED_BASE + 11),
        );
        let mut c = Classifier::new().classify(&{
            let mut req = ClassifyRequest::new(&s.samples, s.sample_rate_hz, Timestamp::UNIX_EPOCH);
            req.obw_hz = Some(s.obw_hz);
            req.snr_db = Some(25.0);
            req
        });
        let before = c.clone();
        // No C14 estimate at all.
        let outcome = verify(
            &mut c,
            &VerifyInput {
                samples: &s.symbol_samples,
                sample_rate_hz: s.symbol_sample_rate_hz,
                symbols: None,
                snr_db: Some(25.0),
            },
        );
        assert_eq!(
            outcome,
            VerifyOutcome::Skipped(SkipReason::NoClockLock),
            "{outcome:?}"
        );
        assert_eq!(before, c, "a skipped verification changes nothing");
    }

    /// An abstaining classification is never turned into a claim.
    #[test]
    fn an_abstention_is_never_verified_into_a_claim() {
        // 5 dB is below every gate.
        let s = generate(
            Class::Fsk2,
            &SynthConfig::new(5.0, ACCEPTANCE_SEED_BASE + 12),
        );
        let mut c14 = SymbolEstimator::new();
        let symbols = c14.from_samples(
            &s.symbol_samples,
            s.symbol_sample_rate_hz,
            Some(s.obw_hz),
            Some(5.0),
        );
        let mut req = ClassifyRequest::new(&s.samples, s.sample_rate_hz, Timestamp::UNIX_EPOCH);
        req.obw_hz = Some(s.obw_hz);
        req.snr_db = Some(5.0);
        req.symbols = symbols.as_ref();
        let mut c = Classifier::new().classify(&req);
        assert_eq!(c.family, UNKNOWN);
        let before = c.clone();
        let outcome = verify(
            &mut c,
            &VerifyInput {
                samples: &s.symbol_samples,
                sample_rate_hz: s.symbol_sample_rate_hz,
                symbols: symbols.as_ref(),
                snr_db: Some(5.0),
            },
        );
        assert!(matches!(outcome, VerifyOutcome::Skipped(_)), "{outcome:?}");
        assert_eq!(before, c);
    }

    /// The bounded update: it reorders within the candidate set, keeps that set's mass, leaves
    /// non-candidates alone, and cannot exceed the 19:1 clamp.
    #[test]
    fn the_update_is_bounded_and_conserves_the_candidate_mass() {
        let prior = vec![
            LabelP {
                label: "2fsk".into(),
                p: 0.6,
            },
            LabelP {
                label: "gfsk".into(),
                p: 0.3,
            },
            LabelP {
                label: "msk".into(),
                p: 0.1,
            },
        ];
        // An overwhelming (and deliberately absurd) likelihood for gfsk.
        let scores = vec![("2fsk".to_owned(), -900.0), ("gfsk".to_owned(), 0.0)];
        let out = bounded_update(&prior, &scores);
        assert_eq!(out.label, "gfsk", "{:?}", out.dist);
        assert_eq!(out.stage, Stage::Verifier);
        // msk was not a candidate: untouched.
        let msk = out.dist.iter().find(|lp| lp.label == "msk").unwrap().p;
        assert!((msk - 0.1).abs() < 1e-9, "msk moved to {msk}");
        // The candidate block kept its 0.9, and the clamp held the odds at 19:1.
        let a = out.dist.iter().find(|lp| lp.label == "2fsk").unwrap().p;
        let b = out.dist.iter().find(|lp| lp.label == "gfsk").unwrap().p;
        assert!((a + b - 0.9).abs() < 1e-9, "candidate mass {}", a + b);
        let odds = (b / 0.3) / (a / 0.6);
        assert!(
            (odds - 19.0).abs() < 1e-6,
            "clamped odds ratio {odds} is not 19:1"
        );
        assert!(out.dist.iter().all(|lp| lp.p <= MAX_CONFIDENCE));
    }

    /// The root-raised-cosine pulse at `t` **symbols** from its centre, for a control waveform whose
    /// symbols sit at exact fractional positions (unlike `synth`, which rounds them to a sample).
    fn rrc_at(t: f64, alpha: f64) -> f64 {
        if t.abs() < 1e-9 {
            return 1.0 - alpha + 4.0 * alpha / std::f64::consts::PI;
        }
        let denom = 1.0 - (4.0 * alpha * t).powi(2);
        if denom.abs() < 1e-9 {
            let a = std::f64::consts::PI / (4.0 * alpha);
            return alpha / 2.0_f64.sqrt()
                * ((1.0 + 2.0 / std::f64::consts::PI) * a.sin()
                    + (1.0 - 2.0 / std::f64::consts::PI) * a.cos());
        }
        let pt = std::f64::consts::PI * t;
        ((pt * (1.0 - alpha)).sin() + 4.0 * alpha * t * (pt * (1.0 + alpha)).cos()) / (pt * denom)
    }

    /// **The eye is sampled on the symbol instants, at every samples-per-symbol** (T-291).
    ///
    /// [`symbol_samples`] skips the matched filter's edge transient before it starts sampling. That
    /// transient is a number of *samples*, but the grid it is added to is anchored on the **symbol
    /// phase** `tau` — so it has to be advanced to a whole number of symbol periods. Adding it in
    /// samples, which is what this did, displaced every symbol in the record by `edge mod sps`.
    ///
    /// `sps` is `sample_rate / (C14 symbol rate)`, so it is an integer essentially never, and the
    /// error is invisible to any test that happens to use an integer one. This drives a *clean*
    /// QPSK control — no CFO, no quantisation, no channel filter, symbols at exact fractional
    /// positions — through the real function at three non-integer `sps`, so the only thing it can
    /// measure is the sampling grid.
    ///
    /// At 30 dB a correctly synchronised member sits near 1 N₀ (measured 0.2). Before the fix these
    /// read 15.4, 7.0 and 11.0 N₀ — the constellation smeared into a ring, which is the mechanism
    /// T-286 attributed to residual carrier frequency offset and T-246 to the psk-qam ALRT. The
    /// bound below is a-priori: "within a small multiple of the noise it was given", with the
    /// failure mode an order of magnitude the other side of it.
    #[test]
    fn the_eye_is_sampled_on_the_symbol_instants_at_any_samples_per_symbol() {
        const SNR_DB: f64 = 30.0;
        const ALPHA: f64 = 0.35;
        const N_SYM: usize = 400;
        const SPAN: f64 = 6.0;
        let n0 = 10f64.powf(-SNR_DB / 10.0);

        for sps in [6.1_f64, 6.3, 6.733] {
            // A deterministic QPSK waveform: every symbol's pulse evaluated at its exact position.
            let mut state = 0x2545_F491_4F6C_DD1D_u64 ^ (sps.to_bits());
            let mut next = move || {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                state >> 11
            };
            let n = (N_SYM as f64 * sps).ceil() as usize;
            let mut x = vec![Complex64::new(0.0, 0.0); n];
            for k in 0..N_SYM {
                let a =
                    std::f64::consts::TAU * (next() % 4) as f64 / 4.0 + std::f64::consts::FRAC_PI_4;
                let s = Complex64::new(a.cos(), a.sin());
                let centre = k as f64 * sps;
                let lo = (centre - SPAN * sps).ceil().max(0.0) as usize;
                let hi = ((centre + SPAN * sps).floor() as usize).min(n.saturating_sub(1));
                for (i, out) in x.iter_mut().enumerate().take(hi + 1).skip(lo) {
                    *out += s * rrc_at((i as f64 - centre) / sps, ALPHA);
                }
            }
            let p = x.iter().map(Complex64::norm_sqr).sum::<f64>() / x.len() as f64;
            let g = 1.0 / p.sqrt();
            let sigma = (n0 / 2.0).sqrt();
            for s in x.iter_mut() {
                // Box-Muller, from the same stream.
                let u1 = ((next() % 1_000_000) as f64 + 1.0) / 1_000_001.0;
                let u2 = (next() % 1_000_000) as f64 / 1_000_000.0;
                let r = (-2.0 * u1.ln()).sqrt();
                let t = std::f64::consts::TAU * u2;
                *s = *s * g + Complex64::new(r * t.cos() * sigma, r * t.sin() * sigma);
            }

            let y = symbol_samples(&x, sps).expect("the control is long enough to sample");

            // Mean squared distance to the nearest QPSK point, in N₀, after the same fourth-power
            // phase alignment the ALRT uses.
            let points = constellation("qpsk").expect("qpsk has a constellation");
            let sum: Complex64 = y.iter().map(|s| s.powu(4)).sum();
            let theta = sum.arg() / 4.0;
            let rot = Complex64::new(theta.cos(), -theta.sin());
            let scale = (1.0 - n0).max(0.1).sqrt();
            let resid = y
                .iter()
                .map(|s| {
                    let r = *s * rot;
                    points
                        .iter()
                        .map(|p| (r - *p * scale).norm_sqr())
                        .fold(f64::INFINITY, f64::min)
                })
                .sum::<f64>()
                / y.len() as f64
                / n0;
            assert!(
                resid < 3.0,
                "sps {sps}: a clean QPSK control recovers at {resid:.2} N₀, so the eye is not \
                 being sampled on the symbol instants (guard {} mod sps = {:.3} samples)",
                (2.0 * sps).ceil(),
                (2.0 * sps).ceil() % sps
            );
        }
    }
}
