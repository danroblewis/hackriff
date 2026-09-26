//! Fusion of evidence with C17 band-plan priors (ADR-0016 §3). **Core interface.**
//!
//! `P(c ∣ x, f, ℓ) ∝ p(x ∣ c) · P(c ∣ f, ℓ)` over **known families only**. Three rules bound what
//! a prior may do, and each is a property test below:
//!
//! 1. **Priors never touch `unknown`.** Its posterior is `max(open_set_score, L[unknown])`,
//!    computed before the prior is applied, and capped at
//!    [`MAX_UNKNOWN_CONFIDENCE`](super::MAX_UNKNOWN_CONFIDENCE) (T-953); the known families share
//!    what is left, which is therefore never zero. That residual is **ranked only where the
//!    likelihood carries known mass**: `hk-classify` builds `L[unknown] = open_set` with the
//!    families sharing `1 − open_set`, so at a fully saturated `open_set = 1.0` every known
//!    likelihood is 0 and the residual is spread **uniformly** — the cap then makes the number
//!    honest, it does not recover a ranking the likelihood never had. For `open_set < 1` the
//!    families keep the likelihood's own order inside the residual.
//! 2. **λ₀ ≥ 0.1.** A prior set with less uniform mass is refused, so no family is ever driven to
//!    zero by a prior: a family the prior omits is floored at `λ₀/K`.
//! 3. **Evidence dominance.** When the likelihood top-1 beats the runner-up by
//!    [`EVIDENCE_DOMINANCE_RATIO`](super::thresholds::EVIDENCE_DOMINANCE_RATIO) or more, the
//!    posterior top **is** the likelihood top: the prior is tempered (`prior^τ`, τ bisected in
//!    [0, 1]) until that holds, and the row is flagged `prior-mismatch`.
//!
//! Why here and not in the classifier crate (T-218): ADR-0016's decision table puts `fuse` in
//! `hk_model::classify`, and both sides of the fusion need it — `hk-classify` produces the
//! likelihood, `hk-context` (T-212) produces the [`FamilyPriorSet`], and neither may depend on the
//! other. `hk_classify::fuse` re-exports this module, so the classifier's paths are unchanged.
//!
//! **Seam (T-212).** The prior *source* is C17's: `hk-context` supplies a [`FamilyPriorSet`] per
//! emitter extent from the allocation services (`fm-broadcast → analog`, `adsb → pulsed`, …).
//! Until then [`FamilyPriors`] has one trivial implementation ([`NoPriors`]) and one
//! test/configuration source ([`StaticPriors`]); the fusion itself is final and pure, so T-212
//! only has to build the set.

use super::thresholds::EVIDENCE_DOMINANCE_RATIO;
use super::{
    ClassFlag, LAMBDA0_MIN, LabelP, MAX_UNKNOWN_CONFIDENCE, PriorUse, SUM_TOLERANCE, UNKNOWN,
    max_confidence_of,
};

/// A C17 family prior: `P(family ∣ f, ℓ)` over known families with its mixture weights.
#[derive(Clone, Debug, PartialEq)]
pub struct FamilyPriorSet {
    /// Reference of the prior data (band table, licence extract) for provenance.
    pub prior_ref: String,
    /// `[λ₀ uniform, λ₁ allocation, λ₂ licence, λ₃ history]`, summing to 1, λ₀ ≥ 0.1.
    pub lambda: [f64; 4],
    /// The distribution. Families left out are floored at `λ₀ / K` when fusing.
    pub dist: Vec<LabelP>,
}

/// A prior set broke the contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidPrior(pub String);

impl std::fmt::Display for InvalidPrior {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid family prior: {}", self.0)
    }
}

impl std::error::Error for InvalidPrior {}

