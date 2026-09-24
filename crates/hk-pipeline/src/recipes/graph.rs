//! Graph build from a validated recipe (ADR-0011 §1.4, §2.3; T-088). Everything here runs **off
//! the real-time thread**: [`stage`] validates a recipe, negotiates ports, builds and initialises
//! new block instances and pre-sizes their buffers, reusing (by position in the running graph)
//! the instances a hot edit keeps. [`crate::recipes::swap::apply`] then exchanges graphs at a
//! chunk boundary in O(nodes) without allocating, and [`Graph::process`] runs one chunk through
//! the nodes in topological order.

use std::collections::BTreeMap;
use std::sync::Arc;

use hk_blocks::{
    AudioFrames, Block, BlockError, BuildCtx, ChunkFlags, ChunkMeta, Input, Io, Output, PortInfo,
    PortSlice, Registry, Status, TapMask,
};
use hk_recipe::{
    Catalogue, EditPlan, Endpoint, NodeChange, NodeSpec, OutputKind, OutputSpec, Params, PortRef,
    PortType, Recipe, RecipeError, StageView,
};
use serde_json::{Map, Value, json};

/// Most inputs one node may have (the runtime passes them in a fixed array, so `process` never
/// allocates to gather them).
pub const MAX_NODE_INPUTS: usize = 8;

/// Where a node input (or a recipe output) reads from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Src {
    /// The recipe input (the channel DDC's output).
    Input,
    /// Output `port` of the node at `pos` (topological position in the graph).
    Node {
        /// Node position.
        pos: usize,
        /// Output index in the block descriptor's order.
        port: usize,
    },
    /// The sink node at `pos` itself (an `audio` output reads an `audio_out` node, which has
    /// no output port; ADR-0011 §8.2). Never a node input.
    Sink {
        /// Node position.
        pos: usize,
    },
}

/// What the control thread remembers about a running node: enough to plan the next edit.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeShape {
    /// Node id.
    pub id: String,
    /// Block kind.
    pub block: String,
    /// Negotiated input ports.
    pub in_info: Vec<PortInfo>,
    /// Negotiated output ports.
    pub out_info: Vec<PortInfo>,
    /// Output port names (descriptor order).
    pub out_names: Vec<String>,
    /// Output ports flagged diagnostic.
    pub diagnostic: Vec<bool>,
}

/// The running graph's shape, by position.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Shape {
    /// Nodes in topological order.
    pub nodes: Vec<NodeShape>,
}

impl Shape {
    /// Position of node `id`.
    pub fn pos(&self, id: &str) -> Option<usize> {
        self.nodes.iter().position(|n| n.id == id)
    }
}

/// A block instance with its wiring and buffers. Owned by the pipeline thread.
pub struct NodeInst {
    /// Node id.
    pub id: String,
    /// Block kind.
    pub block: String,
    /// The instance.
    pub instance: Box<dyn Block>,
    /// Per descriptor input: its source.
    pub sources: Vec<Src>,
    /// Types on the inputs (for `update_params`' build context).
    pub input_types: Vec<PortType>,
    /// Output port names.
    pub out_names: Vec<String>,
    /// Output buffers, pre-sized from `init`'s `max_items`.
    pub outputs: Vec<Output>,
    /// Tapped outputs (stage streams).
    pub taps: TapMask,
    /// Flags ORed into the next chunk's inputs (`RESET` after a swap).
    pub pending: ChunkFlags,
}

/// A recipe output resolved to a port.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputBinding {
    /// The declared output.
    pub spec: OutputSpec,
    /// Its source.
    pub src: Src,
    /// Port type at the source.
    pub ty: PortType,
    /// Port rate, Hz.
    pub rate_hz: f64,
}

/// An instantiated graph.
pub struct Graph {
    /// Nodes in topological order.
    pub nodes: Vec<NodeInst>,
    /// Resolved recipe outputs (document order).
    pub outputs: Vec<OutputBinding>,
    /// The recipe input port.
    pub input: PortInfo,
}

/// A graph build or edit refused. Messages never echo values.
#[derive(Clone, Debug, PartialEq)]
pub struct StageError {
    /// 400 (invalid recipe or parameters) or 422 (valid, but not realisable here).
    pub status: u16,
    /// Stable code: `invalid` or `unrealisable`.
    pub code: &'static str,
    /// Errors with paths.
    pub errors: Vec<RecipeError>,
    /// Warnings with paths.
    pub warnings: Vec<RecipeError>,
}

