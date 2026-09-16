//! The MAUTO seed interface (T-215): what the decoder-synthesis search (ADR-0015) is handed by the
//! M3 classifier (ADR-0016 §8), and the rules the M3 side guarantees about it.
//!
//! **This is an interface, not an engine.** It orders hypotheses, flags the ones that may be
//! deferred, and reserves a share of the budget for open search. It runs no search, scores no
//! pipeline and executes nothing: that is MAUTO's work (ADR-0015 §3), and MAUTO is unscheduled.
//!
//! # The three rules
//!
//! 1. **The posterior orders.** [`BudgetHint::families_ordered`] ranks by [`Hypothesis::posterior`]
//!    — the prior-fused distribution — so the best-supported family is tried first.
//! 2. **Only the likelihood prunes.** [`Hypothesis::prune`] is a function of
//!    [`Hypothesis::likelihood`] and the family's SNR gate *alone*. A prior may reorder the search;
//!    it can never defer a hypothesis the evidence keeps alive. A family below its gate was **not
//!    measured**, which is not "ruled out", so it is never pruned.
//! 3. **Open search is always funded.** [`BudgetHint::open_search_min_share`] is at least
//!    `max(p_unknown, `[`OPEN_SEARCH_MIN_SHARE`]`)` — always, including when the classifier is
//!    confident. Priors never starve unknowns.
//!
//! # Suggestions never dictate
//!
//! A C18 signature match or a cluster of unknowns may **raise** a family's place in the order
//! ([`SeedBoost`]) and hand the search a recipe to try first. Neither may remove a hypothesis,
//! change a posterior or a likelihood, or set an identity. That is the rule the whole project runs
//! on: the database suggests, the measurement decides.
//!
//! # Pruning means deferring
//!
//! [`Hypothesis::prune`] marks a branch the search may leave until last (ADR-0015 §4.2, "defer,
//! don't delete"). A pruned hypothesis stays in [`BudgetHint::families_ordered`] with its numbers
//! visible, and evidence found under it ranks like evidence found under any other.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::taxonomy::UNKNOWN;
use super::thresholds::passes_gate;
use super::{Classification, LabelP};
use crate::ids::EmitterId;
use crate::signature::cluster::ClusterState;
use crate::signature::{
    EmissionFeatures, MatchOutcome, RecipeRef, Signature, SignatureMatch, SignatureRef,
};
use crate::time::Timestamp;

/// Schema version of [`SearchSeed`].
pub const SEED_SCHEMA: u16 = 1;

/// Floor on the share of a per-signal budget reserved for open search, whatever the classifier
/// says (ADR-0016 §8, ADR-0015 §4.2).
pub const OPEN_SEARCH_MIN_SHARE: f64 = 0.2;

/// Likelihood share below which a family branch may be deferred — **the only number pruning looks
/// at**, alongside the family's SNR gate (ADR-0016 §8).
pub const PRUNE_LIKELIHOOD_SHARE: f64 = 0.02;

/// Why a family was raised in the search order.
///
/// A boost changes **order only**. It never removes a hypothesis, never touches a posterior or a
/// likelihood, and never sets an identity, a family or a `known_status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SeedBoost {
    /// A cluster of unknowns this emitter belongs to has a best pipeline naming this family.
    Cluster,
    /// A `partial` signature candidate declares this family.
    SignaturePartial,
    /// A `full` signature candidate declares this family (the templates-first fast path).
    SignatureFull,
}

impl SeedBoost {
    /// Ordering tier: higher is tried earlier. 0 means "no boost".
    pub const fn tier(self) -> u8 {
        match self {
            SeedBoost::Cluster => 1,
            SeedBoost::SignaturePartial => 2,
            SeedBoost::SignatureFull => 3,
        }
    }

    /// The serde/reason string.
    pub const fn as_str(self) -> &'static str {
        match self {
            SeedBoost::Cluster => "cluster",
            SeedBoost::SignaturePartial => "signature-partial",
            SeedBoost::SignatureFull => "signature-full",
        }
    }
}

/// One decode hypothesis handed to the search: a family, what the evidence and the posterior say
/// about it, and whether it may be left until last.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hypothesis {
    /// The `hk-mod@1` family this branch would decode as.
    pub family: String,
    /// Prior-fused probability. **This orders the search** (rule 1).
    pub posterior: f64,
    /// Evidence-only probability. **This, and only this, prunes** (rule 2).
    pub likelihood: f64,
    /// Whether the search may defer this branch to the end. Computed from [`Self::likelihood`] and
    /// [`Self::below_gate`] alone — never from [`Self::posterior`].
    pub prune: bool,
    /// The family sits below its SNR gate, so its likelihood is *not measured* rather than low.
    /// Such a branch is never pruned.
    pub below_gate: bool,
    /// Why this family was raised in the order, if it was. Order only.
    #[serde(default)]
    pub boosts: Vec<SeedBoost>,
    /// Recipes a boosting signature or cluster offers for this family, best first: the
    /// templates-first fast path. A recipe is something to *try*, never a conclusion.
    #[serde(default)]
    pub recipes: Vec<RecipeRef>,
    /// Fields a boosting signature needs that the measurement does not have yet — what the search
    /// must go and estimate (ADR-0016 §8).
    #[serde(default)]
    pub missing: Vec<String>,
    /// Machine reason codes (`low_likelihood`, `below_gate_not_measured`).
    #[serde(default)]
    pub reasons: Vec<String>,
}

