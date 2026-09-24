//! The stage ladder and its default caps and floors (ADR-0015 §1.1, §1.3, as restated by §13.2).
//!
//! The numbers are the ADR's defaults. They are **first guesses** (ADR-0015 §7: "initial
//! guesses, unverified"); T-660 measures whether `floor_j = 6` survives §13.1's maximum rule, and
//! the M-12 review may tighten them, never loosen them after seeing results.

pub use hk_model::synth::Stage;

/// Per-metric ceiling on any **calibrated** metric's claim, in bits (ADR-0015 §13.2 item 1).
/// Above 6 bits the quantisation discount is unmeasured, not small. A calibrated metric claiming
/// more is a loader error, not a capped value.
pub const CALIBRATED_CLAIM_CAP_BITS: f32 = 6.0;

/// The default per-stage cap `cap_j` on `b_j`, in bits, or `None` for no cap (ADR-0015 §1.3):
/// 12 for S0–S3 (now only a belt behind [`CALIBRATED_CLAIM_CAP_BITS`], §13.2), 32 for S4
/// (reachable only analytically), none for S5–S6.
pub const fn default_cap_bits(stage: Stage) -> Option<f32> {
    match stage {
        Stage::S0 | Stage::S1 | Stage::S2 | Stage::S3 => Some(12.0),
        Stage::S4 => Some(32.0),
        Stage::S5 | Stage::S6 => None,
    }
}

/// The default pruning floor `floor_j`, in bits, or `None` where the ADR states none (ADR-0015
/// §1.3): 6 for S0–S3, 10 for S4–S5. S6 has no stated floor.
///
/// A floor is a bits → raw test and so is subject to §13.2's refusal: it is snapped **up** to an
/// expressible level, or the cell is `floor_unreachable` ([`crate::calibration::Threshold`]).
pub const fn default_floor_bits(stage: Stage) -> Option<f32> {
    match stage {
        Stage::S0 | Stage::S1 | Stage::S2 | Stage::S3 => Some(6.0),
        Stage::S4 | Stage::S5 => Some(10.0),
        Stage::S6 => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_and_floors_are_the_adr_defaults() {
        let caps: Vec<_> = Stage::ALL.iter().map(|&s| default_cap_bits(s)).collect();
        assert_eq!(
            caps,
            [
                Some(12.0),
                Some(12.0),
                Some(12.0),
                Some(12.0),
                Some(32.0),
                None,
                None
            ]
        );
        let floors: Vec<_> = Stage::ALL.iter().map(|&s| default_floor_bits(s)).collect();
        assert_eq!(
            floors,
            [
                Some(6.0),
                Some(6.0),
                Some(6.0),
                Some(6.0),
                Some(10.0),
                Some(10.0),
                None
            ]
        );
        // §13.2: a stage's cap never sits below the single-metric calibrated ceiling.
        for s in [Stage::S0, Stage::S1, Stage::S2, Stage::S3] {
            assert!(default_cap_bits(s).unwrap() >= CALIBRATED_CLAIM_CAP_BITS);
        }
    }
}
