//! Evidence in bits, and the two keys a beam node carries (ADR-0015 §1.3, §2.1, §13.1).
//!
//! The per-block evidence (M-2) and the §13.1 combination — `b_j` = the sum over declared groups
//! of the **maximum** within each group, undeclared meaning one group (T-660 (c)) — are not here
//! yet. What is here are the parts every later ticket must agree on: the record, the prior's
//! clipping, the look-elsewhere charge's definition, and the **separation of prior from rank**.

use std::cmp::Ordering;
use std::collections::BTreeMap;

pub use hk_model::synth::{
    EVIDENCE_SET_CAPACITY, Evidence, EvidenceSet, EvidenceSetFull, GroupId, MetricId,
    quality_from_bits,
};
use serde::{Deserialize, Serialize};

use crate::stage::Stage;

/// The lower clip of `prior_bits` (ADR-0015 §1.3: clipped to [−8, 0]).
pub const PRIOR_BITS_MIN: f32 = -8.0;

/// `prior_bits = log₂ π(h)`, clipped to [[`PRIOR_BITS_MIN`], 0] (ADR-0015 §1.3). A zero,
/// negative or non-finite probability clips to the minimum: a prior can make a hypothesis late,
/// never absent (§4.2, "defer, don't delete").
pub fn prior_bits(p: f64) -> f32 {
    if !p.is_finite() || p <= 0.0 {
        return PRIOR_BITS_MIN;
    }
    (p.log2() as f32).clamp(PRIOR_BITS_MIN, 0.0)
}

/// The look-elsewhere cost `L_j = log₂(hypotheses evaluated at stage j in this job)` (ADR-0015
/// §1.3). Charged by the engine, never by a block (§2.2). Zero or one hypothesis costs nothing.
pub fn look_elsewhere_bits(hypotheses: u64) -> f32 {
    if hypotheses <= 1 {
        0.0
    } else {
        (hypotheses as f64).log2() as f32
    }
}

/// `b_j` (ADR-0015 §1.3, as amended by §13.1, T-660 (c)): the sum, over declared dependence
/// groups, of the **maximum** bits within each group. `entries` must all speak for one stage from
/// one block's `evidence()` call (or calls since the engine's last `reset()`) — that is what makes
/// [`GroupId::Undeclared`] mean "one group" rather than "one group across every block that ever
/// touched this stage": the default is **one group per (block, stage)**, and a caller combining
/// several blocks' entries at the same stage must group by block first.
///
/// The sum survives only *between* declared groups; two metrics a block never split stay one
/// group and score their maximum, never their sum — publishing a second metric without a
/// declaration is not a way to be paid twice for one statistic. Not capped by `cap_j`: the caller
/// applies `min(b_j, cap_j)` (ADR-0015 §1.3) after this.
pub fn combine_stage_bits<'a>(entries: impl IntoIterator<Item = &'a Evidence>) -> f32 {
    let mut max_by_group: BTreeMap<GroupId, f32> = BTreeMap::new();
    for e in entries {
        max_by_group
            .entry(e.group)
            .and_modify(|b| {
                if e.bits > *b {
                    *b = e.bits;
                }
            })
            .or_insert(e.bits);
    }
    max_by_group.values().sum()
}

/// A beam node's two numbers, kept apart (ADR-0015 §1.3).
///
/// - [`Self::search_key`] = `evidence_bits + prior_bits + optimistic_remaining`: what the beam
///   expands next.
/// - [`Self::cmp_rank`] = `(stage_reached, evidence_bits)`: what a result ranks by. **It cannot
///   see `prior_bits`**, so a strong template prior can never rank, let alone confirm, a weak
///   decode.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeScore {
    /// Deepest stage the prefix reaches.
    pub stage_reached: Stage,
    /// `Σ_{j≤k} min(b_j, cap_j) − L_j`, after look-elsewhere.
    pub evidence_bits: f32,
    /// `log₂ π(h)`, clipped. Reported; orders the search; never ranks.
    pub prior_bits: f32,
}

impl NodeScore {
    /// The search-order key for a node whose remaining stages could add at most
    /// `optimistic_remaining` bits.
    pub fn search_key(&self, optimistic_remaining: f32) -> f32 {
        self.evidence_bits + self.prior_bits + optimistic_remaining
    }

    /// The result-rank key, `(stage_reached, evidence_bits)`.
    pub fn rank_key(&self) -> (Stage, f32) {
        (self.stage_reached, self.evidence_bits)
    }