impl Hypothesis {
    /// The highest boost tier on this hypothesis, or 0 when nothing raised it.
    pub fn boost_tier(&self) -> u8 {
        self.boosts.iter().map(|b| b.tier()).max().unwrap_or(0)
    }
}

/// The per-signal budget advice: how much must stay open, and in what order the known families are
/// worth trying.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetHint {
    /// Share of the per-signal budget reserved for open search — pipelines that assume no family.
    /// At least `max(p_unknown, `[`OPEN_SEARCH_MIN_SHARE`]`)` (rule 3).
    pub open_search_min_share: f64,
    /// The unknown mass this share was derived from: the larger of the posterior's and the
    /// likelihood's `unknown`, so prior fusion can never lower the floor.
    pub p_unknown: f64,
    /// Every known family, ordered by [`Hypothesis::posterior`] within its boost tier (rule 1).
    /// `unknown` is deliberately absent: it is not a branch to try, it is the reserved share.
    pub families_ordered: Vec<Hypothesis>,
}

impl BudgetHint {
    /// The hypotheses worth trying first, in order.
    pub fn active(&self) -> impl Iterator<Item = &Hypothesis> {
        self.families_ordered.iter().filter(|h| !h.prune)
    }

    /// The deferred hypotheses, in order. Deferred is not deleted: they run if budget remains.
    pub fn deferred(&self) -> impl Iterator<Item = &Hypothesis> {
        self.families_ordered.iter().filter(|h| h.prune)
    }
}

/// The best pipeline a cluster has to offer its members (ADR-0016 §8, "cluster reuse").
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterPipeline {
    /// The recipe to warm-start from.
    pub recipe: RecipeRef,
    /// The signature it came from, when the cluster was promoted to one.
    pub signature: Option<SignatureRef>,
    /// The family that signature declares, if any. This is what may raise a hypothesis's order —
    /// and all it may do.
    pub family: Option<String>,
    /// Evidence score of that pipeline, once a search has reported one. `None` until then; it is
    /// never invented here.
    pub evidence_score: Option<f64>,
}

/// The cluster of unknowns this emitter belongs to, as the search sees it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterSeed {
    /// Cluster id.
    pub cluster_id: String,
    /// Its lifecycle state — how much has been seen, never what it is.
    pub state: ClusterState,
    /// The emitters in it, so a result can be offered to every member.
    pub members: Vec<EmitterId>,
    /// Its best pipeline so far, if it has one.
    pub best_pipeline: Option<ClusterPipeline>,
}

/// Everything the M3 side knows about one emitter, assembled for the search.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchSeed {
    /// Schema version, [`SEED_SCHEMA`].
    pub schema: u16,
    /// The emitter this seeds a search for.
    pub emitter: EmitterId,
    /// When the seed was assembled.
    pub t: Timestamp,
    /// The current classification by arbitration rank, unchanged. The seed never rewrites it.
    pub classification: Classification,
    /// Measured parameters with their uncertainty — the ranges a search may vary within.
    pub features: Option<EmissionFeatures>,
    /// What the catalogue had to say, with its ranked candidates, missing fields and conflicts.
    pub signature_match: Option<SignatureMatch>,
    /// The cluster of unknowns this emitter belongs to, if any.
    pub cluster: Option<ClusterSeed>,
    /// Ordering, pruning and the reserved open share.
    pub budget_hint: BudgetHint,
    /// Machine reason codes about the assembly itself (e.g.
    /// `signature_family_outside_taxonomy`).
    #[serde(default)]
    pub reasons: Vec<String>,
}

/// What [`SearchSeed::assemble`] needs. Everything is read-only: assembling a seed writes nothing
/// and decides nothing.
#[derive(Clone, Copy, Debug)]
pub struct SeedInputs<'a> {
    /// The emitter.
    pub emitter: EmitterId,
    /// Assembly time.
    pub t: Timestamp,
    /// Its current classification.
    pub classification: &'a Classification,
    /// Its latest features snapshot, if any.
    pub features: Option<&'a EmissionFeatures>,
    /// Its current signature match, if any.
    pub signature_match: Option<&'a SignatureMatch>,
    /// The catalogue entries behind that match's candidates. A candidate whose entry is not here,
    /// or whose entry declares no family, simply raises nothing.
    pub catalogue: &'a [Signature],
    /// Its cluster, if any.
    pub cluster: Option<&'a ClusterSeed>,
}

/// What a signature or cluster contributed to one family. Order and things to try — never
/// identity.
#[derive(Default)]
struct Raise {
    boosts: BTreeSet<SeedBoost>,
    recipes: Vec<RecipeRef>,
    missing: BTreeSet<String>,
}

fn p_of(dist: &[LabelP], label: &str) -> f64 {
    dist.iter()
        .find(|lp| lp.label == label)
        .map_or(0.0, |lp| lp.p)
}

