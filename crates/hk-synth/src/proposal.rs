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

// ---------------------------------------------------------------------------------------------
// The adapters (T-855, MAUTO M-4)
// ---------------------------------------------------------------------------------------------

use std::time::{Duration, Instant};

use hk_estimate::assist::{
    self, BlockFragment, Budget, CodeSearchConfig, FieldsConfig, SyncConfig, WorkReport,
};
use hk_recipe::FieldMap;

/// The job's proposal budget. **Counted in operations** (`assist::Budget::max_ops`), because
/// T-552 (docs/27 §3) measured proposal calls at 0.3–2.4 s each, dominating wall time, at a
/// near-constant 1.2–1.6 ns/op; wall time is only a backstop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProposalBudget {
    /// Total assist operations across every proposal call of the job.
    pub max_ops: u64,
    /// Cap on operations for any single call (clamped to what remains).
    pub per_call_ops: u64,
    /// Wall-clock backstop, seconds; `None` = unbounded.
    pub wall_s: Option<f64>,
}

impl Default for ProposalBudget {
    fn default() -> Self {
        Self {
            max_ops: 4 * assist::DEFAULT_MAX_OPS,
            per_call_ops: assist::DEFAULT_MAX_OPS,
            wall_s: None,
        }
    }
}

/// Why a proposal call was refused without running.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Refused {
    /// The operation budget is spent.
    OpsExhausted,
    /// The wall-clock backstop passed.
    WallExhausted,
}

/// What the job has spent on proposals so far; shared by every operator call.
#[derive(Debug)]
pub struct ProposalLedger {
    budget: ProposalBudget,
    started: Instant,
    /// Operations charged.
    pub ops_spent: u64,
    /// Calls that ran.
    pub calls: u64,
    /// Hypotheses the operators evaluated: the stage's look-elsewhere `L_j` input (§1.3).
    pub hypotheses: u64,
    /// Calls that stopped at their cap.
    pub partial_calls: u64,
}

impl ProposalLedger {
    /// A fresh ledger; the wall clock starts now.
    pub fn new(budget: ProposalBudget) -> Self {
        Self {
            budget,
            started: Instant::now(),
            ops_spent: 0,
            calls: 0,
            hypotheses: 0,
            partial_calls: 0,
        }
    }

    /// Operations still available.
    pub fn remaining_ops(&self) -> u64 {
        self.budget.max_ops.saturating_sub(self.ops_spent)
    }

    /// The `assist::Budget` for the next call, or why there is none.
    fn grant(&self) -> Result<Budget, Refused> {
        if let Some(w) = self.budget.wall_s
            && self.started.elapsed() >= Duration::from_secs_f64(w.max(0.0))
        {
            return Err(Refused::WallExhausted);
        }
        let ops = self.remaining_ops().min(self.budget.per_call_ops);
        if ops == 0 {
            return Err(Refused::OpsExhausted);
        }
        Ok(Budget { max_ops: ops })
    }

    fn charge(&mut self, w: &WorkReport) {
        self.ops_spent = self.ops_spent.saturating_add(w.ops);
        self.calls += 1;
        self.hypotheses += w.hypotheses;
        if w.partial {
            self.partial_calls += 1;
        }
    }
}

/// What a proposal carries.
#[derive(Clone, Debug, PartialEq)]
pub enum Payload {
    /// A block fragment (`sync_search`, `crc`, `bch`, …) that becomes a child node's parameters.
    Fragment(BlockFragment),
    /// A draft field map with the length-field hints found (`(name, scale, add)`).
    Fields {
        /// The draft map.
        field_map: FieldMap,
        /// Length fields: frame bits = value × scale + add.
        length_hints: Vec<(String, u32, i64)>,
    },
}

/// One proposed child (§3.2). `score` feeds `prior_bits` only; **`evidence_bits` is never set
/// here** — the child's measured evidence decides.
#[derive(Clone, Debug, PartialEq)]
pub struct Proposal {
    /// Which operator proposed it.
    pub op: ProposalOp,
    /// The child's parameters.
    pub payload: Payload,
    /// The assist suggestion's 0–1 score.
    pub score: f64,
    /// `log₂` of the score, clipped ([`crate::evidence::prior_bits`]).
    pub prior_bits: f32,
    /// Other generators the data cannot tell apart from this one (codes only).
    pub ambiguous_with: Vec<u64>,
}

/// A call's outcome: proposals plus the work it was charged.
#[derive(Clone, Debug, PartialEq)]
pub struct Proposed {
    /// Proposals, best first.
    pub proposals: Vec<Proposal>,
    /// Work charged to the job for this call.
    pub work: WorkReport,
}

fn prop(op: ProposalOp, payload: Payload, score: f64, ambiguous_with: Vec<u64>) -> Proposal {
    Proposal {
        op,
        payload,
        score,
        prior_bits: crate::evidence::prior_bits(score),
        ambiguous_with,
    }
}

/// `assist.sync`: sync words, and a block-code fragment when the stream is a linear block code.
pub fn propose_sync(
    ledger: &mut ProposalLedger,
    bits: &[u8],
    cfg: &SyncConfig,
) -> Result<Proposed, Refused> {
    let mut cfg = cfg.clone();
    cfg.budget = ledger.grant()?;
    let r = assist::analyze_stream(bits, &cfg);
    ledger.charge(&r.work);
    let mut proposals: Vec<Proposal> = r
        .syncs
        .iter()
        .map(|s| {
            prop(
                ProposalOp::Sync,
                Payload::Fragment(s.fragment.clone()),
                s.score,
                vec![],
            )
        })
        .collect();
    if let Some(f) = &r.offset_words {
        let score = r.block_codes.first().map_or(0.0, |c| c.score);
        proposals.push(prop(
            ProposalOp::Sync,
            Payload::Fragment(f.clone()),
            score,
            vec![],
        ));
    }
    proposals.extend(r.block_codes.iter().map(|c| {
        prop(
            ProposalOp::Sync,
            Payload::Fragment(c.fragment.clone()),
            c.score,
            c.ambiguous_with.clone(),
        )
    }));
    Ok(Proposed {
        proposals,
        work: r.work,
    })
}

