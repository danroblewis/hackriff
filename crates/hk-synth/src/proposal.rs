//! Proposal operators (ADR-0015 §3.2): thin adapters over `hk_estimate::assist` that propose
//! values for huge discrete spaces — sync words, check generators, field maps — instead of
//! enumerating them.
//!
//! **The adapters are M-4's.** This module fixes the operator names a candidate's
//! [`crate::candidate::Domain::Proposal`] refers to. Two rules the adapters inherit:
//!
//! - a suggestion's `score` feeds `prior_bits`, **never** `evidence_bits`: the child's measured
//!   evidence decides (§3.2);
//! - every operator's bounded `assist::Budget` is charged to the job, and every hypothesis it
//!   proposes counts toward that stage's look-elsewhere `L_j` (§1.3).

use serde::{Deserialize, Serialize};

/// A proposal operator (ADR-0015 §3.2's table).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProposalOp {
    /// Wraps `analyze_stream`: `sync_search` fragments (sync word or offset words), period, block
    /// codes.
    #[serde(rename = "assist.sync")]
    Sync,
    /// Wraps `search_codes`: `crc`/`bch` fragments with `evidence_bits`, posterior share and
    /// `ambiguous_with`.
    #[serde(rename = "assist.codes")]
    Codes,
    /// Wraps `suggest_fields`: a draft field map and length-field hints.
    #[serde(rename = "assist.fields")]
    Fields,
}

impl ProposalOp {
    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            ProposalOp::Sync => "assist.sync",
            ProposalOp::Codes => "assist.codes",
            ProposalOp::Fields => "assist.fields",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_names_round_trip() {
        for op in [ProposalOp::Sync, ProposalOp::Codes, ProposalOp::Fields] {
            let v = serde_json::to_value(op).unwrap();
            assert_eq!(v, op.as_str());
            assert_eq!(serde_json::from_value::<ProposalOp>(v).unwrap(), op);
        }
    }
}