impl FamilyPriorSet {
    /// Checks the mixture weights (λ₀ ≥ 0.1, summing to 1) and the distribution (no `unknown`,
    /// no repeats, probabilities summing to 1).
    pub fn validate(&self) -> Result<(), InvalidPrior> {
        if self.prior_ref.trim().is_empty() {
            return Err(InvalidPrior("prior_ref is empty".into()));
        }
        let mut lambda_sum = 0.0;
        for l in self.lambda {
            if !l.is_finite() || l < 0.0 {
                return Err(InvalidPrior(format!("lambda {l} is invalid")));
            }
            lambda_sum += l;
        }
        if (lambda_sum - 1.0).abs() > SUM_TOLERANCE {
            return Err(InvalidPrior(format!("lambdas sum to {lambda_sum}")));
        }
        if self.lambda[0] < LAMBDA0_MIN {
            return Err(InvalidPrior(format!(
                "lambda0 {} is below {LAMBDA0_MIN}",
                self.lambda[0]
            )));
        }
        let mut sum = 0.0;
        for (i, lp) in self.dist.iter().enumerate() {
            if lp.label == UNKNOWN {
                return Err(InvalidPrior("a prior never carries unknown mass".into()));
            }
            if !(lp.p.is_finite() && (0.0..=1.0).contains(&lp.p)) {
                return Err(InvalidPrior(format!("prior p {} is not in [0, 1]", lp.p)));
            }
            if self.dist[..i].iter().any(|o| o.label == lp.label) {
                return Err(InvalidPrior(format!("label {:?} repeated", lp.label)));
            }
            sum += lp.p;
        }
        if (sum - 1.0).abs() > 1e-6 {
            return Err(InvalidPrior(format!("prior sums to {sum}")));
        }
        Ok(())
    }

    /// The prior of `family`, floored at the uniform component `λ₀ / k` so no prior can rule a
    /// family out (ADR-0016 §3).
    pub fn p_of(&self, family: &str, k: usize) -> f64 {
        let floor = if k == 0 {
            0.0
        } else {
            self.lambda[0] / k as f64
        };
        self.dist
            .iter()
            .find(|lp| lp.label == family)
            .map_or(floor, |lp| lp.p.max(floor))
    }

    /// The prior as stored on a [`super::Classification`].
    pub fn to_use(&self) -> PriorUse {
        PriorUse {
            prior_ref: self.prior_ref.clone(),
            lambda: self.lambda,
            dist: self.dist.clone(),
        }
    }
}

/// A source of C17 family priors for an emission's extent (T-212 implements the real one).
pub trait FamilyPriors: Send + Sync {
    /// The prior over families for the emission spanning `f_lo_hz..f_hi_hz`, or `None` when there
    /// is no reference data (the posterior is then the likelihood).
    fn prior_for(&self, f_lo_hz: f64, f_hi_hz: f64) -> Option<FamilyPriorSet>;
}

/// No reference data: the posterior always equals the likelihood.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoPriors;

impl FamilyPriors for NoPriors {
    fn prior_for(&self, _f_lo_hz: f64, _f_hi_hz: f64) -> Option<FamilyPriorSet> {
        None
    }
}

/// One prior applied to every extent: the seam a configuration or a test uses until T-212's C17
/// source lands.
#[derive(Clone, Debug)]
pub struct StaticPriors(pub FamilyPriorSet);

impl FamilyPriors for StaticPriors {
    fn prior_for(&self, _f_lo_hz: f64, _f_hi_hz: f64) -> Option<FamilyPriorSet> {
        Some(self.0.clone())
    }
}

/// The outcome of [`fuse`].
#[derive(Clone, Debug, PartialEq)]
pub struct Fused {
    /// Posterior over the same labels as the likelihood, summing to 1.
    pub posterior: Vec<LabelP>,
    /// `prior-tiebreak` / `prior-mismatch`, in that order when both apply.
    pub flags: Vec<ClassFlag>,
    /// The tempering exponent the dominance rule needed (1 = the prior was applied in full).
    pub prior_exponent: f64,
}

fn p_of(dist: &[LabelP], label: &str) -> f64 {
    dist.iter()
        .find(|lp| lp.label == label)
        .map_or(0.0, |lp| lp.p)
}

fn top_label(dist: &[LabelP]) -> Option<&str> {
    dist.iter()
        .filter(|lp| lp.label != UNKNOWN)
        .max_by(|a, b| a.p.total_cmp(&b.p).then_with(|| b.label.cmp(&a.label)))
        .map(|lp| lp.label.as_str())
}

/// Applies `prior^exponent` to the known families of `likelihood`, renormalised to `known_mass`.
fn apply(
    likelihood: &[LabelP],
    prior: Option<&FamilyPriorSet>,
    exponent: f64,
    known_mass: f64,
) -> Vec<LabelP> {
    let k = likelihood.iter().filter(|lp| lp.label != UNKNOWN).count();
    let weighted: Vec<(String, f64)> = likelihood
        .iter()
        .filter(|lp| lp.label != UNKNOWN)
        .map(|lp| {
            let w = match prior {
                Some(p) => lp.p * p.p_of(&lp.label, k).powf(exponent),
                None => lp.p,
            };
            (
                lp.label.clone(),
                if w.is_finite() { w.max(0.0) } else { 0.0 },
            )
        })
        .collect();
    let sum: f64 = weighted.iter().map(|(_, w)| w).sum();
    weighted
        .into_iter()
        .map(|(label, w)| LabelP {
            label,
            p: if sum > 0.0 {
                known_mass * w / sum
            } else if k > 0 {
                known_mass / k as f64
            } else {
                0.0
            },
        })
        .collect()
}