/// `assist.codes`: `crc`/`bch` fragments over aligned frames.
pub fn propose_codes(
    ledger: &mut ProposalLedger,
    frames: &[Vec<u8>],
    cfg: &CodeSearchConfig,
) -> Result<Proposed, Refused> {
    let mut cfg = cfg.clone();
    cfg.budget = ledger.grant()?;
    let r = assist::search_codes(frames, &cfg);
    ledger.charge(&r.work);
    let proposals = r
        .codes
        .iter()
        .map(|c| {
            prop(
                ProposalOp::Codes,
                Payload::Fragment(c.fragment.clone()),
                c.score,
                c.ambiguous_with.clone(),
            )
        })
        .collect();
    Ok(Proposed {
        proposals,
        work: r.work,
    })
}

/// `assist.fields`: a draft field map with length-field hints.
pub fn propose_fields(
    ledger: &mut ProposalLedger,
    frames: &[Vec<u8>],
    cfg: &FieldsConfig,
) -> Result<Proposed, Refused> {
    let mut cfg = cfg.clone();
    cfg.budget = ledger.grant()?;
    let r = assist::suggest_fields(frames, &cfg);
    ledger.charge(&r.work);
    let length_hints: Vec<(String, u32, i64)> = r
        .suggestions
        .iter()
        .filter_map(|s| Some((s.name.clone(), s.length_scale?, s.length_add.unwrap_or(0))))
        .collect();
    let score = if r.suggestions.is_empty() {
        0.0
    } else {
        r.suggestions.iter().map(|s| s.score).sum::<f64>() / r.suggestions.len() as f64
    };
    let proposals = vec![prop(
        ProposalOp::Fields,
        Payload::Fields {
            field_map: r.field_map,
            length_hints,
        },
        score,
        vec![],
    )];
    Ok(Proposed {
        proposals,
        work: r.work,
    })
}

#[cfg(test)]
mod adapter_tests {
    use super::*;

    /// 200 frames of 16 bits: 8 varied bits then their 8-bit parity-ish complement is not needed;
    /// a constant 0xA5 header and a counter give the field search something to find.
    fn frames() -> Vec<Vec<u8>> {
        (0..200u32)
            .map(|i| {
                let mut f = Vec::new();
                for b in (0..8).rev() {
                    f.push(((0xA5u32 >> b) & 1) as u8);
                }
                for b in (0..8).rev() {
                    f.push(((i >> b) & 1) as u8);
                }
                f
            })
            .collect()
    }

    #[test]
    fn calls_are_charged_and_budget_is_by_ops() {
        let mut l = ProposalLedger::new(ProposalBudget {
            max_ops: 1_000_000,
            per_call_ops: 400_000,
            wall_s: None,
        });
        let p = propose_fields(&mut l, &frames(), &FieldsConfig::default()).unwrap();
        assert!(p.work.max_ops <= 400_000, "per-call cap applied");
        assert_eq!(l.calls, 1);
        assert_eq!(l.ops_spent, p.work.ops);
        assert!(l.hypotheses >= p.work.hypotheses);
        assert_eq!(p.proposals[0].op, ProposalOp::Fields);
        assert!(p.proposals[0].prior_bits <= 0.0);
    }

    #[test]
    fn exhausted_ops_refuse_without_running() {
        let mut l = ProposalLedger::new(ProposalBudget {
            max_ops: 10,
            per_call_ops: 10,
            wall_s: None,
        });
        l.ops_spent = 10;
        assert_eq!(
            propose_codes(&mut l, &frames(), &CodeSearchConfig::default()).unwrap_err(),
            Refused::OpsExhausted
        );
        assert_eq!(l.calls, 0);
    }

    #[test]
    fn wall_is_a_backstop() {
        let mut l = ProposalLedger::new(ProposalBudget {
            wall_s: Some(0.0),
            ..Default::default()
        });
        assert_eq!(
            propose_sync(&mut l, &[0, 1, 0, 1], &SyncConfig::default()).unwrap_err(),
            Refused::WallExhausted
        );
    }

    #[test]
    fn tiny_cap_reports_partial() {
        let mut l = ProposalLedger::new(ProposalBudget {
            max_ops: 50,
            per_call_ops: 50,
            wall_s: None,
        });
        let p = propose_codes(&mut l, &frames(), &CodeSearchConfig::default()).unwrap();
        assert!(p.work.partial);
        assert_eq!(l.partial_calls, 1);
    }

    #[test]
    fn sync_charges_ledger() {
        let mut bits = Vec::new();
        for i in 0..40u32 {
            bits.extend_from_slice(&[1, 1, 1, 0, 1, 0, 0, 1, 1, 0, 1, 1, 0, 0, 0, 1]);
            for b in 0..24 {
                bits.push(((i.wrapping_mul(2654435761) >> b) & 1) as u8);
            }
        }
        let mut l = ProposalLedger::new(ProposalBudget::default());
        let p = propose_sync(&mut l, &bits, &SyncConfig::default()).unwrap();
        assert_eq!(l.ops_spent, p.work.ops);
        assert!(p.proposals.iter().all(|x| x.op == ProposalOp::Sync));
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
