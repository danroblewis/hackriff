//! `run_window`: the batch driver a synthesis search evaluates one recipe prefix with
//! (ADR-0015 §3.1 "evaluation uses a batch driver over the hk-blocks graph (no ring reader): an
//! additive `run_window` beside the T-088 graph builder"; T-853 = MAUTO M-2).
//!
//! It is deliberately **not** the live runtime (`hk_pipeline::recipes::graph`): no hot edits,
//! no taps, no swap, no stage streams, no real-time budget. It validates a recipe against the
//! registry, builds a fresh instance of every node, runs one window of the recipe input through
//! them in topological order in fixed chunks (the first flagged `DISCONTINUITY`, the last
//! `END`), and returns each node's [`Evidence`](hk_model::synth::Evidence) and status plus the
//! items every recipe output produced. A fresh graph per call is the window reset §2.1 asks
//! for: evidence covers exactly this window.
//!
//! Chunk scheduling follows the T-088 graph's rule: a node whose inputs carry no item in a
//! chunk is skipped, with the restart flags on those empty inputs held until its first
//! non-empty chunk.

use hk_model::synth::EvidenceSet;
use hk_recipe::{Catalogue, Endpoint, PortRef, Recipe, RecipeError};

use crate::block::{Block, BlockError, Io, PortInfo};
use crate::buffer::{ChunkFlags, ChunkMeta, FrameBuf, Input, Output, PortSlice, PortVec};
use crate::registry::{BuildCtx, Registry};
use crate::status::Status;

/// Most inputs one node may have (as the live graph).
const MAX_NODE_INPUTS: usize = 8;

/// One node's result for the window.
#[derive(Clone, Debug)]
pub struct NodeEvidence {
    /// Node id.
    pub id: String,
    /// Block kind.
    pub block: String,
    /// Block descriptor version (a calibration table is per `name@version`, ADR-0015 §2.2).
    pub version: u32,
    /// What [`Block::evidence`] reported at the end of the window.
    pub evidence: EvidenceSet,
    /// The block's status at the end of the window.
    pub status: Status,
}

impl NodeEvidence {
    /// `name@version`, the calibration key.
    pub fn block_version(&self) -> String {
        format!("{}@{}", self.block, self.version)
    }
}

/// A window's result.
#[derive(Debug)]
pub struct WindowRun {
    /// Every node, in topological order.
    pub nodes: Vec<NodeEvidence>,
    /// Each recipe output's id and every item it produced over the window.
    pub outputs: Vec<(String, PortVec)>,
}

/// Why a window could not run. Messages never echo signal content.
#[derive(Clone, Debug, PartialEq)]
pub enum WindowError {
    /// The recipe does not validate against the registry.
    Invalid(Vec<RecipeError>),
    /// The input's type is not the recipe's.
    InputType,
    /// A node could not be built or initialised, or wiring failed (node id, message).
    Build(String, String),
    /// A block failed while processing (node id, error).
    Process(String, BlockError),
}

impl std::fmt::Display for WindowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WindowError::Invalid(e) => write!(f, "recipe invalid ({} errors)", e.len()),
            WindowError::InputType => write!(f, "input type is not the recipe's input port"),
            WindowError::Build(n, m) => write!(f, "node {n}: {m}"),
            WindowError::Process(n, e) => write!(f, "node {n}: {e}"),
        }
    }
}

impl std::error::Error for WindowError {}

#[derive(Clone, Copy)]
enum Src {
    Input,
    Node(usize, usize),
}

struct Node {
    id: String,
    block: String,
    version: u32,
    instance: Box<dyn Block>,
    sources: Vec<Src>,
    outputs: Vec<Output>,
    pending: ChunkFlags,
}