impl StageError {
    fn one(status: u16, code: &'static str, path: String, message: String) -> Self {
        Self {
            status,
            code,
            errors: vec![RecipeError { path, message }],
            warnings: Vec::new(),
        }
    }

    fn block(path: String, e: &BlockError) -> Self {
        match e {
            BlockError::Params(_) => Self::one(400, "invalid", path, e.to_string()),
            _ => Self::one(422, "unrealisable", path, e.to_string()),
        }
    }
}

/// A node of a staged graph.
pub enum StagedNode {
    /// A new instance (added, rebuilt, or the initial build), initialised off the RT thread.
    Fresh(Option<NodeInst>),
    /// The running instance at `old_pos` moves over (state kept), rewired to the new sources.
    Keep {
        /// Position in the running graph.
        old_pos: usize,
        /// New sources (swapped into the instance).
        sources: Vec<Src>,
        /// New input types.
        input_types: Vec<PortType>,
        /// Output names (swapped in).
        out_names: Vec<String>,
        /// Hot parameters to apply in place at the swap.
        update: Option<Params>,
        /// A fresh, initialised instance used if `update_params` answers `Rebuild` or fails.
        fallback: Option<NodeInst>,
    },
}

/// A graph prepared off the real-time thread, ready to swap in.
pub struct Staged {
    /// The new revision.
    pub recipe: Arc<Recipe>,
    /// Nodes in the new topological order.
    pub nodes: Vec<StagedNode>,
    /// The new outputs (after the swap: the old ones).
    pub outputs: Vec<OutputBinding>,
    /// The new shape.
    pub shape: Shape,
    /// The edit plan (`None` for an initial build).
    pub plan: Option<EditPlan>,
    /// Warnings from validation.
    pub warnings: Vec<RecipeError>,
    /// Scratch the swap moves the running nodes into (pre-sized; after the swap it holds the
    /// retired instances, dropped on the control thread).
    pub(crate) old_slots: Vec<Option<NodeInst>>,
    /// The new node vector, pre-sized (after the swap: the old, empty vector).
    pub(crate) new_nodes: Vec<NodeInst>,
    /// Per new position: the node restarted at the swap (fresh or reset).
    pub(crate) restarted: Vec<bool>,
}

