//! Hot-edit planning (ADR-0011 §2.3): which nodes of a running pipeline keep their state when
//! the recipe changes. Pure data; the runtime (T-088) builds the new graph off the real-time
//! thread and swaps it in at a chunk boundary.

use std::collections::{BTreeMap, BTreeSet};

use crate::recipe::Recipe;

/// What happens to one node across an edit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeChange {
    /// Same block, version, inputs and params: the running instance moves over untouched
    /// (unless an upstream change resets it, [`EditPlan::reset`]).
    Unchanged,
    /// Only params changed. If every changed key is `hot` in the block's schema the instance
    /// gets `update_params` at the swap; otherwise it is rebuilt.
    Params {
        /// Changed (added, removed or modified) keys.
        keys: Vec<String>,
        /// Every changed key is hot.
        hot: bool,
    },
    /// Block kind, version or inputs changed: a fresh instance is built and initialised.
    Rebuilt,
    /// New node.
    Added,
    /// Node no longer in the recipe: dropped at the swap.
    Removed,
}

/// The plan for moving a running pipeline from one recipe to another.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditPlan {
    /// Change per node id (old and new ids).
    pub nodes: BTreeMap<String, NodeChange>,
    /// Surviving nodes that are downstream of a rebuilt, added or non-hot node: they keep
    /// their instance but get `reset()` and a `RESET` chunk flag at the swap.
    pub reset: BTreeSet<String>,
    /// Field maps added, removed or changed. Nodes referencing them swap maps at the next
    /// frame boundary (a field-map param is always hot); no DSP state is touched.
    pub field_maps_changed: BTreeSet<String>,
    /// `input` changed: the channel is re-plumbed and every node is rebuilt.
    pub input_changed: bool,
    /// Outputs changed: streams for removed outputs finish; new ones are offered.
    pub outputs_changed: bool,
}

impl EditPlan {
    /// Plans `old` → `new`. `hot(block, param)` says whether a parameter is hot (from the block
    /// descriptors).
    pub fn between(old: &Recipe, new: &Recipe, hot: &dyn Fn(&str, &str) -> bool) -> EditPlan {
        let input_changed = old.input != new.input;
        let sources = |r: &Recipe| -> BTreeMap<String, BTreeMap<String, String>> {
            r.nodes
                .iter()
                .enumerate()
                .map(|(i, n)| {
                    let inputs = if n.inputs.is_empty() {
                        let prev = if i == 0 {
                            "input".to_owned()
                        } else {
                            r.nodes[i - 1].id.clone()
                        };
                        BTreeMap::from([(String::new(), prev)])
                    } else {
                        n.inputs.clone()
                    };
                    (n.id.clone(), inputs)
                })
                .collect()
        };
        let (old_src, new_src) = (sources(old), sources(new));
        let old_nodes: BTreeMap<_, _> = old.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        let mut nodes = BTreeMap::new();
        // Nodes whose outputs restart: their dependants reset.
        let mut restarted = BTreeSet::new();
        for n in &new.nodes {
            let change = match old_nodes.get(n.id.as_str()) {
                None => NodeChange::Added,
                Some(_) if input_changed => NodeChange::Rebuilt,
                Some(o) if o.block != n.block || o.version != n.version => NodeChange::Rebuilt,
                Some(_) if old_src[&n.id] != new_src[&n.id] => NodeChange::Rebuilt,
                Some(o) if o.params != n.params => {
                    let keys: BTreeSet<&String> = o.params.keys().chain(n.params.keys()).collect();
                    let keys: Vec<String> = keys
                        .into_iter()
                        .filter(|k| o.params.get(*k) != n.params.get(*k))
                        .cloned()
                        .collect();
                    let all_hot = keys.iter().all(|k| hot(&n.block, k));
                    NodeChange::Params { keys, hot: all_hot }
                }
                Some(_) => NodeChange::Unchanged,
            };
            if matches!(
                change,
                NodeChange::Added | NodeChange::Rebuilt | NodeChange::Params { hot: false, .. }
            ) {
                restarted.insert(n.id.clone());
            }
            nodes.insert(n.id.clone(), change);
        }
        for o in &old.nodes {
            nodes.entry(o.id.clone()).or_insert(NodeChange::Removed);
        }
        // Propagate resets downstream in the new graph (document order is not topological in
        // general, so iterate to a fixed point; graphs are small).
        let mut reset = BTreeSet::new();
        loop {
            let mut grew = false;
            for n in &new.nodes {
                if restarted.contains(&n.id) || reset.contains(&n.id) {
                    continue;
                }
                let upstream = new_src[&n.id].values().any(|r| {
                    let node = r.split('.').next().unwrap_or(r);
                    restarted.contains(node) || reset.contains(node)
                });
                if upstream {
                    reset.insert(n.id.clone());
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
        let map_ids: BTreeSet<&String> =
            old.field_maps.keys().chain(new.field_maps.keys()).collect();
        let field_maps_changed = map_ids
            .into_iter()
            .filter(|id| old.field_maps.get(*id) != new.field_maps.get(*id))
            .cloned()
            .collect();
        EditPlan {
            nodes,
            reset,
            field_maps_changed,
            input_changed,
            outputs_changed: old.outputs != new.outputs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn recipe(nodes: serde_json::Value) -> Recipe {
        serde_json::from_value(json!({
            "schema": "hackriff.recipe", "schema_version": 1, "id": "t", "version": 1,
            "name": "T", "input": {"port": "bits"}, "nodes": nodes,
            "outputs": [{"id": "s", "kind": "stage", "from": "c"}],
            "output_policy": {"content_class": "unrestricted"}
        }))
        .unwrap()
    }

    #[test]
    fn hot_params_keep_state_and_cold_ones_reset_downstream() {
        let old = recipe(json!([
            {"id": "a", "block": "slicer", "params": {"threshold": 0.0}},
            {"id": "b", "block": "diff_decode"},
            {"id": "c", "block": "identity"}
        ]));
        let hot = |block: &str, key: &str| block == "slicer" && key == "threshold";

        let mut new = old.clone();
        new.nodes[0].params.insert("threshold".into(), json!(0.1));
        let plan = EditPlan::between(&old, &new, &hot);
        assert_eq!(
            plan.nodes["a"],
            NodeChange::Params {
                keys: vec!["threshold".into()],
                hot: true
            }
        );
        assert!(plan.reset.is_empty());

        let mut cold = old.clone();
        cold.nodes[0].params.insert("invert".into(), json!(true));
        let plan = EditPlan::between(&old, &cold, &hot);
        assert!(matches!(
            plan.nodes["a"],
            NodeChange::Params { hot: false, .. }
        ));
        assert_eq!(plan.reset, BTreeSet::from(["b".into(), "c".into()]));

        let mut rewired = old.clone();
        rewired.nodes.remove(1);
        let plan = EditPlan::between(&old, &rewired, &hot);
        assert_eq!(plan.nodes["b"], NodeChange::Removed);
        assert_eq!(plan.nodes["c"], NodeChange::Rebuilt);
        assert_eq!(plan.nodes["a"], NodeChange::Unchanged);
    }
}