/// Runs `input` (at `rate_hz`) through `recipe` in chunks of `chunk_items` items and reports
/// every node's evidence (see the module docs). `chunk_items` is clamped to at least 1.
pub fn run_window(
    recipe: &Recipe,
    registry: &Registry,
    rate_hz: f64,
    input: PortSlice<'_>,
    chunk_items: usize,
) -> Result<WindowRun, WindowError> {
    let resolved = recipe.validate(registry).map_err(WindowError::Invalid)?;
    if recipe.input.port != input.port_type() {
        return Err(WindowError::InputType);
    }
    let chunk = chunk_items.max(1);
    let in_info = PortInfo {
        ty: input.port_type(),
        rate_hz,
        max_items: chunk,
        hold_items: 0,
    };
    let mut nodes: Vec<Node> = Vec::with_capacity(resolved.order.len());
    let mut out_names: Vec<Vec<String>> = Vec::with_capacity(resolved.order.len());
    let mut out_infos: Vec<Vec<PortInfo>> = Vec::with_capacity(resolved.order.len());
    for &i in &resolved.order {
        let spec = &recipe.nodes[i];
        let build_err = |m: String| WindowError::Build(spec.id.clone(), m);
        let d = registry
            .descriptor(&spec.block)
            .ok_or_else(|| build_err("unknown block".into()))?;
        if d.inputs.len() > MAX_NODE_INPUTS {
            return Err(build_err(format!("more than {MAX_NODE_INPUTS} inputs")));
        }
        let mut sources = Vec::with_capacity(d.inputs.len());
        let mut infos = Vec::with_capacity(d.inputs.len());
        let mut types = Vec::with_capacity(d.inputs.len());
        for dp in &d.inputs {
            let edge = resolved
                .edges
                .iter()
                .find(|e| e.node == spec.id && e.port == dp.name)
                .ok_or_else(|| build_err(format!("input {} not connected", dp.name)))?;
            let (src, info) = match &edge.from {
                Endpoint::Input => (Src::Input, in_info),
                Endpoint::Node { node, port } => {
                    let p = nodes
                        .iter()
                        .position(|n| &n.id == node)
                        .ok_or_else(|| build_err("source is not upstream".into()))?;
                    let k = out_names[p]
                        .iter()
                        .position(|n| n == port)
                        .ok_or_else(|| build_err("no such source port".into()))?;
                    (Src::Node(p, k), out_infos[p][k])
                }
            };
            sources.push(src);
            infos.push(info);
            types.push(edge.ty);
        }
        let ctx = BuildCtx {
            field_maps: &recipe.field_maps,
            input_types: &types,
        };
        let mut instance = registry
            .build(&spec.block, &spec.params, &ctx)
            .map_err(|e| build_err(e.to_string()))?;
        let outs = instance
            .init(&infos)
            .map_err(|e| build_err(e.to_string()))?;
        if outs.len() != d.outputs.len() {
            return Err(build_err("block returned a different output count".into()));
        }
        nodes.push(Node {
            id: spec.id.clone(),
            block: spec.block.clone(),
            version: d.version,
            instance,
            sources,
            outputs: outs.iter().map(Output::for_port).collect(),
            pending: ChunkFlags::NONE,
        });
        out_names.push(d.outputs.iter().map(|o| o.name.clone()).collect());
        out_infos.push(outs);
    }

    // Recipe outputs: (name, source, accumulated items).
    let mut outputs: Vec<(String, Src, PortVec)> = Vec::with_capacity(recipe.outputs.len());
    for o in &recipe.outputs {
        let bad = || WindowError::Build(o.id.clone(), "unresolvable output source".into());
        let (src, ty) = match PortRef::parse(&o.from).ok_or_else(bad)? {
            PortRef::Input => (Src::Input, in_info.ty),
            PortRef::Node { node, port } => {
                let p = nodes.iter().position(|n| n.id == node).ok_or_else(bad)?;
                let k = match port {
                    Some(name) => out_names[p].iter().position(|n| n == name),
                    None => registry
                        .descriptor(&nodes[p].block)
                        .and_then(|d| d.outputs.iter().position(|o| !o.diagnostic)),
                }
                .ok_or_else(bad)?;
                (Src::Node(p, k), out_infos[p][k].ty)
            }
        };
        outputs.push((o.id.clone(), src, PortVec::with_capacity(ty, 0)));
    }

    const RESTART: ChunkFlags = ChunkFlags(ChunkFlags::DISCONTINUITY.0 | ChunkFlags::RESET.0);
    static NO_BITS: [u8; 0] = [];
    let total = input.len();
    let chunks = total.div_ceil(chunk).max(1);
    for c in 0..chunks {
        let lo = c * chunk;
        let hi = (lo + chunk).min(total);
        let mut flags = ChunkFlags::NONE;
        if c == 0 {
            flags |= ChunkFlags::DISCONTINUITY;
        }
        if c + 1 == chunks {
            flags |= ChunkFlags::END;
        }
        let window_in = Input {
            meta: ChunkMeta {
                index: lo as u64,
                source_index: lo as f64,
                source_per_item: 1.0,
                rate_hz,
                channel: 0,
                flags,
            },
            data: slice(input, lo, hi),
        };
        let blank = Input {
            meta: ChunkMeta::start(0.0),
            data: PortSlice::Bits(&NO_BITS),
        };
        for pos in 0..nodes.len() {
            let (before, rest) = nodes.split_at_mut(pos);
            let node = &mut rest[0];
            let n_in = node.sources.len();
            let mut ins = [blank; MAX_NODE_INPUTS];
            let mut items = n_in == 0;
            let mut arrived = ChunkFlags::NONE;
            for (k, src) in node.sources.iter().enumerate() {
                let inp = match *src {
                    Src::Input => window_in,
                    Src::Node(p, port) => {
                        let o = &before[p].outputs[port];
                        Input {
                            meta: o.meta,
                            data: o.data.as_slice(),
                        }
                    }
                };
                items |= !inp.data.is_empty();
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
            let mut io = Io::new(&ins[..n_in], &mut node.outputs);
            node.instance
                .process(&mut io)
                .map_err(|e| WindowError::Process(node.id.clone(), e))?;
        }
        for (_, src, acc) in &mut outputs {
            let items = match *src {
                Src::Input => window_in.data,
                Src::Node(p, k) => nodes[p].outputs[k].data.as_slice(),
            };
            append(acc, items);
        }
    }

    Ok(WindowRun {
        nodes: nodes
            .iter()
            .map(|n| {
                let mut evidence = EvidenceSet::new();
                n.instance.evidence(&mut evidence);
                NodeEvidence {
                    id: n.id.clone(),
                    block: n.block.clone(),
                    version: n.version,
                    evidence,
                    status: n.instance.status(),
                }
            })
            .collect(),
        outputs: outputs.into_iter().map(|(n, _, v)| (n, v)).collect(),
    })
}

/// Items `lo..hi` of `s` (frames: the whole buffer on the first chunk only — a frames recipe
/// input is not chunked).
fn slice<'a>(s: PortSlice<'a>, lo: usize, hi: usize) -> PortSlice<'a> {
    match s {
        PortSlice::Iq(x) => PortSlice::Iq(&x[lo..hi]),
        PortSlice::Real(x) => PortSlice::Real(&x[lo..hi]),
        PortSlice::Soft(x) => PortSlice::Soft(&x[lo..hi]),
        PortSlice::Bits(x) => PortSlice::Bits(&x[lo..hi]),
        PortSlice::Frames(f) => PortSlice::Frames(f),
    }
}

fn append(acc: &mut PortVec, items: PortSlice<'_>) {
    match (acc, items) {
        (PortVec::Iq(a), PortSlice::Iq(x)) => a.extend_from_slice(x),
        (PortVec::Real(a), PortSlice::Real(x)) | (PortVec::Soft(a), PortSlice::Soft(x)) => {
            a.extend_from_slice(x)
        }
        (PortVec::Bits(a), PortSlice::Bits(x)) => a.extend_from_slice(x),
        (PortVec::Frames(a), PortSlice::Frames(x)) => append_frames(a, x),
        _ => {}
    }
}

fn append_frames(acc: &mut FrameBuf, x: &FrameBuf) {
    for f in x.iter() {
        acc.push(f.bytes, f.info.clone());
    }
}