/// Validates `recipe` against `registry` and prepares its graph at `input`. With `old` (the
/// running revision and its shape) the nodes [`EditPlan`] keeps are reused by position:
/// unchanged nodes as they are, hot parameter changes applied in place at the swap. A kept node
/// whose negotiated inputs would change is rebuilt instead (its buffers and plans depend on
/// them), and everything downstream of a rebuilt node is reset at the swap.
pub fn stage(
    old: Option<(&Recipe, &Shape)>,
    recipe: Arc<Recipe>,
    registry: &Registry,
    input: PortInfo,
) -> Result<Staged, StageError> {
    let resolved = recipe.validate(registry).map_err(|errors| StageError {
        status: 400,
        code: "invalid",
        errors,
        warnings: Vec::new(),
    })?;
    let mut warnings = resolved.warnings.clone();
    if recipe.input.port != input.ty {
        return Err(StageError::one(
            422,
            "unrealisable",
            "input.port".into(),
            format!("this runtime delivers {} input", input.ty),
        ));
    }
    let plan = old.map(|(r, _)| EditPlan::between(r, &recipe, registry));
    let input_changed = plan.as_ref().is_some_and(|p| p.input_changed);
    let doc_index: BTreeMap<&str, usize> = recipe
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.as_str(), i))
        .collect();
    let mut pos_of: BTreeMap<&str, usize> = BTreeMap::new();
    let mut shapes: Vec<NodeShape> = Vec::with_capacity(resolved.order.len());
    let mut nodes = Vec::with_capacity(resolved.order.len());
    let mut restarted = Vec::with_capacity(resolved.order.len());

    for (pos, &i) in resolved.order.iter().enumerate() {
        let spec: &NodeSpec = &recipe.nodes[i];
        let path = format!("nodes[{i}]");
        let d = registry.descriptor(&spec.block).ok_or_else(|| {
            StageError::one(
                400,
                "invalid",
                format!("{path}.block"),
                "unknown block".into(),
            )
        })?;
        if d.inputs.len() > MAX_NODE_INPUTS {
            return Err(StageError::one(
                422,
                "unrealisable",
                path,
                format!("more than {MAX_NODE_INPUTS} inputs"),
            ));
        }
        let mut sources = Vec::with_capacity(d.inputs.len());
        let mut in_info = Vec::with_capacity(d.inputs.len());
        let mut input_types = Vec::with_capacity(d.inputs.len());
        for dp in &d.inputs {
            let edge = resolved
                .edges
                .iter()
                .find(|e| e.node == spec.id && e.port == dp.name)
                .ok_or_else(|| {
                    StageError::one(
                        400,
                        "invalid",
                        format!("{path}.inputs.{}", dp.name),
                        "input not connected".into(),
                    )
                })?;
            let (src, info) = match &edge.from {
                Endpoint::Input => (Src::Input, input),
                Endpoint::Node { node, port } => {
                    let p = *pos_of.get(node.as_str()).ok_or_else(|| {
                        StageError::one(
                            400,
                            "invalid",
                            path.clone(),
                            "source is not upstream".into(),
                        )
                    })?;
                    let k = shapes[p]
                        .out_names
                        .iter()
                        .position(|n| n == port)
                        .ok_or_else(|| {
                            StageError::one(
                                400,
                                "invalid",
                                path.clone(),
                                "no such source port".into(),
                            )
                        })?;
                    (Src::Node { pos: p, port: k }, shapes[p].out_info[k])
                }
            };
            if info.ty != edge.ty {
                return Err(StageError::one(
                    422,
                    "unrealisable",
                    format!("{path}.inputs.{}", dp.name),
                    format!("negotiated {} where {} was resolved", info.ty, edge.ty),
                ));
            }
            sources.push(src);
            in_info.push(info);
            input_types.push(edge.ty);
        }
        let out_names: Vec<String> = d.outputs.iter().map(|o| o.name.clone()).collect();
        let diagnostic: Vec<bool> = d.outputs.iter().map(|o| o.diagnostic).collect();
        let ctx = BuildCtx {
            field_maps: &recipe.field_maps,
            input_types: &input_types,
        };
        let change = plan.as_ref().map_or(NodeChange::Added, |p| {
            p.nodes.get(&spec.id).cloned().unwrap_or(NodeChange::Added)
        });
        let old_node = old.and_then(|(_, sh)| sh.pos(&spec.id).map(|p| (p, &sh.nodes[p])));
        let keepable = !input_changed
            && matches!(
                change,
                NodeChange::Unchanged | NodeChange::Params { hot: true, .. }
            )
            && old_node.is_some_and(|(_, o)| o.block == spec.block && o.in_info == in_info);
        let upstream_restarted = sources
            .iter()
            .any(|s| matches!(s, Src::Node { pos, .. } if restarted[*pos]));
        let fresh = || {
            fresh_node(
                registry,
                spec,
                &ctx,
                &path,
                (&sources, &in_info, &input_types, &out_names),
            )
        };
        let staged = match old_node.filter(|_| keepable) {
            Some((old_pos, o)) if matches!(change, NodeChange::Params { .. }) => {
                // Hot parameters: applied in place at the swap, with a fresh instance ready in
                // case the block answers `Rebuild`.
                let (f, out_info) = fresh()?;
                if out_info == o.out_info {
                    shapes.push(shape_of(
                        spec,
                        in_info,
                        out_info,
                        out_names.clone(),
                        diagnostic,
                    ));
                    restarted.push(upstream_restarted);
                    StagedNode::Keep {
                        old_pos,
                        sources,
                        input_types,
                        out_names,
                        update: Some(spec.params.clone()),
                        fallback: Some(f),
                    }
                } else {
                    // The change would renegotiate ports: rebuild instead.
                    shapes.push(shape_of(spec, in_info, out_info, out_names, diagnostic));
                    restarted.push(true);
                    StagedNode::Fresh(Some(f))
                }
            }
            Some((old_pos, o)) => {
                shapes.push(shape_of(
                    spec,
                    in_info,
                    o.out_info.clone(),
                    out_names.clone(),
                    diagnostic,
                ));
                restarted.push(upstream_restarted);
                StagedNode::Keep {
                    old_pos,
                    sources,
                    input_types,
                    out_names,
                    update: None,
                    fallback: None,
                }
            }
            None => {
                let (n, out_info) = fresh()?;
                shapes.push(shape_of(spec, in_info, out_info, out_names, diagnostic));
                restarted.push(true);
                StagedNode::Fresh(Some(n))
            }
        };
        pos_of.insert(spec.id.as_str(), pos);
        nodes.push(staged);
    }

    let mut outputs = Vec::with_capacity(recipe.outputs.len());
    for (k, o) in recipe.outputs.iter().enumerate() {
        let path = format!("outputs[{k}].from");
        let bad = || StageError::one(400, "invalid", path.clone(), "unresolvable source".into());
        if o.kind == OutputKind::Audio {
            // The sink itself: its product leaves through `Block::audio_frames` at the audio
            // profile's rate, and its input type is the port type the output carries.
            let Some(PortRef::Node { node, port: None }) = PortRef::parse(&o.from) else {
                return Err(bad());
            };
            let p = *pos_of.get(node).ok_or_else(bad)?;
            let ty = shapes[p].in_info.first().ok_or_else(bad)?.ty;
            outputs.push(OutputBinding {
                spec: o.clone(),
                src: Src::Sink { pos: p },
                ty,
                rate_hz: hk_stream::audio::AUDIO_SAMPLE_RATE_HZ,
            });
            continue;
        }
        let (src, info) = match PortRef::parse(&o.from).ok_or_else(bad)? {
            PortRef::Input => (Src::Input, input),
            PortRef::Node { node, port } => {
                let p = *pos_of.get(node).ok_or_else(bad)?;
                let sh = &shapes[p];
                let k = match port {
                    Some(name) => sh.out_names.iter().position(|n| n == name),
                    None => sh.diagnostic.iter().position(|d| !d),
                }
                .ok_or_else(bad)?;
                (Src::Node { pos: p, port: k }, sh.out_info[k])
            }
        };
        if o.view == Some(StageView::Spectrum) {
            warnings.push(RecipeError {
                path: format!("outputs[{k}].view"),
                message: "spectrum view not served yet; the stage stream is raw".into(),
            });
        }
        outputs.push(OutputBinding {
            spec: o.clone(),
            src,
            ty: info.ty,
            rate_hz: info.rate_hz,
        });
    }
    // T-112: a malformed `output_policy` fails closed at the writer (no allowlist: restricted
    // decodes keep no metadata and are not republished); say so instead of swallowing it.
    if recipe
        .outputs
        .iter()
        .any(|o| o.kind == hk_recipe::OutputKind::Messages)
        && let Err(e) = crate::recipes::messages::recipe_output_policy(&recipe)
    {
        warnings.push(RecipeError {
            path: "output_policy".into(),
            message: format!(
                "malformed output_policy ({e}); messages outputs store restricted decodes with \
                 no metadata and do not republish them"
            ),
        });
    }
    let _ = doc_index;
    let old_len = old.map_or(0, |(_, s)| s.nodes.len());
    let n = nodes.len();
    Ok(Staged {
        recipe,
        nodes,
        outputs,
        shape: Shape { nodes: shapes },
        plan,
        warnings,
        old_slots: Vec::with_capacity(old_len.max(n)),
        new_nodes: Vec::with_capacity(n),
        restarted: vec![false; n],
    })
}

