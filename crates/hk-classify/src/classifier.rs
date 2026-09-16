//! The C15 cascade's first stage: one normalised snippet in, one [`Classification`] out
//! (ADR-0016 §4).
//!
//! ```text
//! features@1 ─▶ tree gates ─▶ class-conditional densities ─▶ χ² open set ─▶ prior fusion
//!                                                                    └▶ within-family class
//! ```
//!
//! What the stage guarantees, and what the tests below check:
//! - **Gates bound the claim.** A family whose SNR gate the measurement does not reach contributes
//!   no likelihood mass at all: its share goes to `unknown` with reason `low_snr`, never to a
//!   neighbouring family. "Not measured" is not "ruled out" (ADR-0016 §8).
//! - **Unknown is a real outcome.** It wins when nothing is plausible (the χ² open-set score),
//!   when no family passes a gate, and when the evidence is too flat to report one
//!   ([`crate::thresholds`] `min_confidence`, the entropy rule).
//! - **Priors rank, never veto** ([`crate::fuse`]).
//! - **Nothing is certain.** The posterior is capped at `MAX_CONFIDENCE`.
//!
//! The verifier (T-200) and the per-family DL stage (T-204) re-rank *within* what this stage
//! produced; neither may add a family it did not score.

use hk_estimate::blind::SymbolParameters;
use hk_model::Timestamp;
use hk_model::classify::{
    ClassCall, ClassFlag, ClassProvenance, Classification, Coarse, HK_MOD_V1, LabelP, Stage,
    SuspectFlags, TaxonomyRef, UNKNOWN, entropy_norm,
};
use hk_model::emitter::LinkTarget;
use num_complex::Complex32;

use crate::density::DensityModel;
use crate::features::{FeatureInput, Features, features};
use crate::fuse::{FamilyPriorSet, fuse};
use crate::openset::open_set_score;
use crate::thresholds::{
    ABSTAIN_ENTROPY, ABSTAIN_TOP_SHARE, FEATURES_VERSION, RULES_VERSION, THRESHOLDS_VERSION,
    passes_gate, thresholds_of,
};
use crate::tree::{admissible, class_guess, coarse_hint};

/// One classification request: a normalised snippet plus what C13/C14 measured about it.
#[derive(Clone, Debug)]
pub struct ClassifyRequest<'a> {
    /// Normalised snippet (CFO-corrected, unit mean power, cut to the extent).
    pub samples: &'a [Complex32],
    /// Sample rate of `samples`, Hz.
    pub sample_rate_hz: f64,
    /// C13 OBW99, Hz.
    pub obw_hz: Option<f64>,
    /// C13 in-band SNR of the analysed extent, dB. Without it every gated family abstains.
    pub snr_db: Option<f64>,
    /// C14 symbol estimate, when it ran.
    pub symbols: Option<&'a SymbolParameters>,
    /// The same emission at C14's geometry ([`crate::symbols::SYMBOL_SAMPLES_PER_OBW`]), for the
    /// post-sync verifier (T-200). `None` — or a missing clock lock — and the verifier does not
    /// run, leaving the feature tree's within-family ranking exactly as it was.
    pub symbol_samples: Option<&'a [Complex32]>,
    /// Sample rate of [`ClassifyRequest::symbol_samples`], Hz.
    pub symbol_sample_rate_hz: Option<f64>,
    /// When the classification was produced.
    pub t: Timestamp,
    /// The observation it ran on (track, detection, demodulation, decode).
    pub input: Option<LinkTarget>,
    /// Detection provenance: clipped, IMD, image, spur.
    pub suspect: SuspectFlags,
    /// Power mode it ran in, when the caller tracks one.
    pub power_mode: Option<String>,
    /// C17 prior for this extent (T-212 supplies it; `None` = no reference data).
    pub prior: Option<FamilyPriorSet>,
}

