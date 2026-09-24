//! Ranked results and the verdict ladder (ADR-0015 §3.4; ADR-0021 §7A.5; ADR-0022 §2).
//!
//! There are always ranked partial results: a search that never solves still says how deep it
//! got. The ladder says **how deep**; what that means when nothing solved is the
//! [`crate::trace::Resolution`] (ADR-0021 §7A.1 — `unknown` is not a sixth verdict).

use hk_recipe::Recipe;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::evidence::{Evidence, MetricId};
use crate::stage::Stage;

/// How deep a result got (ADR-0015 §3.4). Ordered shallowest first, so `Solved` is the maximum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// S0 only.
    Energy,
    /// S1.
    Demodulated,
    /// S2–S3.
    Clocked,
    /// S4.
    Framed,
    /// S5 partial: "reached sync, 40 % CRC".
    Checked,
    /// S5 on hold-out meets the solve rule.
    Solved,
}

impl Verdict {
    /// The verdict a prefix reaching `stage` earns **without** the solve rule. `Solved` is never
    /// produced here: it needs hold-out evidence (ADR-0015 §3.1 step 7, §5.5 / ADR-0022).
    pub const fn unsolved_at(stage: Stage) -> Verdict {
        match stage {
            Stage::S0 => Verdict::Energy,
            Stage::S1 => Verdict::Demodulated,
            Stage::S2 | Stage::S3 => Verdict::Clocked,
            Stage::S4 => Verdict::Framed,
            Stage::S5 | Stage::S6 => Verdict::Checked,
        }
    }
}

/// One rung of a result's evidence ladder, as served (`stages: [{stage, node, metric, raw, n,
/// bits, quality}]`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageEvidence {
    /// Stage.
    pub stage: Stage,
    /// Recipe node id that produced it.
    pub node: String,
    /// Metric.
    pub metric: MetricId,
    /// Natural-unit value.
    pub raw: f32,
    /// Support.
    pub n: u32,
    /// Significance, bits.
    pub bits: f32,
    /// Display quality.
    pub quality: f32,
}

impl StageEvidence {
    /// A ladder rung from a block's evidence record and the node that emitted it.
    pub fn from_evidence(node: impl Into<String>, e: &Evidence) -> Self {
        Self {
            stage: e.stage,
            node: node.into(),
            metric: e.metric,
            raw: e.raw,
            n: e.n,
            bits: e.bits,
            quality: e.quality,
        }
    }
}

/// The check summary (§3.4 `check`, with §11.5's T-210 rule).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckSummary {
    /// `crc`, `bch`, `parity`, `checksum`.
    pub kind: String,
    /// RevEng model or BCH code (`CRC-16/IBM`), or `searched`.
    pub model: String,
    /// Check width, bits.
    pub width: u32,
    /// Share of frames passing.
    pub pass_rate: f32,
    /// Distinct frames valid **without** FEC correction. Corrected frames never count (T-210).
    pub distinct_valid: u32,
    /// Frames valid only after correction: shown, worth 0 bits.
    #[serde(default)]
    pub corrected_excluded: u32,
    /// Frames tested.
    pub tested: u32,
    /// Whether these counts are from the hold-out window.
    pub holdout: bool,
}

/// A template reference on a result.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateRef {
    /// Template id.
    pub id: String,
    /// Template version.
    pub version: u32,
}

/// Most inspector frame records a result previews (§3.4).
pub const FRAMES_PREVIEW_MAX: usize = 8;

/// One ranked result (ADR-0015 §3.4).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineResult {
    /// 1-based rank by `(stage_reached, evidence_bits)`.
    pub rank: u32,
    /// How deep.
    pub verdict: Verdict,
    /// Backend-rendered text; the UI does no wording logic.
    pub summary: String,
    /// The concrete recipe, every free parameter bound. Startable as-is.
    pub recipe: Recipe,
    /// The template it came from, or `None` for open search.
    pub template: Option<TemplateRef>,
    /// Deepest stage.
    pub stage_reached: Stage,
    /// The evidence ladder.
    pub stages: Vec<StageEvidence>,
    /// Search/rank key, after look-elsewhere. **Not** the confirm key.
    pub evidence_bits: f32,
    /// Reported only; never ranks.
    pub prior_bits: f32,
    /// The confirm key (ADR-0022 §2): analytic-null hold-out bits, each net of its own stage's
    /// look-elsewhere. `None` until computed on hold-out (M-9 / T-575).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analytic_holdout_bits: Option<f32>,
    /// The check, when S5 was reached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<CheckSummary>,
    /// Up to [`FRAMES_PREVIEW_MAX`] inspector frame records, gated by the output policy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub frames_preview: Vec<Value>,
    /// ADR-0021 §7A.5's characterisation, for a `structured-unidentified` rank-1 result (M-9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub characterisation: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ladder_orders_and_never_solves_without_hold_out() {
        assert!(Verdict::Energy < Verdict::Demodulated);
        assert!(Verdict::Checked < Verdict::Solved);
        for s in Stage::ALL {
            assert_ne!(Verdict::unsolved_at(s), Verdict::Solved);
        }
        assert_eq!(Verdict::unsolved_at(Stage::S3), Verdict::Clocked);
        assert_eq!(Verdict::unsolved_at(Stage::S4), Verdict::Framed);
        assert_eq!(
            serde_json::to_value(Verdict::Demodulated).unwrap(),
            "demodulated"
        );
    }
}