    /// Orders two results by [`Self::rank_key`]: deeper stage first, then more evidence. `Greater`
    /// means `self` ranks above `other`. Total over floats (NaN sorts per `f32::total_cmp`).
    pub fn cmp_rank(&self, other: &Self) -> Ordering {
        self.stage_reached
            .cmp(&other.stage_reached)
            .then_with(|| self.evidence_bits.total_cmp(&other.evidence_bits))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stage::Stage;

    fn evidence(metric: MetricId, group: GroupId, bits: f32) -> Evidence {
        Evidence::new(Stage::S2, metric, group, 0.0, 112, bits)
    }

    #[test]
    fn two_metrics_with_no_declared_groups_score_max_not_sum() {
        // T-660 (c): loading a file (or, here, evidence emitted) with two metrics and no `groups`
        // key must score the stage's maximum, not the sum. `snr` and `evm` both default to
        // GroupId::Undeclared (evidence.rs's own parse test shows this is what a bare JSON record
        // deserialises to), so per ADR-0015 §13.1 they are ONE group.
        let entries = [
            evidence(MetricId::Snr, GroupId::Undeclared, 4.0),
            evidence(MetricId::Evm, GroupId::Undeclared, 5.5),
        ];
        // Max, not the 9.5-bit sum a naive combiner would report.
        assert_eq!(combine_stage_bits(&entries), 5.5);
    }

    #[test]
    fn declared_groups_sum_between_groups_and_max_within_one() {
        let entries = [
            evidence(MetricId::Snr, GroupId::SoftQuality, 4.0),
            evidence(MetricId::Evm, GroupId::SoftQuality, 5.5),
            evidence(MetricId::TimingVar, GroupId::SoftQuality, 3.0),
            evidence(MetricId::EyeOpen, GroupId::Eye, 3.0),
        ];
        // soft_quality maxes to 5.5; eye is its own group and adds in full: 5.5 + 3.0.
        assert_eq!(combine_stage_bits(&entries), 8.5);
    }

    #[test]
    fn a_singleton_group_and_an_empty_set_are_the_identity_cases() {
        assert_eq!(combine_stage_bits(&[]), 0.0);
        let one = [evidence(MetricId::Bimodality, GroupId::DemodShape, 2.5)];
        assert_eq!(combine_stage_bits(&one), 2.5);
    }

    #[test]
    fn prior_bits_clip_to_minus_8_and_0() {
        assert_eq!(prior_bits(1.0), 0.0);
        assert_eq!(prior_bits(0.5), -1.0);
        assert_eq!(prior_bits(1e-9), PRIOR_BITS_MIN);
        assert_eq!(prior_bits(0.0), PRIOR_BITS_MIN);
        assert_eq!(prior_bits(f64::NAN), PRIOR_BITS_MIN);
        assert_eq!(prior_bits(1.5), 0.0);
    }

    #[test]
    fn look_elsewhere_is_log2_of_hypotheses() {
        assert_eq!(look_elsewhere_bits(0), 0.0);
        assert_eq!(look_elsewhere_bits(1), 0.0);
        assert_eq!(look_elsewhere_bits(1024), 10.0);
    }

    #[test]
    fn the_prior_orders_the_search_but_never_the_rank() {
        let weak_with_template = NodeScore {
            stage_reached: Stage::S5,
            evidence_bits: 20.0,
            prior_bits: 0.0,
        };
        let strong_open = NodeScore {
            stage_reached: Stage::S5,
            evidence_bits: 25.0,
            prior_bits: PRIOR_BITS_MIN,
        };
        // The template's prior expands it first (20 + 0 > 25 − 8)...
        assert!(weak_with_template.search_key(0.0) > strong_open.search_key(0.0));
        // ...but the rank is evidence alone, and changing a prior changes no rank.
        assert_eq!(strong_open.cmp_rank(&weak_with_template), Ordering::Greater);
        let boosted = NodeScore {
            prior_bits: 0.0,
            ..weak_with_template
        };
        let demoted = NodeScore {
            prior_bits: PRIOR_BITS_MIN,
            ..weak_with_template
        };
        assert_eq!(boosted.cmp_rank(&demoted), Ordering::Equal);
    }

    #[test]
    fn a_deeper_stage_outranks_more_bits_at_a_shallower_one() {
        let framed = NodeScore {
            stage_reached: Stage::S4,
            evidence_bits: 11.0,
            prior_bits: 0.0,
        };
        let loud_carrier = NodeScore {
            stage_reached: Stage::S0,
            evidence_bits: 12.0,
            prior_bits: 0.0,
        };
        assert_eq!(framed.cmp_rank(&loud_carrier), Ordering::Greater);
    }
}