impl<'a> ClassifyRequest<'a> {
    /// A request over `samples` with nothing else measured.
    pub fn new(samples: &'a [Complex32], sample_rate_hz: f64, t: Timestamp) -> Self {
        Self {
            samples,
            sample_rate_hz,
            obw_hz: None,
            snr_db: None,
            symbols: None,
            symbol_samples: None,
            symbol_sample_rate_hz: None,
            t,
            input: None,
            suspect: SuspectFlags::default(),
            power_mode: None,
            prior: None,
        }
    }
}

/// The feature-tree classifier.
#[derive(Clone, Debug)]
pub struct Classifier {
    model: DensityModel,
    /// Densities fitted below each family's gate. Used only to size the `unknown` mass owed to a
    /// family the SNR gate held back, never to claim one.
    below_gate: DensityModel,
}

impl Default for Classifier {
    fn default() -> Self {
        Self::new()
    }
}

impl Classifier {
    /// A classifier over the shipped densities.
    pub fn new() -> Self {
        Self {
            model: DensityModel::builtin().clone(),
            below_gate: DensityModel::builtin_below_gate().clone(),
        }
    }

    /// A classifier over a model the caller fitted (the dev-grid fitter, T-204's experiments).
    /// The shipped below-gate densities are kept: they only ever move mass to `unknown`.
    pub fn with_model(model: DensityModel) -> Self {
        Self {
            model,
            below_gate: DensityModel::builtin_below_gate().clone(),
        }
    }

    /// The densities it scores with.
    pub fn model(&self) -> &DensityModel {
        &self.model
    }

    /// Classifies one normalised snippet.
    pub fn classify(&self, request: &ClassifyRequest<'_>) -> Classification {
        let f = features(&FeatureInput {
            samples: request.samples,
            sample_rate_hz: request.sample_rate_hz,
            obw_hz: request.obw_hz,
            snr_db: request.snr_db,
            symbols: request.symbols,
        });
        self.classify_features(&f, request)
    }

