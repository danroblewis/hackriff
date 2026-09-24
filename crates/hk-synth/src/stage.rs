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
///
/// **T-660's §13.5 falsifier-2 measurement, reported (not silently tuned) here.** No hk-synth
/// engine and no docs/22 acceptance corpus exist yet to run the search itself (docs/22 §9/§11),
/// so this is an analytical worked example from docs/21 §2's *already-measured* per-metric
/// realised bits at a 6-bit claim (worst over Null B1/B2 — wrong symbol rate / wrong centre —
/// `realised = 6 − δ`), combined by §13.1's rule with the M1 FSK ladder's declared groups:
///
/// | Stage | Group(s) | `b_j` under the old sum | `b_j` under §13.1's max | Clears `floor_j = 6`? |
/// |---|---|---|---|---|
/// | S2 | `soft_quality{snr,evm,timing_var}` + `eye{eye_open}` (admissible-capped to 3.0, §13.2) | 17.1 | 8.2 | **yes** |
/// | S3 | `bit_shape{line_violations,bit_structure}` — the **only** declared S3 group | 8.8 | 4.5 | **no** |
///
/// **S2 keeps `floor_j = 6` at these levels; S3 does not** — S3 has only one declared group, so
/// its `b_j` is capped at whichever single metric realises more, and neither S3 metric clears 6
/// bits alone at this realistic (not best-case) level. This is exactly §13.1's own prediction
/// ("S2/S3 lose roughly 6–12 bits of headroom per stage... some true-but-weak signals will prune
/// where they previously survived"), now with the arithmetic against docs/21's numbers. Per
/// §13.5 item 2's rule ("the answer is a restated floor... never a return to the sum"): **a
/// future block-version bump to `hk-synth`'s stage-cap table should consider lowering S3's floor
/// to roughly 4–5 bits, restated against this measurement**, not this ticket's job to decide —
/// this is the falsifier's report, not the amendment. §1.3's "pruning is never total" means an
/// S3 node that misses the floor still survives as a partial result, so the cost is search
/// recall, not silent loss (§13.1's own §1's cost list, item 1).
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
