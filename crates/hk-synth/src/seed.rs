//! Seeding from M3 (ADR-0015 §4.2): the one adapter over ADR-0016's `SearchSeed`
//! (`hk_model::classify::SearchSeed`, T-215).
//!
//! ADR-0015 §4.2 names the M3 fields it reads and says "if ADR-0016 names them differently, one
//! adapter in `hk-synth::seed` absorbs the difference". This is that adapter, at scaffold depth:
//! it turns the seed's ordered family hypotheses into [`SeededFamily`] rows that carry
//! `seed_source` and `family` from the start (ADR-0021 §3, §12's M-1 amendment). The template
//! match term, `band_factor` and the signature fast path are M-5's.
//!
//! **Deferral follows ADR-0016, not ADR-0015's wording.** §4.2 says "a family with posterior
//! < 0.02 is deferred"; ADR-0016 §8 and T-215 say only the **likelihood** prunes and a prior may
//! reorder but never defer. The measured-evidence rule wins (blind first), so a family is deferred
//! exactly when `Hypothesis::prune` says so. Recorded in ADR-0015 §16 as correction C2.

use hk_model::classify::{BudgetHint, SearchSeed};
use serde::{Deserialize, Serialize};

use crate::candidate::SeedSource;
use crate::evidence::prior_bits;

/// One family hypothesis as the search will enqueue it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeededFamily {
    /// `hk-mod@1` family.
    pub family: String,
    /// Where the hypothesis came from: `signature` when a signature or cluster raised it,
    /// otherwise `classification`.
    pub seed_source: SeedSource,
    /// `log₂` of the classification posterior, clipped to [−8, 0]. Orders; never ranks.
    pub prior_bits: f32,
    /// The posterior it came from (reported in `deferred_prior` trace detail).
    pub posterior: f64,
    /// Deferred to the side queue: ADR-0016's likelihood rule, never the posterior.
    pub deferred: bool,
}

/// The seeding plan: families in search order, plus the reserved open-search share.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeedPlan {
    /// Every family the seed offered, in the seed's order (active and deferred alike — deferred
    /// is not deleted).
    pub families: Vec<SeededFamily>,
    /// Share of the evaluation budget open skeletons always get: ≥ max(P(unknown), 0.2).
    pub open_search_min_share: f64,
}

impl SeedPlan {
    /// Reads an ADR-0016 seed. Pure: writes nothing, reorders nothing the seed ordered, and never
    /// drops a family.
    pub fn from_search_seed(seed: &SearchSeed) -> Self {
        Self::from_budget_hint(&seed.budget_hint)
    }

    /// The part of the seed read at scaffold depth: its ordered family hypotheses and open share.
    pub fn from_budget_hint(hint: &BudgetHint) -> Self {
        let families = hint
            .families_ordered
            .iter()
            .map(|h| SeededFamily {
                family: h.family.clone(),
                seed_source: if h.boosts.is_empty() {
                    SeedSource::Classification
                } else {
                    SeedSource::Signature
                },
                prior_bits: prior_bits(h.posterior),
                posterior: h.posterior,
                deferred: h.prune,
            })
            .collect();
        Self {
            families,
            open_search_min_share: hint.open_search_min_share,
        }
    }

    /// The families to try first, in order.
    pub fn active(&self) -> impl Iterator<Item = &SeededFamily> {
        self.families.iter().filter(|f| !f.deferred)
    }

    /// The deferred families, in order. They run if budget remains.
    pub fn deferred(&self) -> impl Iterator<Item = &SeededFamily> {
        self.families.iter().filter(|f| f.deferred)
    }
}

#[cfg(test)]
mod tests {
    use hk_model::classify::{Hypothesis, SeedBoost};

    use super::*;

    fn hyp(family: &str, posterior: f64, likelihood: f64, prune: bool) -> Hypothesis {
        Hypothesis {
            family: family.into(),
            posterior,
            likelihood,
            prune,
            below_gate: false,
            boosts: Vec::new(),
            recipes: Vec::new(),
            missing: Vec::new(),
            reasons: Vec::new(),
        }
    }

    #[test]
    fn a_low_posterior_alone_never_defers_and_nothing_is_dropped() {
        // A family whose prior crushed its posterior below 0.02, while the evidence keeps it alive:
        // ADR-0015 §4.2's wording would defer it; ADR-0016's rule does not.
        let mut boosted = hyp("fsk", 0.6, 0.5, false);
        boosted.boosts.push(SeedBoost::SignatureFull);
        let hint = BudgetHint {
            open_search_min_share: 0.25,
            p_unknown: 0.25,
            families_ordered: vec![
                boosted,
                hyp("ook", 0.01, 0.3, false),
                hyp("psk", 0.1, 0.001, true),
            ],
        };
        let plan = SeedPlan::from_budget_hint(&hint);
        assert_eq!(plan.families.len(), 3);
        assert_eq!(
            plan.active().map(|f| f.family.as_str()).collect::<Vec<_>>(),
            ["fsk", "ook"]
        );
        assert_eq!(
            plan.deferred()
                .map(|f| f.family.as_str())
                .collect::<Vec<_>>(),
            ["psk"]
        );
        assert_eq!(plan.families[0].seed_source, SeedSource::Signature);
        assert_eq!(plan.families[1].seed_source, SeedSource::Classification);
        assert!(plan.families[1].prior_bits < -6.0);
        assert_eq!(plan.open_search_min_share, 0.25);
    }
}