fn shape_of(
    spec: &NodeSpec,
    in_info: Vec<PortInfo>,
    out_info: Vec<PortInfo>,
    out_names: Vec<String>,
    diagnostic: Vec<bool>,
) -> NodeShape {
    NodeShape {
        id: spec.id.clone(),
        block: spec.block.clone(),
        in_info,
        out_info,
        out_names,
        diagnostic,
    }
}

/// Builds and initialises a node; returns it and its output ports.
fn fresh_node(
    registry: &Registry,
    spec: &NodeSpec,
    ctx: &BuildCtx<'_>,
    path: &str,
    (sources, in_info, input_types, out_names): (&[Src], &[PortInfo], &[PortType], &[String]),
) -> Result<(NodeInst, Vec<PortInfo>), StageError> {
    let mut instance = registry
        .build(&spec.block, &spec.params, ctx)
        .map_err(|e| StageError::block(format!("{path}.params"), &e))?;
    let out_info = instance
        .init(in_info)
        .map_err(|e| StageError::block(path.to_owned(), &e))?;
    if out_info.len() != out_names.len() {
        return Err(StageError::one(
            422,
            "unrealisable",
            path.to_owned(),
            "block returned a different number of outputs than it declares".into(),
        ));
    }
    let outputs = out_info.iter().map(Output::for_port).collect();
    Ok((
        NodeInst {
            id: spec.id.clone(),
            block: spec.block.clone(),
            instance,
            sources: sources.to_vec(),
            input_types: input_types.to_vec(),
            out_names: out_names.to_vec(),
            outputs,
            taps: TapMask::default(),
            pending: ChunkFlags::NONE,
        },
        out_info,
    ))
}