/// Fuses an evidence-only distribution with a C17 prior (ADR-0016 §3).
///
/// `likelihood` is over families **plus** `unknown` and sums to 1; `open_set` is the χ² open-set
/// score. The unknown posterior is `max(open_set, likelihood[unknown])`, capped at
/// [`MAX_UNKNOWN_CONFIDENCE`] (T-953), and is never scaled by the prior. The known families share
/// the rest in proportion to their (prior-weighted) likelihoods, and **uniformly when every known
/// likelihood is 0** — which is what the production caller produces at `open_set = 1.0`, so there
/// the residual carries no ranking. With `prior: None` the posterior is the likelihood unchanged
/// apart from that cap.
pub fn fuse(likelihood: &[LabelP], open_set: f64, prior: Option<&FamilyPriorSet>) -> Fused {
    let l_unknown = p_of(likelihood, UNKNOWN);
    // T-953: `unknown` is the residual hypothesis, not a measurement, and `open_set` saturates at
    // 1.0 for anything the shipped densities never saw. Capping it here — before the known mass is
    // shared out — bounds the reported number. It ranks nothing by itself: the residual follows the
    // known likelihoods, which the classifier sets to 0 at `open_set = 1.0` (uniform spread).
    let p_unknown = l_unknown
        .max(open_set.clamp(0.0, 1.0))
        .clamp(0.0, MAX_UNKNOWN_CONFIDENCE);
    let known_mass = 1.0 - p_unknown;

    let mut flags = Vec::new();
    let mut exponent = 1.0;
    let mut known = apply(likelihood, prior, exponent, known_mass);

    if let Some(prior_set) = prior {
        // Likelihood top-1 and runner-up over known families.
        let mut ranked: Vec<&LabelP> = likelihood.iter().filter(|lp| lp.label != UNKNOWN).collect();
        ranked.sort_by(|a, b| b.p.total_cmp(&a.p).then_with(|| a.label.cmp(&b.label)));
        let evidence_top = ranked.first().map(|lp| lp.label.clone());
        let l_top = ranked.first().map_or(0.0, |lp| lp.p);
        let l_second = ranked.get(1).map_or(0.0, |lp| lp.p);
        let dominant = l_second <= 0.0 && l_top > 0.0
            || (l_second > 0.0 && l_top / l_second >= EVIDENCE_DOMINANCE_RATIO);

        let moved = match (&evidence_top, top_label(&known)) {
            (Some(a), Some(b)) => a != b,
            _ => false,
        };
        if moved && dominant {
            // Temper the prior until the dominant evidence is on top again: τ = 0 is a uniform
            // prior, which by construction restores it.
            let (mut lo, mut hi) = (0.0_f64, 1.0_f64);
            for _ in 0..40 {
                let mid = 0.5 * (lo + hi);
                let trial = apply(likelihood, prior, mid, known_mass);
                if top_label(&trial).map(str::to_owned) == evidence_top {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            exponent = lo;
            known = apply(likelihood, prior, exponent, known_mass);
            flags.push(ClassFlag::PriorMismatch);
        } else if moved {
            flags.push(ClassFlag::PriorTiebreak);
        }
        // The prior's own top disagreeing with strong evidence is a mismatch worth flagging even
        // when the posterior did not move (ADR-0016 §3 step 5).
        let prior_top = prior_set
            .dist
            .iter()
            .max_by(|a, b| a.p.total_cmp(&b.p).then_with(|| b.label.cmp(&a.label)))
            .map(|lp| lp.label.as_str());
        let disagrees = match (prior_top, evidence_top.as_deref()) {
            (Some(p), Some(a)) => p != a,
            _ => false,
        };
        if disagrees && l_top >= 0.5 && !flags.contains(&ClassFlag::PriorMismatch) {
            flags.push(ClassFlag::PriorMismatch);
        }
    }

    let mut posterior: Vec<LabelP> = likelihood
        .iter()
        .map(|lp| {
            if lp.label == UNKNOWN {
                LabelP {
                    label: UNKNOWN.to_owned(),
                    p: p_unknown,
                }
            } else {
                LabelP {
                    label: lp.label.clone(),
                    p: p_of(&known, &lp.label),
                }
            }
        })
        .collect();
    cap_and_normalise(&mut posterior);
    Fused {
        posterior,
        flags,
        prior_exponent: exponent,
    }
}

/// Scales a distribution to sum to exactly 1 (no confidence cap: a within-family class
/// distribution of one class is legitimately 1.0, and only the family posterior carries the
/// "never certain" rule).
pub fn normalise_dist(dist: &mut [LabelP]) {
    let sum: f64 = dist.iter().map(|lp| lp.p).sum();
    if sum > 0.0 {
        for lp in dist.iter_mut() {
            lp.p /= sum;
        }
    } else if !dist.is_empty() {
        let share = 1.0 / dist.len() as f64;
        for lp in dist.iter_mut() {
            lp.p = share;
        }
    }
    let residue = 1.0 - dist.iter().map(|lp| lp.p).sum::<f64>();
    if let Some(i) = (0..dist.len()).max_by(|a, b| dist[*a].p.total_cmp(&dist[*b].p)) {
        dist[i].p += residue;
    }
}

/// Caps the top entry at its label's cap — [`MAX_CONFIDENCE`](super::MAX_CONFIDENCE) for a family, and
/// [`MAX_UNKNOWN_CONFIDENCE`] for `unknown` (T-953) — and makes the distribution sum to exactly 1.
pub fn cap_and_normalise(dist: &mut [LabelP]) {
    let sum: f64 = dist.iter().map(|lp| lp.p).sum();
    if sum > 0.0 {
        for lp in dist.iter_mut() {
            lp.p /= sum;
        }
    } else if !dist.is_empty() {
        let share = 1.0 / dist.len() as f64;
        for lp in dist.iter_mut() {
            lp.p = share;
        }
    }
    // Move any excess above the cap to the runner-up (there is always one: every distribution
    // here carries `unknown` plus at least one family).
    let Some(top) = dist
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.p.total_cmp(&b.p))
        .map(|(i, _)| i)
    else {
        return;
    };
    let max = max_confidence_of(&dist[top].label);
    if dist[top].p > max {
        let excess = dist[top].p - max;
        dist[top].p = max;
        let others: Vec<usize> = (0..dist.len()).filter(|i| *i != top).collect();
        if let Some(&second) = others
            .iter()
            .max_by(|a, b| dist[**a].p.total_cmp(&dist[**b].p))
        {
            dist[second].p += excess;
        }
    }
    // Exact sum: fold the rounding residue into the largest entry that can absorb it.
    let sum: f64 = dist.iter().map(|lp| lp.p).sum();
    let residue = 1.0 - sum;
    if residue.abs() > 0.0 {
        if let Some(i) = (0..dist.len())
            .filter(|i| {
                dist[*i].p + residue >= 0.0
                    && dist[*i].p + residue <= max_confidence_of(&dist[*i].label)
            })
            .max_by(|a, b| dist[*a].p.total_cmp(&dist[*b].p))
        {
            dist[i].p += residue;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{HK_MOD_V1, MAX_CONFIDENCE};

    fn lp(label: &str, p: f64) -> LabelP {
        LabelP {
            label: label.into(),
            p,
        }
    }

    fn prior(pairs: &[(&str, f64)]) -> FamilyPriorSet {
        FamilyPriorSet {
            prior_ref: "band-plan/test@1".into(),
            lambda: [0.2, 0.6, 0.1, 0.1],
            dist: pairs.iter().map(|(l, p)| lp(l, *p)).collect(),
        }
    }

    /// The same, at the smallest λ₀ the contract allows: with only two families in play the
    /// uniform floor is λ₀/2, so a prior needs this little uniform mass to move a 10:1 call at
    /// all.
    fn sharp_prior(pairs: &[(&str, f64)]) -> FamilyPriorSet {
        FamilyPriorSet {
            prior_ref: "band-plan/test@1".into(),
            lambda: [0.1, 0.9, 0.0, 0.0],
            dist: pairs.iter().map(|(l, p)| lp(l, *p)).collect(),
        }
    }

    fn p(f: &Fused, label: &str) -> f64 {
        p_of(&f.posterior, label)
    }

    #[test]
    fn without_a_prior_the_posterior_is_the_likelihood() {
        let l = vec![lp("fsk", 0.6), lp("psk-qam", 0.2), lp(UNKNOWN, 0.2)];
        let f = fuse(&l, 0.2, None);
        for x in &l {
            assert!((p(&f, &x.label) - x.p).abs() < 1e-9, "{}", x.label);
        }
        assert!(f.flags.is_empty());
    }

    #[test]
    fn a_prior_never_moves_the_unknown_mass() {
        let l = vec![lp("fsk", 0.45), lp("analog", 0.25), lp(UNKNOWN, 0.3)];
        for pr in [
            prior(&[("analog", 0.98), ("fsk", 0.02)]),
            prior(&[("fsk", 0.9), ("analog", 0.1)]),
        ] {
            let with = fuse(&l, 0.3, Some(&pr));
            let without = fuse(&l, 0.3, None);
            assert!(
                (p(&with, UNKNOWN) - p(&without, UNKNOWN)).abs() < 1e-9,
                "the prior changed p(unknown)"
            );
        }
        // The open-set score raises unknown, whatever the prior says.
        let strong = fuse(&l, 0.8, Some(&prior(&[("analog", 0.99), ("fsk", 0.01)])));
        assert!((p(&strong, UNKNOWN) - 0.8).abs() < 1e-9);
    }

    #[test]
    fn a_prior_cannot_flip_a_ten_to_one_likelihood() {
        // 10:1 evidence for fsk, and a prior strong enough that — after the λ₀ floor keeps fsk
        // above zero — it would otherwise carry analog past it.
        let l = vec![lp("fsk", 0.5), lp("analog", 0.05), lp(UNKNOWN, 0.45)];
        let f = fuse(
            &l,
            0.45,
            Some(&sharp_prior(&[("analog", 0.995), ("fsk", 0.005)])),
        );
        assert_eq!(top_label(&f.posterior), Some("fsk"), "{:?}", f.posterior);
        assert!(f.flags.contains(&ClassFlag::PriorMismatch));
        assert!(f.prior_exponent < 1.0, "the prior must be tempered");
        // Just under the ratio, the prior may decide: that is a tiebreak, not a mismatch.
        let close = vec![lp("fsk", 0.30), lp("analog", 0.20), lp(UNKNOWN, 0.5)];
        let g = fuse(&close, 0.5, Some(&prior(&[("analog", 0.9), ("fsk", 0.1)])));
        assert_eq!(top_label(&g.posterior), Some("analog"));
        assert!(g.flags.contains(&ClassFlag::PriorTiebreak), "{:?}", g.flags);
    }

    #[test]
    fn a_prior_can_never_zero_a_family_and_lambda0_is_enforced() {
        let l = vec![lp("fsk", 0.4), lp("analog", 0.3), lp(UNKNOWN, 0.3)];
        let f = fuse(&l, 0.3, Some(&prior(&[("analog", 1.0)])));
        assert!(p(&f, "fsk") > 0.0, "the omitted family kept mass");
        let mut bad = prior(&[("analog", 1.0)]);
        bad.lambda = [0.0, 1.0, 0.0, 0.0];
        assert!(bad.validate().is_err(), "lambda0 = 0 must be refused");
        bad.lambda = [0.5, 0.6, 0.0, 0.0];
        assert!(bad.validate().is_err(), "lambdas must sum to 1");
        assert!(prior(&[("analog", 0.6), ("fsk", 0.4)]).validate().is_ok());
        let mut carries_unknown = prior(&[("analog", 0.5)]);
        carries_unknown.dist.push(lp(UNKNOWN, 0.5));
        assert!(carries_unknown.validate().is_err());
    }

    #[test]
    fn the_posterior_sums_to_one_and_is_never_certain() {
        let l = vec![lp("fsk", 0.9999), lp("analog", 0.0), lp(UNKNOWN, 0.0001)];
        let f = fuse(&l, 0.0, None);
        let sum: f64 = f.posterior.iter().map(|x| x.p).sum();
        assert!((sum - 1.0).abs() < 1e-12, "sum {sum}");
        assert!(
            f.posterior.iter().all(|x| x.p <= MAX_CONFIDENCE),
            "{:?}",
            f.posterior
        );
    }

    /// The likelihood `hk_classify::Classifier::classify` actually builds: `L[unknown] = open_set`
    /// and the known families sharing `1 − open_set` in proportion to their evidence.
    fn classifier_likelihood(open_set: f64, evidence: &[(&str, f64)]) -> Vec<LabelP> {
        let total: f64 = evidence.iter().map(|(_, e)| e).sum();
        evidence
            .iter()
            .map(|(l, e)| {
                lp(
                    l,
                    if total > 0.0 {
                        (1.0 - open_set) * e / total
                    } else {
                        0.0
                    },
                )
            })
            .chain(std::iter::once(lp(UNKNOWN, open_set)))
            .collect()
    }

    /// T-953: an emission the shipped densities have never seen saturates the χ² tail to
    /// `open_set = 1.0`, and the row used to read `unknown` at 0.999 (measured on live air on
    /// 2026-09-25 for FLEX pager bursts). With the classifier's own likelihood at that point every
    /// known family has `L = 0`, so the cap makes the number honest and spreads the residual
    /// **uniformly** — it cannot and does not claim a ranking.
    #[test]
    fn a_saturated_open_set_reports_unknown_at_the_cap_over_a_flat_residual() {
        let l = classifier_likelihood(1.0, &[("analog", 5.0), ("fsk", 1.0), ("psk", 0.5)]);
        assert!(l.iter().filter(|x| x.label != UNKNOWN).all(|x| x.p == 0.0));
        let f = fuse(&l, 1.0, None);
        let u = p(&f, UNKNOWN);
        assert!(
            (u - MAX_UNKNOWN_CONFIDENCE).abs() < 1e-12,
            "a saturated open set reports unknown at the {MAX_UNKNOWN_CONFIDENCE} cap, not {u}"
        );
        assert!(u < MAX_CONFIDENCE, "unknown must not reach a family's cap");
        let share = (1.0 - MAX_UNKNOWN_CONFIDENCE) / 3.0;
        for fam in ["analog", "fsk", "psk"] {
            assert!(
                (p(&f, fam) - share).abs() < 1e-12,
                "{fam}: the residual is uniform at open_set = 1.0 (no ranking exists): {:?}",
                f.posterior
            );
        }
        let sum: f64 = f.posterior.iter().map(|x| x.p).sum();
        assert!((sum - 1.0).abs() < 1e-12, "sum {sum}");
    }

    /// Below saturation, `0.9 < open_set < 1.0`, the classifier's likelihood does carry known mass,
    /// and the capped posterior keeps its order inside the residual tenth.
    #[test]
    fn a_nearly_saturated_open_set_is_capped_and_keeps_the_families_ranked() {
        for open_set in [0.93, 0.97, 0.995] {
            let l = classifier_likelihood(open_set, &[("analog", 4.0), ("fsk", 1.0), ("psk", 0.0)]);
            let f = fuse(&l, open_set, None);
            let u = p(&f, UNKNOWN);
            assert!(
                (u - MAX_UNKNOWN_CONFIDENCE).abs() < 1e-12,
                "open_set {open_set}: unknown {u}, want the cap"
            );
            let (a, fsk, psk) = (p(&f, "analog"), p(&f, "fsk"), p(&f, "psk"));
            assert!(
                a > fsk && fsk > psk,
                "open_set {open_set}: the likelihood's order must survive: {:?}",
                f.posterior
            );
            assert!(
                (a / fsk - 4.0).abs() < 1e-9,
                "open_set {open_set}: the residual follows the likelihood ratio: {:?}",
                f.posterior
            );
            assert!(
                (a + fsk + psk - (1.0 - MAX_UNKNOWN_CONFIDENCE)).abs() < 1e-9,
                "open_set {open_set}: the families share exactly the residual"
            );
        }
    }

    /// The cap is on `unknown` alone: a family with the same evidence still reports up to
    /// [`MAX_CONFIDENCE`], and `cap_and_normalise` cannot push `unknown` back over its own cap.
    #[test]
    fn the_unknown_cap_is_lower_than_a_family_s_and_capping_respects_the_label() {
        let l = vec![lp("fsk", 1.0), lp(UNKNOWN, 0.0)];
        let f = fuse(&l, 0.0, None);
        assert!(
            (p(&f, "fsk") - MAX_CONFIDENCE).abs() < 1e-12,
            "a family keeps the 0.999 cap: {:?}",
            f.posterior
        );
        let mut dist = vec![lp(UNKNOWN, 0.998), lp("fsk", 0.002)];
        cap_and_normalise(&mut dist);
        assert!(
            p_of(&dist, UNKNOWN) <= MAX_UNKNOWN_CONFIDENCE + 1e-12,
            "{dist:?}"
        );
        let sum: f64 = dist.iter().map(|x| x.p).sum();
        assert!((sum - 1.0).abs() < 1e-12, "sum {sum}");
    }

    #[test]
    fn a_disagreeing_prior_is_flagged_even_when_the_top_does_not_move() {
        let l = vec![lp("fsk", 0.6), lp("analog", 0.2), lp(UNKNOWN, 0.2)];
        let f = fuse(&l, 0.2, Some(&prior(&[("analog", 0.55), ("fsk", 0.45)])));
        assert_eq!(top_label(&f.posterior), Some("fsk"));
        assert!(f.flags.contains(&ClassFlag::PriorMismatch), "{:?}", f.flags);
    }

    // ---- Property tests (T-218) -----------------------------------------------------------
    //
    // The examples above pin the cases the ADR names. These run the same three rules over ~30 000
    // randomly drawn (likelihood, open-set, prior) triples, because a prior that can flip a call
    // is a *silent* failure — a plausible wrong family, not a crash — and the cases that break it
    // are the extreme ones nobody writes by hand (near-ties, one family carrying everything, a
    // prior that omits the family the evidence likes). Random draws are deterministic (a fixed
    // LCG, no dependency) so a failure is reproducible from the printed seed.

    /// SplitMix64: a deterministic, dependency-free source of well-distributed draws.
    struct Rng(u64);

    impl Rng {
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        /// A float in [0, 1).
        fn unit(&mut self) -> f64 {
            (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
        }

        fn range(&mut self, lo: usize, hi: usize) -> usize {
            lo + (self.next_u64() % (hi - lo + 1) as u64) as usize
        }
    }

    /// A random distribution over `k` of the taxonomy's families plus `unknown`, summing to 1.
    /// Values are drawn on a wide dynamic range (`x^4`) so near-zero families, near-ties and
    /// one-family-takes-all all occur.
    fn random_likelihood(rng: &mut Rng) -> Vec<LabelP> {
        let families: Vec<&str> = HK_MOD_V1.families.iter().map(|f| f.name).collect();
        let k = rng.range(2, families.len());
        let mut chosen: Vec<&str> = families.clone();
        // Fisher-Yates prefix shuffle.
        for i in 0..k {
            let j = i + rng.range(0, chosen.len() - 1 - i);
            chosen.swap(i, j);
        }
        let mut dist: Vec<LabelP> = chosen[..k]
            .iter()
            .map(|f| {
                let u = rng.unit();
                lp(f, u * u * u * u + 1e-9)
            })
            .collect();
        dist.push(lp(UNKNOWN, rng.unit() + 1e-9));
        let sum: f64 = dist.iter().map(|x| x.p).sum();
        for x in dist.iter_mut() {
            x.p /= sum;
        }
        dist
    }

    /// A random **valid** prior over a random subset of the drawn families (sometimes omitting
    /// the evidence's favourite, which is the case the λ₀ floor exists for).
    fn random_prior(rng: &mut Rng, likelihood: &[LabelP]) -> FamilyPriorSet {
        let families: Vec<&str> = likelihood
            .iter()
            .filter(|x| x.label != UNKNOWN)
            .map(|x| x.label.as_str())
            .collect();
        let k = rng.range(1, families.len());
        let mut dist: Vec<LabelP> = families[..k]
            .iter()
            .map(|f| {
                let u = rng.unit();
                lp(f, u * u * u + 1e-9)
            })
            .collect();
        let sum: f64 = dist.iter().map(|x| x.p).sum();
        for x in dist.iter_mut() {
            x.p /= sum;
        }
        // λ₀ anywhere in the legal range, the rest split arbitrarily.
        let l0 = LAMBDA0_MIN + rng.unit() * (1.0 - LAMBDA0_MIN);
        let rest = 1.0 - l0;
        let a = rng.unit();
        let b = rng.unit();
        let c = rng.unit();
        let s = (a + b + c).max(1e-12);
        FamilyPriorSet {
            prior_ref: "band-plan/property@1".into(),
            lambda: [l0, rest * a / s, rest * b / s, rest * c / s],
            dist,
        }
    }

    const TRIALS: u32 = 30_000;

    #[test]
    fn property_a_prior_never_scales_the_unknown_mass() {
        let mut rng = Rng(0xC15_0016);
        for trial in 0..TRIALS {
            let l = random_likelihood(&mut rng);
            let open_set = rng.unit();
            let pr = random_prior(&mut rng, &l);
            pr.validate().unwrap();
            let want = p_of(&l, UNKNOWN).max(open_set);
            let with = fuse(&l, open_set, Some(&pr));
            let without = fuse(&l, open_set, None);
            // The cap only ever moves mass when a family would exceed 0.999, so compare within
            // the cap's tolerance.
            assert!(
                (p(&with, UNKNOWN) - p(&without, UNKNOWN)).abs() < 1e-9,
                "trial {trial}: the prior moved p(unknown) {} → {}",
                p(&without, UNKNOWN),
                p(&with, UNKNOWN)
            );
            assert!(
                (p(&with, UNKNOWN) - want.min(MAX_UNKNOWN_CONFIDENCE)).abs() < 1e-9,
                "trial {trial}: p(unknown) {} is not max(open_set, L[unknown]) {want}",
                p(&with, UNKNOWN)
            );
        }
    }

    #[test]
    fn property_a_prior_never_zeroes_a_family_and_the_posterior_is_a_distribution() {
        let mut rng = Rng(0xC15_0017);
        for trial in 0..TRIALS {
            let l = random_likelihood(&mut rng);
            let open_set = rng.unit();
            let pr = random_prior(&mut rng, &l);
            let f = fuse(&l, open_set, Some(&pr));
            let sum: f64 = f.posterior.iter().map(|x| x.p).sum();
            assert!((sum - 1.0).abs() < 1e-9, "trial {trial}: sum {sum}");
            assert_eq!(f.posterior.len(), l.len(), "trial {trial}: labels dropped");
            for x in &f.posterior {
                assert!(
                    x.p.is_finite() && (0.0..=MAX_CONFIDENCE).contains(&x.p),
                    "trial {trial}: {} = {}",
                    x.label,
                    x.p
                );
            }
            // λ₀ ≥ 0.1 floors every prior, so a family the evidence gave mass to keeps some —
            // even one the prior leaves out entirely. (Unless the whole known mass is zero:
            // open_set = 1 legitimately puts everything on `unknown`.)
            if 1.0 - p_of(&f.posterior, UNKNOWN) > 1e-6 {
                for x in l.iter().filter(|x| x.label != UNKNOWN && x.p > 1e-6) {
                    assert!(
                        p_of(&f.posterior, &x.label) > 0.0,
                        "trial {trial}: the prior zeroed {}",
                        x.label
                    );
                }
            }
        }
    }

    #[test]
    fn property_a_prior_cannot_flip_a_ten_to_one_likelihood() {
        let mut rng = Rng(0xC15_0018);
        let mut dominant_cases = 0u32;
        for trial in 0..TRIALS {
            let l = random_likelihood(&mut rng);
            let open_set = rng.unit();
            let pr = random_prior(&mut rng, &l);

            let mut ranked: Vec<&LabelP> = l.iter().filter(|x| x.label != UNKNOWN).collect();
            ranked.sort_by(|a, b| b.p.total_cmp(&a.p).then_with(|| a.label.cmp(&b.label)));
            let (top, second) = (ranked[0], ranked.get(1));
            let dominant = second.is_none_or(|s| s.p <= 0.0 || top.p / s.p >= 10.0);
            if !dominant {
                continue;
            }
            dominant_cases += 1;
            let f = fuse(&l, open_set, Some(&pr));
            if 1.0 - p_of(&f.posterior, UNKNOWN) <= 1e-6 {
                continue; // everything is unknown: there is no family call to protect.
            }
            assert_eq!(
                top_label(&f.posterior),
                Some(top.label.as_str()),
                "trial {trial}: a prior flipped a {:.1}:1 call\nL={l:?}\nprior={:?}\npost={:?}",
                top.p / second.map_or(f64::INFINITY, |s| s.p),
                pr.dist,
                f.posterior
            );
            assert!(
                f.flags.contains(&ClassFlag::PriorMismatch) || f.prior_exponent == 1.0,
                "trial {trial}: a tempered prior must be flagged"
            );
        }
        assert!(
            dominant_cases > 1_000,
            "the draw produced only {dominant_cases} dominant-evidence cases: \
             the property would not be exercised"
        );
    }

    #[test]
    fn property_a_uniform_prior_leaves_the_known_families_as_the_evidence_ranked_them() {
        let mut rng = Rng(0xC15_0019);
        for trial in 0..(TRIALS / 10) {
            let l = random_likelihood(&mut rng);
            let open_set = rng.unit();
            let k = l.iter().filter(|x| x.label != UNKNOWN).count();
            let uniform = FamilyPriorSet {
                prior_ref: "uniform@1".into(),
                lambda: [1.0, 0.0, 0.0, 0.0],
                dist: l
                    .iter()
                    .filter(|x| x.label != UNKNOWN)
                    .map(|x| lp(&x.label, 1.0 / k as f64))
                    .collect(),
            };
            uniform.validate().unwrap();
            let with = fuse(&l, open_set, Some(&uniform));
            let without = fuse(&l, open_set, None);
            for x in &l {
                assert!(
                    (p_of(&with.posterior, &x.label) - p_of(&without.posterior, &x.label)).abs()
                        < 1e-9,
                    "trial {trial}: a uniform prior changed {}",
                    x.label
                );
            }
            assert!(
                !with.flags.contains(&ClassFlag::PriorTiebreak),
                "trial {trial}: a uniform prior decided nothing, so it is no tiebreak"
            );
        }
    }
}
