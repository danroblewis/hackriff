//! The `TraceSink` (ADR-0021 §2.3, §3): the search trace's producer-side store, with the
//! retention policy applied **on insert** so residency never exceeds the bound.
//!
//! The engine calls [`TraceSink::insert`] at every site that removes a node from the frontier,
//! refuses one or skips one — there is no "prune quietly" path. The sink then keeps the trace
//! within [`TraceBounds`] by moving nodes into `elided` count buckets, so the trace is **complete
//! in counts and lossy in detail, never the reverse**: `Σ node.evaluations + Σ elided.evaluations`
//! is every evaluation the job did (ADR-0021 §1's accounting identity).
//!
//! # What is never dropped (ADR-0021 §2.3)
//!
//! - **Lineage.** Only a *leaf* can be dropped: a node with a retained child, or with a live
//!   child the engine has not finalised yet ([`TraceSink::pin`]), stays. So the root and every
//!   ancestor of any retained node — in particular of every result — is always present, and no
//!   retained node names a dropped parent.
//! - Every **not-tried** node, every parentless node, the **best node per (stage, family)**, the
//!   top [`PROTECTED_TOP_RANK`] nodes by result rank (so `results[]` can always be drawn from the
//!   retained trace), the best two `pruned_floor` per (stage, family) and `evaluated_worse` up to
//!   rank 10.
//!
//! # What is dropped first
//!
//! `memoised`, then `pruned_beam`, then `pruned_bound` (each ascending by evidence), then
//! `pruned_floor` beyond the best two per (stage, family), then `evaluated_worse` beyond rank 10,
//! then — only if the bound still binds — any other unprotected tried leaf, ascending. If every
//! node left is protected the sink stays over the bound rather than drop one, and reports it
//! ([`Trace::over_bound`]); ADR-0021 §2.3 argues that set is small (one per deferred family,
//! one per unsupported structure), and a test holds it to that.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::stage::Stage;
use crate::trace::{Elided, Outcome, OutcomeKind, TraceBounds, TraceNode};

/// How many nodes, by result rank, retention protects: enough that `results[]` (≤ 10, ADR-0015
/// §5.2) never has to be drawn from outside the retained trace.
pub const PROTECTED_TOP_RANK: usize = 10;

/// The trace as served (ADR-0021 §2.3, §4.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trace {
    /// Retained nodes, in creation order (a parent's id is always lower than its child's).
    pub nodes: Vec<TraceNode>,
    /// Count buckets for dropped nodes.
    pub elided: Vec<Elided>,
    /// Whether anything was dropped. A client must show it.
    pub truncated: bool,
    /// Nodes dropped.
    pub nodes_elided: u64,
    /// Serialized size of the retained nodes, bytes.
    pub bytes: u64,
    /// The bound was exceeded because every remaining node is protected (never-dropped rows).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub over_bound: bool,
}

impl Trace {
    /// Evaluations the trace accounts for: retained plus elided (ADR-0021 §1's identity).
    pub fn evaluations(&self) -> u64 {
        self.nodes.iter().map(|n| n.evaluations).sum::<u64>()
            + self.elided.iter().map(|e| e.evaluations).sum::<u64>()
    }
}

/// (stage, family): the unit retention protects the best of.
type FamilyKey<'a> = (Stage, Option<&'a str>);

struct Entry {
    node: TraceNode,
    bytes: u64,
    parent: Option<u32>,
    family: Option<String>,
}

impl Entry {
    fn bits(&self) -> f32 {
        self.node.evidence_bits.unwrap_or(f32::NEG_INFINITY)
    }
}

/// Retention on insert (module docs).
pub struct TraceSink {
    bounds: TraceBounds,
    entries: BTreeMap<u32, Entry>,
    retained_children: HashMap<u32, u32>,
    pins: HashMap<u32, u32>,
    bytes: u64,
    elided: BTreeMap<(Stage, Option<String>, OutcomeKind), Elided>,
    nodes_elided: u64,
    over_bound: bool,
    cost: Duration,
}

impl TraceSink {
    /// An empty sink under `bounds`.
    pub fn new(bounds: TraceBounds) -> Self {
        Self {
            bounds,
            entries: BTreeMap::new(),
            retained_children: HashMap::new(),
            pins: HashMap::new(),
            bytes: 0,
            elided: BTreeMap::new(),
            nodes_elided: 0,
            over_bound: false,
            cost: Duration::ZERO,
        }
    }

    /// A live (not yet finalised) child of `parent` exists: `parent` may not be dropped until
    /// that child is inserted (and then only if the child itself is dropped).
    pub fn pin(&mut self, parent: u32) {
        *self.pins.entry(parent).or_default() += 1;
    }