impl SearchSeed {
    /// Assembles the seed, applying the three rules.
    ///
    /// It is a pure function of what was measured and what the catalogue says. It writes nothing,
    /// starts nothing and never modifies the classification it is given.
    pub fn assemble(inputs: SeedInputs<'_>) -> Self {
        let c = inputs.classification;
        let mut reasons = Vec::new();

        // Which families exist at all: the taxonomy's, plus anything either distribution mentions.
        // `unknown` is not a branch — it is the reserved open share (rule 3).
        let mut labels: BTreeSet<String> = BTreeSet::new();
        if let Ok(tax) = c.resolved_taxonomy() {
            labels.extend(tax.families.iter().map(|f| f.name.to_owned()));
        }
        labels.extend(c.posterior.iter().map(|lp| lp.label.clone()));
        labels.extend(c.likelihood.iter().map(|lp| lp.label.clone()));
        labels.remove(UNKNOWN);

        // What the catalogue and the cluster would like tried earlier. Order only.
        let mut raises: BTreeMap<String, Raise> = BTreeMap::new();
        let mut raise_outside = false;

        if let Some(m) = inputs.signature_match {
            let boost = match m.outcome {
                MatchOutcome::Full => Some(SeedBoost::SignatureFull),
                MatchOutcome::Partial => Some(SeedBoost::SignaturePartial),
                MatchOutcome::None => None,
            };
            if let Some(boost) = boost {
                for cand in &m.candidates {
                    let entry = inputs
                        .catalogue
                        .iter()
                        .find(|s| s.id == cand.signature.id && s.version == cand.signature.version);
                    let Some(family) = entry.and_then(|s| s.family.as_deref()) else {
                        continue;
                    };
                    if family == UNKNOWN {
                        continue;
                    }
                    if !labels.contains(family) {
                        // A signature naming a family this taxonomy does not have raises nothing:
                        // the catalogue never invents a hypothesis.
                        raise_outside = true;
                        continue;
                    }
                    let r = raises.entry(family.to_owned()).or_default();
                    r.boosts.insert(boost);
                    if let Some(recipe) = &cand.recipe
                        && !r.recipes.contains(recipe)
                    {
                        r.recipes.push(recipe.clone());
                    }
                    r.missing.extend(cand.missing.iter().cloned());
                }
            }
        }

        if let Some(pipeline) = inputs.cluster.and_then(|cl| cl.best_pipeline.as_ref())
            && let Some(family) = pipeline.family.as_deref()
            && family != UNKNOWN
        {
            if labels.contains(family) {
                let r = raises.entry(family.to_owned()).or_default();
                r.boosts.insert(SeedBoost::Cluster);
                if !r.recipes.contains(&pipeline.recipe) {
                    r.recipes.push(pipeline.recipe.clone());
                }
            } else {
                raise_outside = true;
            }
        }

        if raise_outside {
            reasons.push("signature_family_outside_taxonomy".to_owned());
        }

        // Rule 3: the open share, before anything else can eat into it. The larger of the two
        // unknown masses, so fusing a prior can never lower the floor.
        let p_unknown = p_of(&c.posterior, UNKNOWN)
            .max(p_of(&c.likelihood, UNKNOWN))
            .clamp(0.0, 1.0);
        let open_search_min_share = p_unknown.max(OPEN_SEARCH_MIN_SHARE).clamp(0.0, 1.0);

        let snr_db = c.provenance.snr_db;
        let mut families_ordered: Vec<Hypothesis> = labels
            .into_iter()
            .map(|family| {
                let posterior = p_of(&c.posterior, &family);
                let likelihood = p_of(&c.likelihood, &family);
                let below_gate = !passes_gate(&family, snr_db);

                // Rule 2: the likelihood and the gate decide this, and nothing else. `posterior`
                // is deliberately not in scope of this expression.
                let prune = likelihood < PRUNE_LIKELIHOOD_SHARE && !below_gate;

                let mut reasons = Vec::new();
                if below_gate {
                    reasons.push("below_gate_not_measured".to_owned());
                }
                if prune {
                    reasons.push("low_likelihood".to_owned());
                }

                let raise = raises.get(&family);
                Hypothesis {
                    family,
                    posterior,
                    likelihood,
                    prune,
                    below_gate,
                    boosts: raise
                        .map(|r| r.boosts.iter().copied().collect())
                        .unwrap_or_default(),
                    recipes: raise.map(|r| r.recipes.clone()).unwrap_or_default(),
                    missing: raise
                        .map(|r| r.missing.iter().cloned().collect())
                        .unwrap_or_default(),
                    reasons,
                }
            })
            .collect();

        // Rule 1: the posterior orders. A boost may lift a family over the tier below it; inside a
        // tier the posterior alone decides, with the label as a deterministic tie-break.
        families_ordered.sort_by(|a, b| {
            b.boost_tier()
                .cmp(&a.boost_tier())
                .then_with(|| b.posterior.total_cmp(&a.posterior))
                .then_with(|| a.family.cmp(&b.family))
        });

        SearchSeed {
            schema: SEED_SCHEMA,
            emitter: inputs.emitter,
            t: inputs.t,
            classification: c.clone(),
            features: inputs.features.cloned(),
            signature_match: inputs.signature_match.cloned(),
            cluster: inputs.cluster.cloned(),
            budget_hint: BudgetHint {
                open_search_min_share,
                p_unknown,
                families_ordered,
            },
            reasons,
        }
    }