    /// Classifies an already-computed feature vector (the fitter and the evaluation harness reuse
    /// the features they measured).
    pub fn classify_features(&self, f: &Features, request: &ClassifyRequest<'_>) -> Classification {
        let tax = &HK_MOD_V1;
        let mut reasons: Vec<String> = f.reasons.clone();
        let mut flags: Vec<ClassFlag> = Vec::new();

        // 1. Gates and tree exclusions decide which families may be scored at all.
        let rules = admissible(f);
        let mut gated = false;
        let mut scored: Vec<(String, crate::density::FamilyScore)> = Vec::new();
        // How plausibly a family held back by its SNR gate could explain this snippet, read from
        // the below-gate densities. It becomes `unknown` mass below, never a claim.
        let mut gated_plausibility = 0.0_f64;
        for rule in &rules {
            if !rule.allowed {
                if let Some(r) = rule.reason {
                    push_reason(&mut reasons, r);
                }
                continue;
            }
            if passes_gate(rule.family, request.snr_db) {
                match self.model.score(rule.family, f) {
                    Some(s) => scored.push((rule.family.to_owned(), s)),
                    None => push_reason(&mut reasons, "too_few_features"),
                }
                continue;
            }
            gated = true;
            push_reason(
                &mut reasons,
                if request.snr_db.is_some() {
                    "low_snr"
                } else {
                    "no_snr"
                },
            );
            if let Some(s) = self.below_gate.score(rule.family, f) {
                gated_plausibility = gated_plausibility.max(s.plausibility);
            }
        }
        if gated {
            flags.push(ClassFlag::BelowGate);
        }
        if request.suspect.any() {
            flags.push(ClassFlag::SuspectInput);
        }

        // 2. Open set: how far the snippet is from every family that was allowed to explain it
        // (the calibrated plausibility). The evidence-only distribution ranks what is left.
        let open_set = open_set_score(scored.iter().map(|(_, s)| s.plausibility));

        // A family held back by its SNR gate has not been ruled out — it was **not measured**, and
        // ADR-0016 §2 puts its share on `unknown`, not on its neighbours. Dropping it and
        // renormalising over the survivors is what makes a low-SNR FSK burst come back as
        // `analog`: FSK is gated out at 20 dB while analog, gated at 10, is still standing and
        // collects the whole distribution.
        //
        // So the `unknown` mass is at least the plausibility of the best family the gate held back:
        // if a gated family could genuinely have produced this snippet, no survivor may be claimed,
        // because the one measurement that would have separated them is the one we were not allowed
        // to make. `plausibility` is the right currency for that comparison — a dev-calibrated χ²
        // tail, so it is directly comparable with the open-set score, unlike `evidence`, which is
        // `exp(-K_REF*m/2)` and underflows to exactly 0 for any poor fit.
        //
        // The plausibility comes from the **below-gate** densities, which are fitted where the
        // family is gated (`fit-densities` writes both files). The shipped at-gate densities cannot
        // answer this: scoring a family below its own gate is an extrapolation, and a genuine 2-FSK
        // burst 10 dB under the FSK gate scored its own family at plausibility 0.000 — so no mass
        // moved to `unknown` and the constant-envelope `analog` classes, which that waveform really
        // does resemble at that SNR, claimed it instead (a 0.65 wrong-label rate in that bin).
        //
        // An AM carrier at the same SNR is not plausible FSK, contributes nothing, and is still
        // reported as analog. This is only ever a `max`, so a gated family can move mass to
        // `unknown` but can never make a claim.
        let open_set = open_set.max(gated_plausibility);
        let known_mass = 1.0 - open_set;
        let total: f64 = scored.iter().map(|(_, s)| s.evidence).sum();
        let likelihood: Vec<LabelP> = tax
            .families
            .iter()
            .map(|fam| LabelP {
                label: fam.name.to_owned(),
                p: match scored.iter().find(|(l, _)| l == fam.name) {
                    Some((_, s)) if total > 0.0 => known_mass * s.evidence / total,
                    _ => 0.0,
                },
            })
            .chain(std::iter::once(LabelP {
                label: UNKNOWN.to_owned(),
                p: open_set,
            }))
            .collect();

        // 3. Fusion with the C17 prior (a prior can reorder, never veto or reach `unknown`).
        let fused = fuse(&likelihood, open_set, request.prior.as_ref());
        let mut posterior = fused.posterior;
        flags.extend(fused.flags);

        // 4. Report or abstain.
        let top = top_known(&posterior);
        let l_top = top_known(&likelihood).map_or(0.0, |(_, p)| p);
        let l_share = if known_mass > 0.0 {
            l_top / known_mass
        } else {
            0.0
        };
        let entropy_of_likelihood = entropy_norm(&likelihood, tax.families.len() + 1);
        let abstain = match &top {
            None => Some("no_family_scored"),
            Some((label, p)) => {
                let t = thresholds_of(label);
                if scored.is_empty() {
                    Some("no_family_scored")
                } else if *p < t.map_or(0.6, |t| t.min_confidence) {
                    Some("low_confidence")
                } else if open_set > t.map_or(0.5, |t| t.open_set_max) {
                    Some("open_set")
                } else if l_share < ABSTAIN_TOP_SHARE && entropy_of_likelihood > ABSTAIN_ENTROPY {
                    Some("ambiguous")
                } else {
                    None
                }
            }
        };
        if let Some(reason) = abstain {
            push_reason(&mut reasons, reason);
            force_unknown(&mut posterior);
        }

        let (family, confidence) = posterior
            .iter()
            .max_by(|a, b| a.p.total_cmp(&b.p).then_with(|| b.label.cmp(&a.label)))
            .map(|lp| (lp.label.clone(), lp.p))
            .unwrap_or_else(|| (UNKNOWN.to_owned(), 1.0));

        // 5. Within-family class, only above the family's class gate.
        let snr_gate_db = thresholds_of(&family)
            .and_then(|t| t.snr_gate_db)
            .unwrap_or(0.0);
        let class_gate = thresholds_of(&family).map_or(0.0, |t| t.class_gate_db);
        let class = if family == UNKNOWN
            || !request
                .snr_db
                .is_some_and(|s| s >= snr_gate_db + class_gate)
        {
            if family != UNKNOWN {
                push_reason(&mut reasons, "below_class_gate");
            }
            None
        } else {
            class_guess(
                &family,
                f,
                request.obw_hz,
                request.symbols.and_then(|s| s.mod_index_h.value()),
            )
            .map(|g| ClassCall {
                label: g.label,
                p: g.p,
                dist: g.dist,
                stage: Stage::FeatureTree,
            })
        };

        let coarse = if family == UNKNOWN {
            coarse_hint(f)
        } else {
            tax.coarse_of(&family).unwrap_or(Coarse::Unknown)
        };

        let mut out = Classification {
            schema: hk_model::classify::CLASSIFICATION_SCHEMA,
            t: request.t,
            taxonomy: TaxonomyRef::current(),
            input: request.input,
            coarse,
            entropy_norm: entropy_norm(&posterior, tax.families.len() + 1),
            likelihood,
            posterior,
            prior: request.prior.as_ref().map(FamilyPriorSet::to_use),
            family,
            confidence,
            class,
            open_set_score: open_set,
            stage: Stage::FeatureTree,
            provenance: ClassProvenance {
                rules: RULES_VERSION.to_owned(),
                features_version: FEATURES_VERSION,
                features_ref: None,
                ml: None,
                snr_db: request.snr_db,
                snr_gate_db,
                gated,
                thresholds: THRESHOLDS_VERSION.to_owned(),
                suspect: request.suspect,
                power_mode: request.power_mode.clone(),
            },
            flags,
            reasons,
        };

        // 6. The post-sync verifier (T-200), the cascade's next stage. It re-ranks the within-family
        // candidates above and may do nothing else: it never changes the family, never adds a class
        // the tree did not offer, and never turns an abstention into a claim (see [`crate::verify`]).
        // Without the symbol-geometry view, or without a C14 clock lock, it does not run at all.
        if let (Some(samples), Some(rate)) = (request.symbol_samples, request.symbol_sample_rate_hz)
        {
            crate::verify::verify(
                &mut out,
                &crate::verify::VerifyInput {
                    samples,
                    sample_rate_hz: rate,
                    symbols: request.symbols,
                    snr_db: request.snr_db,
                },
            );
        }
        out
    }
}

