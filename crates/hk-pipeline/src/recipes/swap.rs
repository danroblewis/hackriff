//! Hot-edit swap at a chunk boundary (ADR-0011 §2.3 step 3, T-088). Runs **on the pipeline
//! thread** between two chunks: moves the running instances the plan keeps into the staged graph,
//! applies hot parameters in place, resets what is downstream of a restarted node and exchanges
//! the graphs. O(nodes), no allocation (every vector was pre-sized by
//! [`crate::recipes::graph::stage`]); the retired instances stay in the [`Staged`] value, which the
//! caller hands back to the control thread to drop. The ring reader's cursor is not touched, so no
//! sample is lost.

use hk_blocks::{BuildCtx, ChunkFlags, ParamUpdate};

use crate::recipes::graph::{Graph, Src, Staged, StagedNode};

/// What a swap did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SwapReport {
    /// Fresh instances swapped in (added, rebuilt, or a hot update that answered `Rebuild`).
    pub rebuilt: u32,
    /// Kept instances reset because something upstream restarted.
    pub reset: u32,
    /// Kept instances whose hot parameters were applied in place.
    pub updated: u32,
    /// Kept instances untouched.
    pub kept: u32,
}

/// Swaps `staged` into `graph`. Refuses (leaving `graph` untouched) when the staged plan names a
/// running position that does not exist (a stale plan).
pub fn apply(graph: &mut Graph, staged: &mut Staged) -> Result<SwapReport, &'static str> {
    let running = graph.nodes.len();
    let mut seen_fresh = true;
    for n in &staged.nodes {
        match n {
            StagedNode::Keep { old_pos, .. } if *old_pos >= running => {
                return Err("the edit was planned against another revision");
            }
            StagedNode::Fresh(None) => seen_fresh = false,
            _ => {}
        }
    }
    if !seen_fresh {
        return Err("the edit was already applied");
    }
    let Staged {
        recipe,
        nodes,
        outputs,
        old_slots,
        new_nodes,
        restarted,
        ..
    } = staged;
    old_slots.clear();
    for n in graph.nodes.drain(..) {
        old_slots.push(Some(n));
    }
    new_nodes.clear();
    let mut report = SwapReport::default();
    for (pos, sn) in nodes.iter_mut().enumerate() {
        let (mut node, mut fresh) = match sn {
            StagedNode::Fresh(n) => (n.take().expect("checked above"), true),
            StagedNode::Keep {
                old_pos,
                sources,
                input_types,
                out_names,
                update,
                fallback,
            } => {
                let mut n = old_slots[*old_pos].take().expect("positions are unique");
                std::mem::swap(&mut n.sources, sources);
                std::mem::swap(&mut n.input_types, input_types);
                std::mem::swap(&mut n.out_names, out_names);
                let mut fresh = false;
                if let Some(params) = update.as_ref() {
                    let ctx = BuildCtx {
                        field_maps: &recipe.field_maps,
                        input_types: &n.input_types,
                    };
                    match n.instance.update_params(params, &ctx) {
                        Ok(ParamUpdate::Applied) => report.updated += 1,
                        _ => {
                            if let Some(f) = fallback.as_mut() {
                                // The fallback was built with the new wiring; the running
                                // instance goes back to the control thread in its place.
                                std::mem::swap(&mut n, f);
                                fresh = true;
                            }
                        }
                    }
                }
                (n, fresh)
            }
        };
        if !fresh {
            let upstream = node
                .sources
                .iter()
                .any(|s| matches!(s, Src::Node { pos: p, .. } if restarted[*p]));
            if upstream {
                node.instance.reset();
                node.pending |= ChunkFlags::RESET;
                fresh = true;
                report.reset += 1;
            } else if !matches!(
                sn,
                StagedNode::Keep {
                    update: Some(_),
                    ..
                }
            ) {
                report.kept += 1;
            }
        } else {
            node.pending |= ChunkFlags::RESET;
            report.rebuilt += 1;
        }
        restarted[pos] = fresh;
        new_nodes.push(node);
    }
    std::mem::swap(&mut graph.nodes, new_nodes);
    std::mem::swap(&mut graph.outputs, outputs);
    Ok(report)
}
