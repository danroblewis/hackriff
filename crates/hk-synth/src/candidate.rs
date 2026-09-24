//! Candidate pipelines (ADR-0015 §1.2): a **recipe prefix** plus typed **free parameters**.
//!
//! "Synthesized pipelines are recipes" holds at every beam node, not only at the end: a
//! candidate's `recipe` is an ordinary `hk_recipe::Recipe` holding the nodes for S0..S_k and one
//! `stage` output on the deepest node, so a result can be started, hot-edited and saved exactly
//! like a hand-written one. [`Candidate::check_prefix`] is that invariant as a function.
//!
//! Structure alternatives live in the skeleton ([`crate::skeleton`]), not in the recipe schema;
//! `choices` records which alternative each stage slot fixed so far. The beam that expands
//! candidates is M-3's.

use std::collections::BTreeMap;

use hk_recipe::{Catalogue, OutputKind, PortRef, Recipe, RecipeError};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::proposal::ProposalOp;
use crate::stage::Stage;

/// Where a free parameter's seed, or a hypothesis, came from (ADR-0015 §1.2; ADR-0021 §2.1
/// carries it on every trace node, so M-1's types carry it from the start).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeedSource {
    /// A blind estimator (C13/C14).
    Estimate,
    /// A template's declared range or value.
    Template,
    /// The M3 classification's family distribution.
    Classification,
    /// A C18 signature match.
    Signature,
    /// A proposal operator (§3.2).
    Proposal,
    /// Open search: no seed.
    Open,
}

/// A search-space scale for a continuous parameter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scale {
    /// Linear steps.
    #[default]
    Linear,
    /// Logarithmic steps (symbol rates, bandwidths).
    Log,
}

/// A continuous range.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FloatDomain {
    /// Lower bound.
    pub lo: f64,
    /// Upper bound.
    pub hi: f64,
    /// Step scale.
    #[serde(default)]
    pub scale: Scale,
    /// Finest meaningful step (relative for `log`, absolute for `linear`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<f64>,
}

/// An integer range.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntDomain {
    /// Lower bound, inclusive.
    pub lo: i64,
    /// Upper bound, inclusive.
    pub hi: i64,
    /// Step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<i64>,
}

/// An enumeration with optional prior weights. Weights order the search; they never score.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnumDomain {
    /// The values (a spec's list, e.g. POCSAG 512/1200/2400 Bd).
    pub values: Vec<Value>,
    /// Prior weight per value, same length as `values`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weights: Option<Vec<f64>>,
}

/// A list of candidate hex words (sync words, polynomials).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HexDomain {
    /// `0x…` strings.
    pub candidates: Vec<String>,
}

/// A free parameter's domain (ADR-0015 §1.2). Huge discrete spaces are never enumerated: they
/// name a [`ProposalOp`] instead.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Domain {
    /// `{"float": {lo, hi, scale, resolution}}`.
    Float(FloatDomain),
    /// `{"int": {lo, hi, resolution}}`.
    Int(IntDomain),
    /// `{"enum": {values, weights}}`.
    Enum(EnumDomain),
    /// `{"hex": {candidates}}`.
    Hex(HexDomain),
    /// `{"proposal": "assist.sync"}`.
    Proposal(ProposalOp),
}

/// One free parameter of a candidate or template.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FreeParam {
    /// ADR-0011 node id and parameter name: `nodes[clock].params.symbol_rate_bd`.
    pub path: String,
    /// Where the search may move it.
    pub domain: Domain,
    /// Starting value, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<Value>,
    /// Where that starting value came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SeedSource>,
}

/// A candidate pipeline: one beam node (ADR-0015 §1.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    /// The skeleton or template this candidate instantiates, as `id@version`
    /// (`generic-fsk-framed@1`, `pocsag@1`).
    pub skeleton: String,
    /// The slot alternative fixed so far, per stage (`{"S1": "fsk", "S3": "nrzi"}`).
    #[serde(default)]
    pub choices: BTreeMap<Stage, String>,
    /// A `hackriff.recipe` document holding nodes for S0..S_k only, with one `stage` output on
    /// the deepest node.
    pub recipe: Recipe,
    /// Parameters still free.
    #[serde(default)]
    pub free: Vec<FreeParam>,
}