impl Graph {
    /// A graph with no nodes at `input` (swap a [`Staged`] build into it).
    pub fn empty(input: PortInfo) -> Self {
        Self {
            nodes: Vec::new(),
            outputs: Vec::new(),
            input,
        }
    }

    /// Validates, stages and swaps in `recipe` (the initial build of a pipeline).
    pub fn build(
        recipe: Arc<Recipe>,
        registry: &Registry,
        input: PortInfo,
    ) -> Result<(Self, Vec<RecipeError>), StageError> {
        let mut staged = stage(None, recipe, registry, input)?;
        let mut g = Self::empty(input);
        crate::recipes::swap::apply(&mut g, &mut staged)
            .map_err(|m| StageError::one(500, "invalid", String::new(), m.to_owned()))?;
        Ok((g, staged.warnings))
    }

    /// Runs one chunk of the recipe input through every node, in order. Allocation-free on
    /// sample-rate ports once buffers are sized (ADR-0011 §1.4). On a block error returns the
    /// failing node's position and the error; the pipeline stops (§1.4 rule 6).
    ///
    /// **Restart flags reach the first item.** A flag applies to the chunk whose first item it
    /// applies to (ADR-0011 §1.1), and blocks such as resamplers and clock recovery take their
    /// time origin from the flagged chunk. So a node whose inputs carry no item this chunk is not
    /// processed (its outputs are cleared): the `RESET`/`DISCONTINUITY` flags on those empty
    /// inputs, and the `RESET` a swap set on a rebuilt or reset node, are held until the first
    /// chunk that carries an item.
    pub fn process(&mut self, input: Input<'_>) -> Result<(), (usize, BlockError)> {
        const RESTART: ChunkFlags = ChunkFlags(ChunkFlags::DISCONTINUITY.0 | ChunkFlags::RESET.0);
        static NO_BITS: [u8; 0] = [];
        let blank = Input {
            meta: ChunkMeta {
                index: 0,
                source_index: 0.0,
                source_per_item: 1.0,
                rate_hz: 0.0,
                channel: 0,
                flags: ChunkFlags::NONE,
            },
            data: PortSlice::Bits(&NO_BITS),
        };
        for pos in 0..self.nodes.len() {
            let (before, rest) = self.nodes.split_at_mut(pos);
            let node = &mut rest[0];
            let n_in = node.sources.len();
            let mut ins = [blank; MAX_NODE_INPUTS];
            let mut items = n_in == 0;
            let mut arrived = ChunkFlags::NONE;
            for (k, src) in node.sources.iter().enumerate() {
                let inp = match *src {
                    Src::Input | Src::Sink { .. } => input,
                    Src::Node { pos: p, port } => {
                        let o = &before[p].outputs[port];
                        Input {
                            meta: o.meta,
                            data: o.data.as_slice(),
                        }
                    }
                };
                // A frames recipe input (a follow-hops merge, T-093) runs its reader every chunk:
                // frames carry their own time, and an empty chunk still advances the watermark.
                items |= !inp.data.is_empty()
                    || (matches!(src, Src::Input) && matches!(inp.data, PortSlice::Frames(_)));
                arrived |= inp.meta.flags;
                ins[k] = inp;
            }
            for o in node.outputs.iter_mut() {
                o.begin_chunk();
            }
            if !items {
                node.pending |= ChunkFlags(arrived.0 & RESTART.0);
                continue;
            }
            for inp in &mut ins[..n_in] {
                inp.meta.flags |= node.pending;
            }
            node.pending = ChunkFlags::NONE;
            let mut io = Io::new(&ins[..n_in], &mut node.outputs).with_taps(node.taps);
            node.instance.process(&mut io).map_err(|e| (pos, e))?;
        }
        Ok(())
    }