    /// Finalises node `id` (whose parent is `parent`, and whose committed family is `family`)
    /// and applies retention. Consumes one [`Self::pin`] on `parent`.
    pub fn insert(
        &mut self,
        id: u32,
        parent: Option<u32>,
        family: Option<String>,
        node: TraceNode,
    ) {
        let t = Instant::now();
        let bytes = serde_json::to_vec(&node).map_or(0, |v| v.len() as u64);
        if let Some(p) = parent {
            if let Some(c) = self.pins.get_mut(&p) {
                *c = c.saturating_sub(1);
                if *c == 0 {
                    self.pins.remove(&p);
                }
            }
            *self.retained_children.entry(p).or_default() += 1;
        }
        self.bytes += bytes;
        self.entries.insert(
            id,
            Entry {
                node,
                bytes,
                parent,
                family,
            },
        );
        while self.over() {
            match self.victim() {
                Some(v) => self.drop_node(v),
                None => {
                    self.over_bound = true;
                    break;
                }
            }
        }
        self.cost += t.elapsed();
    }

    /// Whether node `id` is retained.
    pub fn contains(&self, id: u32) -> bool {
        self.entries.contains_key(&id)
    }

    /// Retained nodes.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is retained.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Retained bytes.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Nodes dropped so far.
    pub fn nodes_elided(&self) -> u64 {
        self.nodes_elided
    }

    /// Wall time spent inside [`Self::insert`] (serialising, retention): the trace's measured
    /// cost, ADR-0021 §3.
    pub fn cost(&self) -> Duration {
        self.cost
    }

    /// The finished trace.
    pub fn finish(self) -> Trace {
        Trace {
            truncated: self.nodes_elided > 0,
            nodes_elided: self.nodes_elided,
            bytes: self.bytes,
            over_bound: self.over_bound,
            nodes: self.entries.into_values().map(|e| e.node).collect(),
            elided: self.elided.into_values().collect(),
        }
    }

    fn over(&self) -> bool {
        self.entries.len() > self.bounds.max_trace_nodes as usize
            || self.bytes > u64::from(self.bounds.max_trace_bytes)
    }