    /// Re-checks the three rules on an assembled or deserialised seed, so a seed crossing the API
    /// boundary is checked rather than trusted.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != SEED_SCHEMA {
            return Err(format!("schema {} is not {SEED_SCHEMA}", self.schema));
        }
        let b = &self.budget_hint;

        // Rule 3.
        let floor = b.p_unknown.max(OPEN_SEARCH_MIN_SHARE);
        if b.open_search_min_share + 1e-12 < floor {
            return Err(format!(
                "open share {} is below the reserved floor {floor}",
                b.open_search_min_share
            ));
        }

        for h in &b.families_ordered {
            if h.family == UNKNOWN {
                return Err("`unknown` is the reserved open share, not a family branch".into());
            }
            // Rule 2.
            let expected = h.likelihood < PRUNE_LIKELIHOOD_SHARE && !h.below_gate;
            if h.prune != expected {
                return Err(format!(
                    "{}: prune {} does not follow from likelihood {} and below_gate {}",
                    h.family, h.prune, h.likelihood, h.below_gate
                ));
            }
        }

        // Rule 1.
        for w in b.families_ordered.windows(2) {
            let (prev, next) = (&w[0], &w[1]);
            if prev.boost_tier() < next.boost_tier() {
                return Err(format!(
                    "{}: a lower boost tier is ordered first",
                    next.family
                ));
            }
            if prev.boost_tier() == next.boost_tier() && prev.posterior < next.posterior {
                return Err(format!(
                    "{}: posterior {} is ordered after the smaller {}",
                    next.family, next.posterior, prev.posterior
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::fuse::cap_and_normalise;
    use crate::classify::taxonomy::{Coarse, HK_MOD_V1, TaxonomyRef};
    use crate::classify::{
        CLASSIFICATION_SCHEMA, ClassProvenance, FamilyPriorSet, MAX_CONFIDENCE, PriorUse, Stage,
        entropy_norm, fuse,
    };
    use crate::signature::{
        SIGNATURE_SCHEMA, SignatureCandidate, SignatureKind, SignatureProvenance,
    };

    const TRIALS: usize = 300;

    /// A deterministic generator: property tests here are exhaustive over a seeded stream, not
    /// random from run to run.
    struct Rng(u64);

    impl Rng {
        fn next_u64(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 11
        }

        /// A weight in (0, 1].
        fn weight(&mut self) -> f64 {
            0.001 + 0.999 * ((self.next_u64() % 1_000_001) as f64 / 1_000_000.0)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next_u64() % n as u64) as usize
        }
    }

    fn t0() -> Timestamp {
        Timestamp::from_unix_nanos(1_789_000_000_000_000_000)
    }

    fn labels() -> Vec<String> {
        let mut v: Vec<String> = HK_MOD_V1
            .families
            .iter()
            .map(|f| f.name.to_owned())
            .collect();
        v.push(UNKNOWN.to_owned());
        v
    }

    /// A normalised random distribution over the families plus `unknown`.
    fn a_distribution(rng: &mut Rng) -> Vec<LabelP> {
        let mut dist: Vec<LabelP> = labels()
            .into_iter()
            .map(|label| LabelP {
                label,
                p: rng.weight(),
            })
            .collect();
        let sum: f64 = dist.iter().map(|lp| lp.p).sum();
        for lp in &mut dist {
            lp.p /= sum;
        }
        dist
    }

    /// A classification built from `likelihood` fused with `prior`, exactly as the cascade builds
    /// one. Asserted valid, so the property tests run on legal rows only.
    fn a_classification(
        likelihood: Vec<LabelP>,
        prior: Option<&FamilyPriorSet>,
        snr_db: Option<f64>,
    ) -> Classification {
        let open_set = p_of(&likelihood, UNKNOWN);
        let fused = fuse(&likelihood, open_set, prior);
        let mut posterior = fused.posterior;
        // No call is certain: the contract caps what may be reported.
        cap_and_normalise(&mut posterior);

        let top = posterior
            .iter()
            .max_by(|a, b| a.p.total_cmp(&b.p).then_with(|| b.label.cmp(&a.label)))
            .expect("non-empty")
            .clone();
        let family = top.label.clone();
        let confidence = top.p;
        assert!(confidence <= MAX_CONFIDENCE, "no call is certain");
        let coarse = if family == UNKNOWN {
            Coarse::Unknown
        } else {
            HK_MOD_V1.coarse_of(&family).unwrap_or(Coarse::Unknown)
        };

        let c = Classification {
            schema: CLASSIFICATION_SCHEMA,
            t: t0(),
            taxonomy: TaxonomyRef::current(),
            input: None,
            coarse,
            entropy_norm: entropy_norm(&posterior, HK_MOD_V1.families.len() + 1),
            likelihood,
            posterior,
            prior: prior.map(|p| PriorUse {
                prior_ref: p.prior_ref.clone(),
                lambda: p.lambda,
                dist: p.dist.clone(),
            }),
            family,
            confidence,
            class: None,
            open_set_score: open_set,
            stage: Stage::FeatureTree,
            provenance: ClassProvenance {
                rules: "hk-classify/tree@1".to_owned(),
                features_version: crate::signature::EMISSION_FEATURES_VERSION,
                features_ref: None,
                ml: None,
                snr_db,
                snr_gate_db: 0.0,
                gated: false,
                thresholds: super::super::THRESHOLDS_VERSION.to_owned(),
                suspect: Default::default(),
                power_mode: None,
            },
            flags: Vec::new(),
            reasons: Vec::new(),
        };
        c.validate().expect("the fixture must be a legal row");
        c
    }

    /// A prior that puts nearly all its mass on one family — the strongest a prior may legally be.
    fn a_hard_prior(family: &str) -> FamilyPriorSet {
        let known: Vec<&str> = HK_MOD_V1.families.iter().map(|f| f.name).collect();
        let others = (known.len() - 1) as f64;
        let dist = known
            .iter()
            .map(|name| LabelP {
                label: (*name).to_owned(),
                p: if *name == family {
                    0.9
                } else {
                    0.1 / others.max(1.0)
                },
            })
            .collect();
        let p = FamilyPriorSet {
            prior_ref: "test/hard@1".to_owned(),
            lambda: [0.1, 0.9, 0.0, 0.0],
            dist,
        };
        p.validate().expect("the fixture prior must be legal");
        p
    }

    fn seed_of(c: &Classification) -> SearchSeed {
        SearchSeed::assemble(SeedInputs {
            emitter: EmitterId::from_uuid(uuid::Uuid::nil()),
            t: t0(),
            classification: c,
            features: None,
            signature_match: None,
            catalogue: &[],
            cluster: None,
        })
    }

    fn a_signature(id: &str, family: &str) -> Signature {
        Signature {
            schema: SIGNATURE_SCHEMA,
            id: id.to_owned(),
            version: 1,
            name: format!("test {id}"),
            kind: SignatureKind::Protocol,
            taxonomy: Some(TaxonomyRef::current()),
            family: Some(family.to_owned()),
            class: None,
            fields: Default::default(),
            min_discriminating: 2,
            recipe: Some(RecipeRef {
                id: "test-recipe".to_owned(),
                version: 1,
            }),
            provenance: SignatureProvenance::Builtin,
            author: "test".to_owned(),
            created_at: t0(),
            supersedes: None,
            bands_hz: Vec::new(),
            notes: None,
        }
    }

    /// A match against `signature`. A `full` match has nothing missing by definition, a `partial`
    /// names what is still unmeasured, and a `none` carries no candidate at all.
    fn a_match(outcome: MatchOutcome, signature: &Signature) -> SignatureMatch {
        let candidates = match outcome {
            MatchOutcome::None => Vec::new(),
            _ => vec![SignatureCandidate {
                signature: SignatureRef {
                    id: signature.id.clone(),
                    version: signature.version,
                },
                name: signature.name.clone(),
                score: 0.9,
                agreement: Vec::new(),
                missing: match outcome {
                    MatchOutcome::Full => Vec::new(),
                    _ => vec!["sync_word".to_owned()],
                },
                conflicting: Vec::new(),
                recipe: signature.recipe.clone(),
            }],
        };
        let m = SignatureMatch {
            schema: SIGNATURE_SCHEMA,
            emitter_id: EmitterId::from_uuid(uuid::Uuid::nil()),
            t: t0(),
            outcome,
            features_ref: None,
            signatures_rev: 1,
            candidates,
            reasons: Vec::new(),
        };
        m.validate().expect("the fixture must be a legal match");
        m
    }

    // ---------------------------------------------------------------- rule 1

    /// **Rule 1: the posterior orders the search.**
    ///
    /// With nothing raising anything, the order is exactly the posterior's, descending. The order
    /// does not depend on the order the distributions arrive in, and swapping two families'
    /// posterior mass swaps their place in the search.
    #[test]
    fn the_posterior_orders_the_search() {
        let mut rng = Rng(0xA215_0001);
        for _ in 0..TRIALS {
            let c = a_classification(a_distribution(&mut rng), None, Some(30.0));
            let seed = seed_of(&c);
            seed.validate().unwrap();

            let got: Vec<&str> = seed
                .budget_hint
                .families_ordered
                .iter()
                .map(|h| h.family.as_str())
                .collect();

            // Exactly a posterior-descending sort, label as the tie-break.
            let mut want: Vec<(&str, f64)> = seed
                .budget_hint
                .families_ordered
                .iter()
                .map(|h| (h.family.as_str(), h.posterior))
                .collect();
            want.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
            let want: Vec<&str> = want.into_iter().map(|(f, _)| f).collect();
            assert_eq!(got, want, "order must follow the posterior");

            // Monotone, and the likelihood is *not* what sorted it.
            for w in seed.budget_hint.families_ordered.windows(2) {
                assert!(
                    w[0].posterior >= w[1].posterior,
                    "{} ({}) before {} ({})",
                    w[0].family,
                    w[0].posterior,
                    w[1].family,
                    w[1].posterior
                );
            }

            // Presentation order of the input must not matter.
            let mut shuffled = c.clone();
            shuffled.posterior.reverse();
            shuffled.likelihood.rotate_left(1);
            let shuffled_seed = seed_of(&shuffled);
            let reordered: Vec<&str> = shuffled_seed
                .budget_hint
                .families_ordered
                .iter()
                .map(|h| h.family.as_str())
                .collect();
            assert_eq!(got, reordered, "assembly must be order-independent");
        }
    }

    /// Moving posterior mass between two families moves them past each other in the order.
    #[test]
    fn the_posterior_is_what_moves_a_hypothesis_up() {
        let mut rng = Rng(0xA215_0002);
        for _ in 0..TRIALS {
            let c = a_classification(a_distribution(&mut rng), None, Some(30.0));
            let order: Vec<String> = seed_of(&c)
                .budget_hint
                .families_ordered
                .iter()
                .map(|h| h.family.clone())
                .collect();

            let (i, j) = (0, order.len() - 1);
            let (first, last) = (order[i].clone(), order[j].clone());
            let mut swapped = c.clone();
            let pi = p_of(&swapped.posterior, &first);
            let pj = p_of(&swapped.posterior, &last);
            if pi == pj {
                continue;
            }
            for lp in &mut swapped.posterior {
                if lp.label == first {
                    lp.p = pj;
                } else if lp.label == last {
                    lp.p = pi;
                }
            }
            let after: Vec<String> = seed_of(&swapped)
                .budget_hint
                .families_ordered
                .iter()
                .map(|h| h.family.clone())
                .collect();
            let pos = |v: &[String], f: &str| v.iter().position(|x| x == f).unwrap();
            assert!(
                pos(&after, &last) < pos(&after, &first),
                "the family given the larger posterior must be searched first"
            );
        }
    }

    // ---------------------------------------------------------------- rule 2

    /// **Rule 2: pruning consults only the likelihood.**
    ///
    /// The same evidence with any prior gives the same prune flags, so a strong prior can never
    /// prune a hypothesis the evidence keeps alive (nor rescue one it does not). A family above
    /// the likelihood share is never pruned however small its posterior, and a family below its
    /// SNR gate — not measured, not ruled out — is never pruned at all.
    #[test]
    fn only_the_likelihood_prunes() {
        let mut rng = Rng(0xA215_0003);
        let mut posteriors_actually_differed = 0;
        let mut prior_starved_a_live_branch = 0;

        for _ in 0..TRIALS {
            let likelihood = a_distribution(&mut rng);
            let families: Vec<&str> = HK_MOD_V1.families.iter().map(|f| f.name).collect();
            let favoured = families[rng.below(families.len())];
            let prior = a_hard_prior(favoured);

            let flat = a_classification(likelihood.clone(), None, Some(30.0));
            let biased = a_classification(likelihood, Some(&prior), Some(30.0));

            let a = seed_of(&flat);
            let b = seed_of(&biased);
            a.validate().unwrap();
            b.validate().unwrap();

            let ha = &a.budget_hint.families_ordered;
            let hb = &b.budget_hint.families_ordered;
            assert_eq!(ha.len(), hb.len(), "a prior must not add or drop a family");

            for h in ha {
                let other = hb
                    .iter()
                    .find(|o| o.family == h.family)
                    .expect("a prior must never remove a hypothesis");

                assert_eq!(
                    h.likelihood, other.likelihood,
                    "{}: the evidence must be untouched by the prior",
                    h.family
                );
                assert_eq!(
                    h.prune, other.prune,
                    "{}: prune changed when only the prior changed (posterior {} vs {})",
                    h.family, h.posterior, other.posterior
                );

                if h.posterior != other.posterior {
                    posteriors_actually_differed += 1;
                }
                // The case the rule exists for: evidence keeps it alive, the prior buries it.
                if !h.prune && other.posterior < PRUNE_LIKELIHOOD_SHARE {
                    prior_starved_a_live_branch += 1;
                    assert!(
                        !other.prune,
                        "{}: a low posterior pruned a branch the likelihood keeps alive",
                        h.family
                    );
                }

                // The two standing guarantees.
                if h.likelihood >= PRUNE_LIKELIHOOD_SHARE {
                    assert!(!h.prune, "{}: pruned above the likelihood share", h.family);
                }
                if h.below_gate {
                    assert!(!h.prune, "{}: not measured is not ruled out", h.family);
                }
            }
        }

        assert!(
            posteriors_actually_differed > 0,
            "the priors must actually have moved the posteriors for this to prove anything"
        );
        assert!(
            prior_starved_a_live_branch > 0,
            "the test never exercised a prior burying an evidence-alive branch"
        );
    }

    /// An ungated measurement leaves every gated family below its gate, and none of them pruned.
    #[test]
    fn an_unmeasured_snr_prunes_nothing() {
        let mut rng = Rng(0xA215_0004);
        let c = a_classification(a_distribution(&mut rng), None, None);
        let seed = seed_of(&c);
        seed.validate().unwrap();
        let gated: Vec<&Hypothesis> = seed
            .budget_hint
            .families_ordered
            .iter()
            .filter(|h| h.below_gate)
            .collect();
        assert!(!gated.is_empty(), "the taxonomy has SNR-gated families");
        for h in gated {
            assert!(!h.prune, "{}: below gate must never be pruned", h.family);
            assert!(h.reasons.iter().any(|r| r == "below_gate_not_measured"));
        }
    }

    // ---------------------------------------------------------------- rule 3

    /// **Rule 3: the open-search share is always reserved.**
    ///
    /// At least `max(p_unknown, 0.2)` of the budget, including when the classifier is as confident
    /// as it is allowed to get.
    #[test]
    fn open_search_is_always_reserved() {
        let mut rng = Rng(0xA215_0005);
        for _ in 0..TRIALS {
            let c = a_classification(a_distribution(&mut rng), None, Some(30.0));
            let b = seed_of(&c).budget_hint;
            let p_unknown = p_of(&c.posterior, UNKNOWN).max(p_of(&c.likelihood, UNKNOWN));
            assert!(
                b.open_search_min_share >= OPEN_SEARCH_MIN_SHARE,
                "share {} fell below the floor",
                b.open_search_min_share
            );
            assert!(
                b.open_search_min_share >= p_unknown - 1e-12,
                "share {} is below p_unknown {p_unknown}",
                b.open_search_min_share
            );
            assert!(b.open_search_min_share <= 1.0);
        }

        // The confident case: the mass concentrated on one family, `unknown` ~ 0.
        let mut confident: Vec<LabelP> = labels()
            .into_iter()
            .map(|label| {
                let p = if label == UNKNOWN { 1e-4 } else { 1.0 };
                LabelP { label, p }
            })
            .collect();
        confident[0].p = 40.0;
        let sum: f64 = confident.iter().map(|lp| lp.p).sum();
        for lp in &mut confident {
            lp.p /= sum;
        }
        let c = a_classification(confident, None, Some(40.0));
        assert!(c.confidence > 0.8, "fixture must be a confident call");
        let b = seed_of(&c).budget_hint;
        assert!(
            b.open_search_min_share >= OPEN_SEARCH_MIN_SHARE,
            "a confident classifier must still fund open search, got {}",
            b.open_search_min_share
        );

        // And the opposite: mostly unknown, where the floor must rise with p_unknown.
        let mut mostly_unknown: Vec<LabelP> = labels()
            .into_iter()
            .map(|label| LabelP { label, p: 0.01_f64 })
            .collect();
        let rest: f64 = 0.01 * (mostly_unknown.len() - 1) as f64;
        for lp in &mut mostly_unknown {
            if lp.label == UNKNOWN {
                lp.p = 1.0 - rest;
            }
        }
        let c = a_classification(mostly_unknown, None, Some(40.0));
        let b = seed_of(&c).budget_hint;
        assert!(
            b.open_search_min_share >= 0.9,
            "p_unknown must raise the reserved share, got {}",
            b.open_search_min_share
        );
    }

    // ------------------------------------------- signatures and clusters seed

    /// **Signatures and clusters seed but never dictate.**
    ///
    /// A match or a cluster may only move a family earlier. It never removes a hypothesis, never
    /// changes a posterior, a likelihood or a prune flag, and never sets an identity or a family:
    /// the classification comes back exactly as it went in.
    #[test]
    fn signatures_and_clusters_raise_order_but_never_dictate() {
        let mut rng = Rng(0xA215_0006);
        let mut actually_raised = 0;

        for _ in 0..TRIALS {
            let c = a_classification(a_distribution(&mut rng), None, Some(30.0));
            let bare = seed_of(&c);

            let families: Vec<&str> = HK_MOD_V1.families.iter().map(|f| f.name).collect();
            let target = families[rng.below(families.len())];
            let sig = a_signature("test-sig", target);
            let catalogue = [sig.clone()];
            let m = a_match(MatchOutcome::Full, &sig);
            let cluster_family = families[rng.below(families.len())];
            let cluster = ClusterSeed {
                cluster_id: "cluster:test".to_owned(),
                state: ClusterState::Active,
                members: vec![EmitterId::from_uuid(uuid::Uuid::nil())],
                best_pipeline: Some(ClusterPipeline {
                    recipe: RecipeRef {
                        id: "cluster-recipe".to_owned(),
                        version: 2,
                    },
                    signature: None,
                    family: Some(cluster_family.to_owned()),
                    evidence_score: None,
                }),
            };

            let seeded = SearchSeed::assemble(SeedInputs {
                emitter: EmitterId::from_uuid(uuid::Uuid::nil()),
                t: t0(),
                classification: &c,
                features: None,
                signature_match: Some(&m),
                catalogue: &catalogue,
                cluster: Some(&cluster),
            });
            seeded.validate().unwrap();

            // Nothing added, nothing removed.
            let before: BTreeSet<&str> = bare
                .budget_hint
                .families_ordered
                .iter()
                .map(|h| h.family.as_str())
                .collect();
            let after: BTreeSet<&str> = seeded
                .budget_hint
                .families_ordered
                .iter()
                .map(|h| h.family.as_str())
                .collect();
            assert_eq!(
                before, after,
                "a suggestion must not add or remove a family"
            );

            // No number and no flag moved.
            for h in &bare.budget_hint.families_ordered {
                let s = seeded
                    .budget_hint
                    .families_ordered
                    .iter()
                    .find(|o| o.family == h.family)
                    .unwrap();
                assert_eq!(h.posterior, s.posterior, "{}: posterior moved", h.family);
                assert_eq!(h.likelihood, s.likelihood, "{}: likelihood moved", h.family);
                assert_eq!(h.prune, s.prune, "{}: prune flag moved", h.family);
            }

            // The identity side is untouched.
            assert_eq!(
                c, seeded.classification,
                "the seed must never rewrite the classification"
            );
            assert_eq!(
                seeded.budget_hint.open_search_min_share, bare.budget_hint.open_search_min_share,
                "a suggestion must not eat the open share"
            );

            // The boost may only move the family earlier.
            let pos = |s: &SearchSeed, f: &str| {
                s.budget_hint
                    .families_ordered
                    .iter()
                    .position(|h| h.family == f)
                    .unwrap()
            };
            assert!(
                pos(&seeded, target) <= pos(&bare, target),
                "{target}: a full match must not push a family back"
            );
            if pos(&seeded, target) < pos(&bare, target) {
                actually_raised += 1;
            }

            let raised = seeded
                .budget_hint
                .families_ordered
                .iter()
                .find(|h| h.family == target)
                .unwrap();
            assert!(raised.boosts.contains(&SeedBoost::SignatureFull));
            assert!(
                raised.recipes.iter().any(|r| r.id == "test-recipe"),
                "the fast-path recipe must be offered"
            );
            assert!(
                raised.missing.is_empty(),
                "a full match leaves nothing for the search to estimate"
            );
        }

        assert!(
            actually_raised > 0,
            "the test never exercised a boost actually changing the order"
        );
    }

    /// A `partial` match raises its family too, and names what the search must still estimate.
    #[test]
    fn a_partial_match_names_what_the_search_must_estimate() {
        let mut rng = Rng(0xA215_000B);
        let c = a_classification(a_distribution(&mut rng), None, Some(30.0));
        let sig = a_signature("test-sig", "fsk");
        let m = a_match(MatchOutcome::Partial, &sig);
        let seeded = SearchSeed::assemble(SeedInputs {
            emitter: EmitterId::from_uuid(uuid::Uuid::nil()),
            t: t0(),
            classification: &c,
            features: None,
            signature_match: Some(&m),
            catalogue: std::slice::from_ref(&sig),
            cluster: None,
        });
        seeded.validate().unwrap();

        let fsk = seeded
            .budget_hint
            .families_ordered
            .iter()
            .find(|h| h.family == "fsk")
            .unwrap();
        assert!(fsk.boosts.contains(&SeedBoost::SignaturePartial));
        assert!(
            !fsk.boosts.contains(&SeedBoost::SignatureFull),
            "a partial match must not read as a full one"
        );
        assert_eq!(
            fsk.missing,
            vec!["sync_word".to_owned()],
            "what the search must estimate must be named"
        );
        assert_eq!(
            fsk.recipes.len(),
            1,
            "a partial match still offers its template as a fast path"
        );
        assert_eq!(
            c, seeded.classification,
            "a partial match sets nothing at all"
        );
    }

    /// A `none` outcome raises nothing: the catalogue having nothing to say is not evidence.
    #[test]
    fn a_none_match_changes_nothing() {
        let mut rng = Rng(0xA215_0007);
        let c = a_classification(a_distribution(&mut rng), None, Some(30.0));
        let sig = a_signature("test-sig", "fsk");
        let m = a_match(MatchOutcome::None, &sig);
        let seeded = SearchSeed::assemble(SeedInputs {
            emitter: EmitterId::from_uuid(uuid::Uuid::nil()),
            t: t0(),
            classification: &c,
            features: None,
            signature_match: Some(&m),
            catalogue: &[sig],
            cluster: None,
        });
        assert_eq!(
            seeded.budget_hint.families_ordered,
            seed_of(&c).budget_hint.families_ordered
        );
    }

    /// A signature naming a family outside this taxonomy raises nothing and invents nothing.
    #[test]
    fn a_signature_never_invents_a_family() {
        let mut rng = Rng(0xA215_0008);
        let c = a_classification(a_distribution(&mut rng), None, Some(30.0));
        let sig = a_signature("test-sig", "definitely-not-a-family");
        let m = a_match(MatchOutcome::Full, &sig);
        let seeded = SearchSeed::assemble(SeedInputs {
            emitter: EmitterId::from_uuid(uuid::Uuid::nil()),
            t: t0(),
            classification: &c,
            features: None,
            signature_match: Some(&m),
            catalogue: &[sig],
            cluster: None,
        });
        assert!(
            seeded
                .budget_hint
                .families_ordered
                .iter()
                .all(|h| h.family != "definitely-not-a-family")
        );
        assert!(
            seeded
                .reasons
                .iter()
                .any(|r| r == "signature_family_outside_taxonomy")
        );
    }

    /// `unknown` is the reserved share, never a branch to try or prune.
    #[test]
    fn unknown_is_never_a_branch() {
        let mut rng = Rng(0xA215_0009);
        for _ in 0..50 {
            let c = a_classification(a_distribution(&mut rng), None, Some(30.0));
            let seed = seed_of(&c);
            assert!(
                seed.budget_hint
                    .families_ordered
                    .iter()
                    .all(|h| h.family != UNKNOWN)
            );
        }
    }

    /// A seed survives the API boundary and is re-checked, not trusted, on the way back.
    #[test]
    fn a_seed_round_trips_and_is_revalidated() {
        let mut rng = Rng(0xA215_000A);
        let c = a_classification(a_distribution(&mut rng), None, Some(30.0));
        let seed = seed_of(&c);
        let json = serde_json::to_string(&seed).unwrap();
        let back: SearchSeed = serde_json::from_str(&json).unwrap();
        assert_eq!(seed, back);
        back.validate().unwrap();

        // A tampered seed is caught by each rule in turn.
        let mut bad = back.clone();
        bad.budget_hint.open_search_min_share = 0.05;
        assert!(bad.validate().is_err(), "rule 3 must be checked");

        let mut bad = back.clone();
        if let Some(h) = bad.budget_hint.families_ordered.first_mut() {
            h.prune = !h.prune;
        }
        assert!(bad.validate().is_err(), "rule 2 must be checked");

        let mut bad = back;
        bad.budget_hint.families_ordered.reverse();
        if bad.budget_hint.families_ordered.len() > 1 {
            assert!(bad.validate().is_err(), "rule 1 must be checked");
        }
    }
}