    /// The output buffer a source names (`None` for the recipe input and a sink).
    pub fn output(&self, src: Src) -> Option<&Output> {
        match src {
            Src::Input | Src::Sink { .. } => None,
            Src::Node { pos, port } => self.nodes.get(pos)?.outputs.get(port),
        }
    }

    /// The finished audio frames of the sink a source names (`None` for anything else).
    pub fn audio_frames(&mut self, src: Src) -> Option<&mut AudioFrames> {
        match src {
            Src::Sink { pos } => self.nodes.get_mut(pos)?.instance.audio_frames(),
            _ => None,
        }
    }

    /// Every node's status readout, flattened as `<node>.<metric>` keys (allocates: status rate).
    pub fn status_metadata(&self) -> Map<String, Value> {
        let mut m = Map::new();
        for n in &self.nodes {
            n.instance.status().to_metadata(&n.id, &mut m);
        }
        m
    }

    /// Per-node status as `[{id, block, status}]`.
    pub fn node_status(&self) -> Vec<(String, String, Status)> {
        self.nodes
            .iter()
            .map(|n| (n.id.clone(), n.block.clone(), n.instance.status()))
            .collect()
    }
}

/// An edit plan as the API serves it.
pub fn plan_json(plan: &EditPlan) -> Value {
    let nodes: Vec<Value> = plan
        .nodes
        .iter()
        .map(|(id, c)| match c {
            NodeChange::Unchanged => json!({"id": id, "change": "unchanged"}),
            NodeChange::Params { keys, hot } => json!({
                "id": id, "change": if *hot { "params-hot" } else { "params-cold" }, "keys": keys
            }),
            NodeChange::Rebuilt => json!({"id": id, "change": "rebuilt"}),
            NodeChange::Added => json!({"id": id, "change": "added"}),
            NodeChange::Removed => json!({"id": id, "change": "removed"}),
        })
        .collect();
    json!({
        "nodes": nodes,
        "reset": plan.reset,
        "field_maps_changed": plan.field_maps_changed,
        "input_changed": plan.input_changed,
        "outputs_changed": plan.outputs_changed,
    })
}