fn push_reason(reasons: &mut Vec<String>, reason: &str) {
    if !reasons.iter().any(|r| r == reason) {
        reasons.push(reason.to_owned());
    }
}

fn top_known(dist: &[LabelP]) -> Option<(String, f64)> {
    dist.iter()
        .filter(|lp| lp.label != UNKNOWN)
        .max_by(|a, b| a.p.total_cmp(&b.p).then_with(|| b.label.cmp(&a.label)))
        .filter(|lp| lp.p > 0.0)
        .map(|lp| (lp.label.clone(), lp.p))
}

/// Raises `unknown` above every family so the row *reports* an abstention, keeping the relative
/// order of the families below it (the ranking still seeds MAUTO, ADR-0016 §8).
fn force_unknown(posterior: &mut [LabelP]) {
    let top = posterior
        .iter()
        .filter(|lp| lp.label != UNKNOWN)
        .map(|lp| lp.p)
        .fold(0.0_f64, f64::max);
    if let Some(u) = posterior.iter_mut().find(|lp| lp.label == UNKNOWN) {
        u.p = u.p.max(top + 0.05);
    }
    crate::fuse::cap_and_normalise(posterior);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::{Class, SynthConfig, generate};

    fn classify(class: Class, snr_db: f64, seed: u64) -> Classification {
        let s = generate(class, &SynthConfig::new(snr_db, seed));
        let mut req = ClassifyRequest::new(
            &s.samples,
            s.sample_rate_hz,
            Timestamp::from_unix_nanos(1_789_000_000_000_000_000),
        );
        req.obw_hz = Some(s.obw_hz);
        req.snr_db = Some(snr_db);
        Classifier::new().classify(&req)
    }

    #[test]
    fn every_classification_satisfies_the_contract() {
        for class in Class::TAXONOMY.iter().chain(Class::HELD_OUT) {
            for snr in [0.0, 12.0, 25.0] {
                let c = classify(*class, snr, 1_000_123);
                c.validate()
                    .unwrap_or_else(|e| panic!("{} at {snr} dB: {e}", class.label()));
                assert!(c.confidence <= hk_model::classify::MAX_CONFIDENCE);
                assert_eq!(c.stage, Stage::FeatureTree);
                assert_eq!(c.provenance.rules, RULES_VERSION);
                assert_eq!(c.likelihood.len(), c.posterior.len());
            }
        }
    }

    #[test]
    fn below_the_gate_the_mass_goes_to_unknown_not_to_a_neighbour() {
        // 5 dB is below every gate: nothing may be claimed.
        for class in [Class::Fsk2, Class::Bpsk, Class::Ook] {
            let c = classify(class, 5.0, 1_000_321);
            assert_eq!(
                c.family,
                UNKNOWN,
                "{} at 5 dB: {:?}",
                class.label(),
                c.top(3)
            );
            assert!(c.flags.contains(&ClassFlag::BelowGate));
            assert!(c.reasons.iter().any(|r| r == "low_snr"));
            assert!(c.provenance.gated);
        }
        // Without a measured SNR, a gated family cannot be claimed either.
        let s = generate(Class::Fsk2, &SynthConfig::new(30.0, 1_000_322));
        let req = ClassifyRequest::new(&s.samples, s.sample_rate_hz, Timestamp::UNIX_EPOCH);
        let c = Classifier::new().classify(&req);
        assert_eq!(c.family, UNKNOWN);
        assert!(c.reasons.iter().any(|r| r == "no_snr"));
    }

    #[test]
    fn an_out_of_taxonomy_generator_is_unknown_not_the_nearest_family() {
        let mut unknowns = 0;
        let mut n = 0;
        for class in Class::HELD_OUT {
            for seed in 0..6 {
                let c = classify(*class, 25.0, crate::synth::ACCEPTANCE_SEED_BASE + seed);
                n += 1;
                if c.family == UNKNOWN || c.open_set_score >= 0.5 {
                    unknowns += 1;
                }
            }
        }
        // ADR-0016 §7 floor for held-out unknowns.
        let recall = f64::from(unknowns) / f64::from(n);
        assert!(recall >= 0.80, "held-out unknown recall {recall:.2}");
    }

    #[test]
    fn a_short_snippet_abstains_with_a_reason_rather_than_guessing() {
        let c = Classifier::new().classify(&ClassifyRequest::new(
            &[Complex32::new(0.5, 0.5); 32],
            1e6,
            Timestamp::UNIX_EPOCH,
        ));
        assert_eq!(c.family, UNKNOWN);
        assert!(
            c.reasons.iter().any(|r| r == "too_short"),
            "{:?}",
            c.reasons
        );
        c.validate().unwrap();
    }

    #[test]
    fn a_suspect_input_is_flagged_but_still_classified() {
        let s = generate(Class::Fsk2, &SynthConfig::new(25.0, 1_000_555));
        let mut req = ClassifyRequest::new(&s.samples, s.sample_rate_hz, Timestamp::UNIX_EPOCH);
        req.obw_hz = Some(s.obw_hz);
        req.snr_db = Some(25.0);
        req.suspect.clipped = true;
        let c = Classifier::new().classify(&req);
        assert!(c.flags.contains(&ClassFlag::SuspectInput));
        assert!(c.provenance.suspect.clipped);
        c.validate().unwrap();
    }
}
