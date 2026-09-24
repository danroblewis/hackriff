//! Evidence in bits, and the two keys a beam node carries (ADR-0015 §1.3, §2.1, §13.1).
//!
//! The per-block evidence (M-2) and the §13.1 combination — `b_j` = the sum over declared groups
//! of the **maximum** within each group, undeclared meaning one group (T-660 (c)) — are not here
//! yet. What is here are the parts every later ticket must agree on: the record, the prior's
//! clipping, the look-elsewhere charge's definition, and the **separation of prior from rank**.

use std::cmp::Ordering;

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