    fn protected(&self) -> BTreeSet<u32> {
        let mut keep = BTreeSet::new();
        // Best per (stage, family), whatever the outcome.
        let mut best: BTreeMap<FamilyKey<'_>, (f32, u32)> = BTreeMap::new();
        // Best two pruned_floor per (stage, family).
        let mut floors: BTreeMap<FamilyKey<'_>, Vec<(f32, u32)>> = BTreeMap::new();
        let mut ranked: Vec<(Stage, f32, u32)> = Vec::new();
        for (&id, e) in &self.entries {
            // A memoised hit repeats another node's measurement; the original speaks for it.
            if !e.node.tried || matches!(e.node.outcome, Outcome::Memoised { .. }) {
                continue;
            }
            let key = (e.node.stage, e.family.as_deref());
            let b = e.bits();
            let slot = best.entry(key).or_insert((b, id));
            if b > slot.0 {
                *slot = (b, id);
            }
            match &e.node.outcome {
                Outcome::PrunedFloor { .. } => floors.entry(key).or_default().push((b, id)),
                Outcome::EvaluatedWorse { rank, .. } if *rank <= 10 => {
                    keep.insert(id);
                }
                _ => {}
            }
            ranked.push((e.node.stage, b, id));
        }
        keep.extend(best.values().map(|&(_, id)| id));
        for mut v in floors.into_values() {
            v.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
            keep.extend(v.iter().take(2).map(|&(_, id)| id));
        }
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.total_cmp(&a.1)).then(a.2.cmp(&b.2)));
        keep.extend(ranked.iter().take(PROTECTED_TOP_RANK).map(|r| r.2));
        keep
    }

    fn victim(&self) -> Option<u32> {
        let keep = self.protected();
        self.entries
            .iter()
            .filter(|(id, e)| {
                e.node.tried
                    && e.parent.is_some()
                    && !keep.contains(id)
                    && !self.pins.contains_key(id)
                    && self.retained_children.get(id).copied().unwrap_or(0) == 0
            })
            .map(|(&id, e)| {
                let tier = match e.node.outcome.kind() {
                    OutcomeKind::Memoised => 0,
                    OutcomeKind::PrunedBeam => 1,
                    OutcomeKind::PrunedBound => 2,
                    OutcomeKind::PrunedFloor => 3,
                    OutcomeKind::EvaluatedWorse => 4,
                    _ => 5,
                };
                (tier, e.bits(), id)
            })
            .min_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)).then(b.2.cmp(&a.2)))
            .map(|(_, _, id)| id)
    }

    fn drop_node(&mut self, id: u32) {
        let Some(e) = self.entries.remove(&id) else {
            return;
        };
        self.bytes -= e.bytes;
        if let Some(p) = e.parent
            && let Some(c) = self.retained_children.get_mut(&p)
        {
            *c = c.saturating_sub(1);
            if *c == 0 {
                self.retained_children.remove(&p);
            }
        }
        self.nodes_elided += 1;
        let kind = e.node.outcome.kind();
        let bits = e.bits();
        let bucket = self
            .elided
            .entry((e.node.stage, e.family.clone(), kind))
            .or_insert_with(|| Elided {
                stage: e.node.stage,
                family: e.family.clone(),
                outcome: kind,
                count: 0,
                bits_max: bits,
                bits_min: bits,
                evaluations: 0,
            });
        bucket.count += 1;
        bucket.bits_max = bucket.bits_max.max(bits);
        bucket.bits_min = bucket.bits_min.min(bits);
        bucket.evaluations += e.node.evaluations;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::SeedSource;
    use crate::evidence::MetricId;
    use crate::search::StopReason;
    use crate::trace::{BeamCause, Measured, TraceHypothesis};

    fn node(id: u32, parent: Option<u32>, outcome: Outcome, bits: f32) -> TraceNode {
        let tried = outcome.tried();
        TraceNode {
            id: format!("n{id}"),
            parent: parent.map(|p| format!("n{p}")),
            stage: Stage::S1,
            hypothesis: TraceHypothesis {
                skeleton: "sk@1".into(),
                slot: Stage::S1,
                choice: "fsk".into(),
                family: Some("fsk".into()),
                params: BTreeMap::new(),
                swept: Vec::new(),
            },
            seed_source: SeedSource::Open,
            prior_bits: 0.0,
            measured: tried.then_some(Measured {
                metric: MetricId::Bimodality,
                raw: 0.0,
                n: 1,
                bits,
                quality: 0.0,
                floor_bits: Some(6.0),
                look_elsewhere_bits: 0.0,
            }),
            evidence_bits: tried.then_some(bits),
            tried,
            outcome,
            evaluations: 3,
            cpu_ms: 0,
            summary: String::new(),
        }
    }

    fn beam(rank: u32) -> Outcome {
        Outcome::PrunedBeam {
            rank,
            width: 4,
            cause: BeamCause::Width,
        }
    }

    #[test]
    fn residency_never_exceeds_the_node_bound_and_counts_are_complete() {
        let bounds = TraceBounds {
            max_trace_nodes: 20,
            max_trace_bytes: u32::MAX,
        };
        let mut sink = TraceSink::new(bounds);
        sink.insert(
            0,
            None,
            None,
            node(0, None, Outcome::Survived { children: 200 }, 9.0),
        );
        for _ in 1..=200u32 {
            sink.pin(0);
        }
        for i in 1..=200u32 {
            sink.insert(
                i,
                Some(0),
                Some("fsk".into()),
                node(i, Some(0), beam(i), i as f32 / 10.0),
            );
            assert!(sink.len() <= 20, "bound held on insert {i}");
        }
        let t = sink.finish();
        assert!(t.truncated);
        assert_eq!(t.nodes.len() as u64 + t.nodes_elided, 201);
        assert_eq!(t.evaluations(), 201 * 3);
        // The root is kept; the dropped ones are the weakest.
        assert!(t.nodes.iter().any(|n| n.id == "n0"));
        assert!(t.nodes.iter().any(|n| n.id == "n200"));
        let bucket = &t.elided[0];
        assert_eq!(bucket.outcome, OutcomeKind::PrunedBeam);
        assert!(bucket.bits_min <= 0.2 && bucket.bits_max < 20.0);
    }

    #[test]
    fn not_tried_nodes_and_lineage_are_never_dropped() {
        let bounds = TraceBounds {
            max_trace_nodes: 3,
            max_trace_bytes: u32::MAX,
        };
        let mut sink = TraceSink::new(bounds);
        // n1 (under root n0) has a live child: it is pinned and must survive.
        sink.pin(0);
        sink.pin(1);
        sink.insert(1, Some(0), None, node(1, Some(0), beam(9), 0.1));
        for i in 2..6u32 {
            sink.insert(
                i,
                None,
                None,
                node(
                    i,
                    None,
                    Outcome::DeferredBudget {
                        stop: StopReason::Budget,
                        queue_position: i,
                    },
                    0.0,
                ),
            );
        }
        let t = sink.finish();
        assert!(t.nodes.iter().any(|n| n.id == "n1"));
        assert_eq!(t.nodes.iter().filter(|n| !n.tried).count(), 4);
        assert!(
            t.over_bound,
            "all protected: reported, never silently dropped"
        );
        assert_eq!(t.nodes_elided, 0);
    }

    #[test]
    fn memoised_leaves_go_first() {
        let bounds = TraceBounds {
            max_trace_nodes: 2,
            max_trace_bytes: u32::MAX,
        };
        let mut sink = TraceSink::new(bounds);
        sink.insert(
            0,
            None,
            None,
            node(0, None, Outcome::Survived { children: 2 }, 8.0),
        );
        sink.pin(0);
        sink.pin(0);
        sink.insert(1, Some(0), Some("a".into()), node(1, Some(0), beam(3), 1.0));
        sink.insert(
            2,
            Some(0),
            Some("b".into()),
            node(
                2,
                Some(0),
                Outcome::Memoised {
                    reused: "n9".into(),
                },
                7.0,
            ),
        );
        let t = sink.finish();
        // n1 is the best of family "a" and protected; the memoised leaf is dropped instead.
        assert!(t.nodes.iter().all(|n| n.id != "n2"));
        assert_eq!(t.elided[0].outcome, OutcomeKind::Memoised);
    }
}
