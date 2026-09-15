//! Hot-edit planning (ADR-0011 §2.3): which nodes of a running pipeline keep their state when
//! the recipe changes. Pure data; the runtime (T-088) builds the new graph off the real-time
//! thread and swaps it in at a chunk boundary.

use std::collections::{BTreeMap, BTreeSet};

use crate::param::{Catalogue, ParamType};
use crate::recipe::Recipe;

/// What happens to one node across an edit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeChange {
    /// Same block, version, inputs and params: the running instance moves over untouched
    /// (unless an upstream change resets it, [`EditPlan::reset`]).
    Unchanged,
    /// Only params changed, or a field map a `field-map` param names changed content (that
    /// param's key is listed; field-map params are hot). If every changed key is `hot` in the
    /// block's schema the instance gets `update_params` (with the new `BuildCtx` field maps)
    /// at the swap; otherwise it is rebuilt.
    Params {
        /// Changed (added, removed or modified) keys, plus field-map params whose map changed.
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
    /// Field maps added, removed or changed. Nodes referencing them are `Params` changes on
    /// their field-map param (hot: applied in place at the swap, no reset downstream).
    pub field_maps_changed: BTreeSet<String>,
    /// `input` changed: the channel is re-plumbed and every node is rebuilt.
    pub input_changed: bool,
    /// Outputs changed: streams for removed outputs finish; new ones are offered.
    pub outputs_changed: bool,
}

impl EditPlan {
    /// Plans `old` → `new` against the block descriptors in `catalogue` (which params are hot,
    /// which name field maps). A key the catalogue doesn't know is cold.
    pub fn between(old: &Recipe, new: &Recipe, catalogue: &dyn Catalogue) -> EditPlan {
        let input_changed = old.input != new.input;
        let schema = |block: &str, key: &str| {
            catalogue
                .descriptor(block)
                .and_then(|d| d.params.iter().find(|p| p.name == key))
        };
        let hot = |block: &str, key: &str| schema(block, key).is_some_and(|p| p.hot);
        let map_ids: BTreeSet<&String> =
            old.field_maps.keys().chain(new.field_maps.keys()).collect();
        let field_maps_changed: BTreeSet<String> = map_ids
            .into_iter()
            .filter(|id| old.field_maps.get(*id) != new.field_maps.get(*id))
            .cloned()
            .collect();
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
                Some(o) => {
                    let keys: BTreeSet<&String> = o.params.keys().chain(n.params.keys()).collect();
                    let keys: Vec<String> = keys
                        .into_iter()
                        .filter(|k| {
                            o.params.get(*k) != n.params.get(*k)
                                || (schema(&n.block, k)
                                    .is_some_and(|p| p.ty == ParamType::FieldMap)
                                    && n.params
                                        .get(*k)
                                        .and_then(|v| v.as_str())
                                        .is_some_and(|id| field_maps_changed.contains(id)))
                        })
                        .cloned()
                        .collect();
                    if keys.is_empty() {
                        NodeChange::Unchanged
                    } else {
                        let all_hot = keys.iter().all(|k| hot(&n.block, k));
                        NodeChange::Params { keys, hot: all_hot }
                    }
                }
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
    use crate::param::{BlockDescriptor, ParamSchema};
    use serde_json::json;

    fn recipe(nodes: serde_json::Value) -> Recipe {
        serde_json::from_value(json!({
            "schema": "hackriff.recipe", "schema_version": 2, "id": "t", "version": 1,
            "name": "T", "input": {"port": "bits"}, "nodes": nodes,
            "outputs": [{"id": "s", "kind": "stage", "from": "c"}],
            "output_policy": {"content_class": "unrestricted"}
        }))
        .unwrap()
    }

    /// Descriptors with just the params these tests use (ports are irrelevant to planning).
    fn catalogue() -> Vec<BlockDescriptor> {
        let param = |name: &str, ty: ParamType, hot: bool| ParamSchema {
            name: name.into(),
            ty,
            required: false,
            default: None,
            hot,
            doc: String::new(),
        };
        let block = |name: &str, params: Vec<ParamSchema>| BlockDescriptor {
            name: name.into(),
            version: 1,
            group: "t".into(),
            doc: String::new(),
            inputs: vec![],
            outputs: vec![],
            params,
            params_pinned: true,
        };
        vec![
            block(
                "slicer",
                vec![
                    param(
                        "threshold",
                        ParamType::Float {
                            min: None,
                            max: None,
                            unit: None,
                        },
                        true,
                    ),
                    param("invert", ParamType::Bool, false),
                ],
            ),
            block("fields", vec![param("map", ParamType::FieldMap, true)]),
        ]
    }

    #[test]
    fn hot_params_keep_state_and_cold_ones_reset_downstream() {
        let old = recipe(json!([
            {"id": "a", "block": "slicer", "params": {"threshold": 0.0}},
            {"id": "b", "block": "diff_decode"},
            {"id": "c", "block": "identity"}
        ]));
        let hot = &catalogue();

        let mut new = old.clone();
        new.nodes[0].params.insert("threshold".into(), json!(0.1));
        let plan = EditPlan::between(&old, &new, hot);
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
        let plan = EditPlan::between(&old, &cold, hot);
        assert!(matches!(
            plan.nodes["a"],
            NodeChange::Params { hot: false, .. }
        ));
        assert_eq!(plan.reset, BTreeSet::from(["b".into(), "c".into()]));

        let mut rewired = old.clone();
        rewired.nodes.remove(1);
        let plan = EditPlan::between(&old, &rewired, hot);
        assert_eq!(plan.nodes["b"], NodeChange::Removed);
        assert_eq!(plan.nodes["c"], NodeChange::Rebuilt);
        assert_eq!(plan.nodes["a"], NodeChange::Unchanged);
    }

    #[test]
    fn a_field_map_content_change_is_a_hot_params_change_on_its_users() {
        let mut old = recipe(json!([
            {"id": "a", "block": "slicer"},
            {"id": "f", "block": "fields", "params": {"map": "m"}},
            {"id": "c", "block": "text"}
        ]));
        old.field_maps.insert(
            "m".into(),
            serde_json::from_value(json!({"unit": "bits", "fields": [
                {"name": "x", "type": "uint", "length": 4}
            ]}))
            .unwrap(),
        );
        let cat = catalogue();
        let mut new = old.clone();
        new.field_maps.get_mut("m").unwrap().fields[0].label = Some("X".into());
        let plan = EditPlan::between(&old, &new, &cat);
        assert_eq!(
            plan.nodes["f"],
            NodeChange::Params {
                keys: vec!["map".into()],
                hot: true
            }
        );
        assert_eq!(plan.nodes["a"], NodeChange::Unchanged);
        assert_eq!(plan.nodes["c"], NodeChange::Unchanged);
        assert!(plan.reset.is_empty(), "text and downstream keep state");
        assert_eq!(plan.field_maps_changed, BTreeSet::from(["m".into()]));

        // An unchanged map leaves its user untouched.
        let plan = EditPlan::between(&old, &old.clone(), &cat);
        assert!(plan.nodes.values().all(|c| *c == NodeChange::Unchanged));
    }
}
