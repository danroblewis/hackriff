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
use crate::trace::NullControl;

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

/// Where a result's **check** came from, which decides its look-elsewhere `L_check` and whether
/// ADR-0021 §8.2's null control must pass before it may confirm (ADR-0022 §5.1).
///
/// The default is [`CheckOrigin::Searched`]: a root that does not say is treated as the most
/// charged case, never the free one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CheckOrigin {
    /// Open search: the generator was found in the data. `L_check` is the S5 stage's own
    /// look-elsewhere, and the null control must run and pass.
    #[default]
    Searched,
    /// Generator, width, start bit, tail, bit order and class count were all fixed before the data
    /// was seen by a template whose provenance is `builtin` or `user` (ADR-0022 §5.1). No extra
    /// charge beyond the S5 stage's own count (1 hypothesis → 0 bits).
    TemplateFixed,
    /// A template **discovered** by an earlier search (§4.3). It inherits that search's
    /// look-elsewhere as `L_check` — the laundering rule (ADR-0022 §5.1) — and counts as searched.
    /// `None` means the discovering search's cost was not recorded, and such a result can never
    /// confirm: an unknown charge is not a zero charge.
    Discovered {
        /// `discovery_look_elsewhere_bits` recorded at save-as-template time.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        look_elsewhere_bits: Option<f32>,
    },
}

impl CheckOrigin {
    /// Whether the check counts as **searched** for ADR-0022 §6 step 4 (the null control gates it).
    pub const fn searched(self) -> bool {
        !matches!(self, CheckOrigin::TemplateFixed)
    }

    /// The look-elsewhere a previous search spent finding this check, charged on top of the S5
    /// stage's own count. `None` when it was never recorded (a discovered template without it).
    pub fn inherited_bits(self) -> Option<f32> {
        match self {
            CheckOrigin::Searched | CheckOrigin::TemplateFixed => Some(0.0),
            CheckOrigin::Discovered {
                look_elsewhere_bits,
            } => look_elsewhere_bits,
        }
    }
}

/// A result's evidence **on the hold-out window** (ADR-0015 §3.1 step 7, ADR-0022 §2), the only
/// evidence that solves or confirms. Every number the confirm gate reads is here, so a stored row
/// can reconstruct the decision without the job.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HoldoutEvidence {
    /// The whole chain on hold-out, `Σ min(b_j, cap_j) − L_j` — the rank currency. **Not** the
    /// confirm key.
    pub evidence_bits: f32,
    /// ADR-0022 §2.1: `Σ (min(b_j^analytic, cap_j) − L_j)` over stages on the prefix that emitted
    /// analytic-null evidence, minus any inherited `L_check`. The confirm key.
    pub analytic_bits: f32,
    /// ADR-0022 §6 step 2: the S5 analytic bits (`width × differences`) less `L_check`; `None`
    /// when the prefix carried no check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_bits: Option<f32>,
    /// `L_check`: the S5 stage's look-elsewhere plus the inherited charge. `None` when the
    /// inherited charge is unknown ([`CheckOrigin::Discovered`] without it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub l_check: Option<f32>,
    /// The check's width, from the check summary; `None` when the evaluator gave none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_width: Option<u32>,
    /// ADR-0022 §4.2's `differences`: distinct frames valid **without** correction whose payloads
    /// differ beyond a short period (the S5 `check_distinct_valid` support on hold-out).
    pub differences: u32,
    /// Where the check came from.
    pub check_origin: CheckOrigin,
    /// The hold-out ladder.
    pub stages: Vec<StageEvidence>,
    /// ADR-0021 §8.2, recorded whether or not it fired; `None` when the control does not apply
    /// (a template-fixed check, or a profile with K = 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub null_control: Option<NullControl>,
}

/// One decoded frame from the rank-1 prefix's hold-out run (ADR-0015 §5.5 "stored decodes"):
/// what the evaluator's last stage produced, as the attach step stores it through the ordinary
/// decode ingestion. Frames are content: the job drops `content` when the IQ's class forbids it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HoldoutFrame {
    /// Frame time on the capture clock, Unix ns.
    pub t_ns: i64,
    /// The check passed **without** correction. A corrected frame is never CRC-valid evidence
    /// (T-210).
    pub check_valid: bool,
    /// The frame needed FEC correction to pass.
    #[serde(default)]
    pub corrected: bool,
    /// Frame model, e.g. `adsb-df17`; an open search names its structure (`hk-framing`).
    pub frame_model: String,
    /// The identity the frame carries, as `(scheme, value)`, when the template maps one. Open
    /// search leaves it `None`; the attach step gives it the structural scheme.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<(String, String)>,
    /// Metadata fields (always stored).
    #[serde(default)]
    pub metadata: Value,
    /// Content fields, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
}

/// Most hold-out frames the engine keeps for the attach step.
pub const MAX_HOLDOUT_FRAMES: usize = 256;

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
    /// look-elsewhere ([`HoldoutEvidence::analytic_bits`]). `None` when not validated on hold-out.
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
    /// The hold-out evidence, when this result was validated (M-9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holdout: Option<HoldoutEvidence>,
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