/// Recipe errors as `[{path, message}]`.
pub fn errors_json(errors: &[RecipeError]) -> Value {
    Value::Array(
        errors
            .iter()
            .map(|e| json!({"path": e.path, "message": e.message}))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use hk_blocks::{BlockFactory, ParamUpdate, PortVec};
    use hk_recipe::{BlockDescriptor, ParamSchema, ParamType, Params, PortSpec};
    use num_complex::Complex32;
    use serde_json::json;

    use super::*;

    type Seen = Arc<Mutex<Vec<(u32, ChunkFlags)>>>;

    /// `sparse` passes its input only on odd chunks (empty output, flags propagated, on even
    /// ones); `probe` copies and records the flags of every chunk it is given.
    struct Factory {
        d: BlockDescriptor,
        seen: Seen,
        builds: Arc<Mutex<u32>>,
    }

    impl BlockFactory for Factory {
        fn descriptor(&self) -> &BlockDescriptor {
            &self.d
        }
        fn build(&self, _: &Params, _: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
            let mut b = self.builds.lock().unwrap();
            *b += 1;
            Ok(Box::new(Test {
                sparse: self.d.name == "sparse",
                instance: *b,
                chunks: 0,
                seen: Arc::clone(&self.seen),
            }))
        }
    }

    struct Test {
        sparse: bool,
        instance: u32,
        chunks: u64,
        seen: Seen,
    }

    impl Block for Test {
        fn init(&mut self, i: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
            Ok(vec![i[0]])
        }
        fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
            let input = io.input(0)?;
            self.chunks += 1;
            if !self.sparse {
                self.seen
                    .lock()
                    .unwrap()
                    .push((self.instance, input.meta.flags));
            }
            let out = io.output(0)?;
            out.meta = ChunkMeta {
                index: out.meta.index,
                ..input.meta
            };
            if let (PortSlice::Iq(x), PortVec::Iq(y)) = (input.data, &mut out.data)
                && (!self.sparse || self.chunks % 2 == 0)
            {
                y.extend_from_slice(x);
            }
            Ok(())
        }
        fn reset(&mut self) {}
        fn update_params(
            &mut self,
            _: &Params,
            _: &BuildCtx<'_>,
        ) -> Result<ParamUpdate, BlockError> {
            Ok(ParamUpdate::Rebuild)
        }
        fn status(&self) -> Status {
            Status::default()
        }
    }

    fn registry(seen: &Seen) -> Registry {
        let mut r = Registry::builtin();
        for name in ["sparse", "probe"] {
            let cold = ParamSchema {
                name: "x".into(),
                ty: ParamType::Int {
                    min: None,
                    max: None,
                },
                required: false,
                default: None,
                hot: false,
                doc: String::new(),
            };
            r.register(Arc::new(Factory {
                d: hk_blocks::schema::descriptor(
                    name,
                    "test",
                    "test",
                    vec![PortSpec::new("in", PortType::Iq)],
                    vec![PortSpec::new("out", PortType::Iq)],
                    vec![cold],
                    true,
                ),
                seen: Arc::clone(seen),
                builds: Arc::new(Mutex::new(0)),
            }))
            .unwrap();
        }
        r
    }

    fn recipe(probe_x: i64) -> Arc<Recipe> {
        Arc::new(
            serde_json::from_value(json!({
                "schema": "hackriff.recipe", "schema_version": 2, "id": "flags", "version": 1,
                "name": "flags", "input": {"port": "iq"},
                "nodes": [{"id": "s", "block": "sparse"},
                          {"id": "p", "block": "probe", "params": {"x": probe_x}}],
                "outputs": [{"id": "o", "kind": "stage", "from": "p"}],
                "output_policy": {"content_class": "unrestricted"}
            }))
            .unwrap(),
        )
    }

    #[test]
    fn a_started_or_rebuilt_node_sees_its_restart_flag_on_its_first_item() {
        let seen: Seen = Arc::default();
        let reg = registry(&seen);
        let input = PortInfo {
            ty: PortType::Iq,
            rate_hz: 1e3,
            max_items: 8,
            hold_items: 0,
        };
        let old = recipe(1);
        let mut staged = stage(None, Arc::clone(&old), &reg, input).unwrap();
        let shape = staged.shape.clone();
        let mut g = Graph::empty(input);
        crate::recipes::swap::apply(&mut g, &mut staged).unwrap();
        let x = [Complex32::new(1.0, 0.0); 8];
        let mut meta = ChunkMeta::start(1e3);
        let run = |g: &mut Graph, n: usize, meta: &mut ChunkMeta| {
            for _ in 0..n {
                g.process(Input {
                    meta: *meta,
                    data: PortSlice::Iq(&x),
                })
                .unwrap();
                meta.flags = ChunkFlags::NONE;
            }
        };
        // Chunk 1: `sparse` emits nothing (its output carries the stream-start flag); chunk 2
        // is the probe's first item, and must carry DISCONTINUITY (stream start) and RESET.
        run(&mut g, 4, &mut meta);
        let got = seen.lock().unwrap().clone();
        assert_eq!(
            got.len(),
            2,
            "the probe runs only on chunks with items: {got:?}"
        );
        assert!(got[0].1.contains(ChunkFlags::DISCONTINUITY), "{got:?}");
        assert!(got[0].1.contains(ChunkFlags::RESET), "{got:?}");
        assert_eq!(got[1].1, ChunkFlags::NONE);

        // A cold edit rebuilds the probe: its first item carries RESET even though the chunk
        // right after the swap brings it nothing.
        seen.lock().unwrap().clear();
        let mut staged = stage(Some((&old, &shape)), recipe(2), &reg, input).unwrap();
        let report = crate::recipes::swap::apply(&mut g, &mut staged).unwrap();
        assert_eq!((report.rebuilt, report.kept), (1, 1));
        run(&mut g, 4, &mut meta);
        let got = seen.lock().unwrap().clone();
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].0, 2, "the fresh instance");
        assert!(got[0].1.contains(ChunkFlags::RESET), "{got:?}");
        assert_eq!(got[1].1, ChunkFlags::NONE);
    }
}