/// Why a candidate is not a runnable prefix.
#[derive(Clone, Debug, PartialEq)]
pub enum CandidateError {
    /// `Recipe::validate` refused the prefix.
    Recipe(Vec<RecipeError>),
    /// The prefix must end in exactly one `stage` output reading its last node (or the input, for
    /// a node-less S0 prefix).
    TailNotStageOutput,
}

impl std::fmt::Display for CandidateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CandidateError::Recipe(errs) => {
                write!(f, "recipe prefix invalid:")?;
                for e in errs {
                    write!(f, " {e};")?;
                }
                Ok(())
            }
            CandidateError::TailNotStageOutput => write!(
                f,
                "a candidate prefix ends in exactly one `stage` output on its deepest node"
            ),
        }
    }
}

impl std::error::Error for CandidateError {}

impl Candidate {
    /// The §1.2 invariant: the recipe validates against `catalogue`, and its only output is a
    /// `stage` output on the last node.
    pub fn check_prefix(&self, catalogue: &dyn Catalogue) -> Result<(), CandidateError> {
        self.recipe
            .validate(catalogue)
            .map_err(CandidateError::Recipe)?;
        let [out] = self.recipe.outputs.as_slice() else {
            return Err(CandidateError::TailNotStageOutput);
        };
        if out.kind != OutputKind::Stage {
            return Err(CandidateError::TailNotStageOutput);
        }
        let reads_tail = match (PortRef::parse(&out.from), self.recipe.nodes.last()) {
            (Some(PortRef::Node { node, .. }), Some(last)) => node == last.id,
            (Some(PortRef::Input), None) => true,
            _ => false,
        };
        if reads_tail {
            Ok(())
        } else {
            Err(CandidateError::TailNotStageOutput)
        }
    }

    /// The deepest stage a choice has been fixed for, if any.
    pub fn deepest_choice(&self) -> Option<Stage> {
        self.choices.keys().next_back().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_param_domains_parse_in_the_adr_shape() {
        let rate: FreeParam = serde_json::from_str(
            r#"{ "path": "nodes[clock].params.symbol_rate_bd",
                 "domain": { "float": { "lo": 4700, "hi": 4900, "scale": "log", "resolution": 0.001 } },
                 "seed": 4800, "source": "estimate" }"#,
        )
        .unwrap();
        assert!(matches!(
            rate.domain,
            Domain::Float(FloatDomain {
                scale: Scale::Log,
                ..
            })
        ));
        assert_eq!(rate.source, Some(SeedSource::Estimate));

        let sync: FreeParam = serde_json::from_str(
            r#"{ "path": "nodes[sync].params.sync_word", "domain": { "proposal": "assist.sync" } }"#,
        )
        .unwrap();
        assert_eq!(sync.domain, Domain::Proposal(ProposalOp::Sync));

        let e: Domain =
            serde_json::from_str(r#"{ "enum": { "values": [512, 1200, 2400] } }"#).unwrap();
        assert!(matches!(e, Domain::Enum(EnumDomain { weights: None, .. })));
    }

    #[test]
    fn seed_sources_are_the_six_the_adr_names() {
        let names: Vec<_> = [
            SeedSource::Estimate,
            SeedSource::Template,
            SeedSource::Classification,
            SeedSource::Signature,
            SeedSource::Proposal,
            SeedSource::Open,
        ]
        .iter()
        .map(|s| serde_json::to_value(s).unwrap())
        .collect();
        assert_eq!(
            names,
            [
                "estimate",
                "template",
                "classification",
                "signature",
                "proposal",
                "open"
            ]
        );
    }
}
