//! follow_hops: one recipe pipeline spanning several channels (ADR-0011 §2.5, T-093).
//!
//! # Shape
//! A follow-hops recipe (`input.channels.mode = follow-hops`, exactly one `follow_hops` node) is
//! [`split`] at the merge node into two sub-recipes, each staged and hot-edited with the ordinary
//! graph machinery ([`graph::stage`], [`swap::apply`], `EditPlan`):
//! - **upstream** (`iq` in): every node that is not the merge node or downstream of it, with one
//!   synthetic stage output naming the merge node's source. It is instantiated once per channel
//!   as a [`Lane`]: its own channel DDC and graph, `ChunkMeta::channel` = the channel index.
//! - **downstream** (`frames` in): the merge node and everything reachable from it, run once. It
//!   is the pipeline's main graph: recipe outputs, taps and sinks bind to it, so every recipe
//!   output must come from the merge node or downstream of it.
//!
//! Per ring chunk the pipeline thread runs every lane on the chunk (a `DISCONTINUITY` stays pending
//! on a lane whose DDC yields no samples for that chunk and reaches its graph with the next
//! samples, T-107), copies each lane's frames
//! into one pre-sized merge buffer (stamping `FrameInfo::channel` with the lane's index, so the
//! tag never depends on a block) and runs the downstream graph once with that buffer and
//! `meta.source_index` = the ring sample every lane has been processed up to (the merge node's
//! watermark). The merge node orders and deduplicates (`hk_blocks::blocks::multi`). Frame
//! records carry `channel` and that channel's `channel_hz`; the header lists the channels known
//! at open. When the source ends, the merge is flushed (`END`).
//!
//! # Channel sets
//! [`resolve_channels`], in order: `list_hz` (a static list); an inventory **hop-set** emitter
//! target (its fingerprint's `hop_set_hz`, measured blindly by the tracker's hop-set linking,
//! T-059/T-084); otherwise the blind detections in `band_hz` (or the target's extent): inventory
//! emitters narrower than two channel bandwidths, most sightings first. Channels closer than half
//! a channel bandwidth merge; at most `max_channels`. Nothing is looked up in a band plan.
//!
//! A channel set change ([`set_channels`]; `RecipeRuntime::refresh_channels` re-resolves the
//! source, e.g. a hop set that gained a channel) builds the new lanes off the real-time thread
//! at the tune read then, and swaps them in at a chunk boundary. Each lane records the tune its
//! DDC was planned for; a lane built before a retune that the pipeline thread has already applied
//! is re-planned for the running tune at the swap (T-107), and if the channel is no longer inside
//! the window the change is refused (`409 outside_window`) and nothing changes. At the swap the other lanes keep their instances and state, so
//! they continue without a gap, and the merge input carries `CHANNEL_CHANGE`. Channel indices
//! are never reused within a pipeline. A hot edit stages the upstream sub-recipe once per lane
//! and swaps every lane and the downstream graph at the same boundary.
//!
//! # Budget
//! A follow-hops pipeline with N channels counts as **N `recipe` chains**, each at the
//! per-channel cost (`chain_mcores` at the window rate: every lane runs its own DDC over the whole
//! ring chunk, which dominates). The pipeline's own slot covers the first channel; its control
//! state holds N − 1 extra slots. Adding a channel claims one (`503 busy`, nothing changes);
//! removing one releases one. The merge buffer and the downstream input port are sized for
//! `max_channels`, so a channel change never renegotiates ports.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use hk_blocks::{ChunkFlags, ChunkMeta, FrameBuf, Input, PortInfo, PortSlice, PortVec, Registry};
use hk_core::ReadChunk;
use hk_dsp::{Ddc, InputInfo};
use hk_model::{FreqRange, InventoryQuery};
use hk_recipe::{
    ChannelsSpec, Endpoint, FOLLOW_HOPS_BLOCK, InputSpec, OutputKind, OutputSpec, PortRef,
    PortType, Recipe,
};
use hk_stream::inspector::ChannelInfo;
use num_complex::Complex;
use serde_json::{Map, Value, json};

use crate::chains::budget::{ChainKind, Slot};
use crate::chains::listen::ListenConfig;
use crate::recipes::graph::{self, Graph, Shape, Src, Staged};
use crate::recipes::runtime::{PipelineCtl, RuntimeError, Target, channel_plan, in_window};
use crate::recipes::swap;
use crate::run::Shared;
use crate::stats::Counters;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn unrealisable(message: &str) -> RuntimeError {
    RuntimeError::new(422, "unrealisable", message)
}

/// A follow-hops recipe cut at its merge node.
#[derive(Clone, Debug, PartialEq)]
pub struct Split {
    /// Per-channel part (`iq` in; one stage output: the merge node's source).
    pub up: Recipe,
    /// Merge node and downstream (`frames` in; the recipe's outputs).
    pub down: Recipe,
}

/// Cuts `recipe` at its `follow_hops` node (module docs). Every node input becomes explicit, so
/// both halves resolve exactly as the whole did.
pub fn split(recipe: &Recipe, registry: &Registry) -> Result<Split, RuntimeError> {
    let resolved = recipe.validate(registry).map_err(RuntimeError::invalid)?;
    let merge = recipe
        .nodes
        .iter()
        .find(|n| n.block == FOLLOW_HOPS_BLOCK)
        .ok_or_else(|| unrealisable("a follow-hops recipe needs a follow_hops node"))?;
    let mut down: BTreeSet<&str> = BTreeSet::from([merge.id.as_str()]);
    loop {
        let before = down.len();
        for e in &resolved.edges {
            if let Endpoint::Node { node, .. } = &e.from
                && down.contains(node.as_str())
            {
                down.insert(e.node.as_str());
            }
        }
        if down.len() == before {
            break;
        }
    }
    let reference = |from: &Endpoint| match from {
        Endpoint::Input => "input".to_owned(),
        Endpoint::Node { node, port } => format!("{node}.{port}"),
    };
    let mut inputs: BTreeMap<&str, BTreeMap<String, String>> = BTreeMap::new();
    let mut merge_src = None;
    for e in &resolved.edges {
        let from = reference(&e.from);
        if e.node == merge.id {
            match &e.from {
                Endpoint::Node { node, .. } if !down.contains(node.as_str()) => {
                    merge_src = Some(from.clone());
                }
                _ => {
                    return Err(unrealisable(
                        "follow_hops must read a per-channel node, not the recipe input",
                    ));
                }
            }
            inputs
                .entry(e.node.as_str())
                .or_default()
                .insert(e.port.clone(), "input".into());
            continue;
        }
        if down.contains(e.node.as_str())
            && !matches!(&e.from, Endpoint::Node { node, .. } if down.contains(node.as_str()))
        {
            return Err(unrealisable(
                "a node downstream of follow_hops may only read the merge node and its descendants",
            ));
        }
        inputs
            .entry(e.node.as_str())
            .or_default()
            .insert(e.port.clone(), from);
    }
    let merge_src = merge_src.ok_or_else(|| unrealisable("follow_hops has no source"))?;
    for (i, o) in recipe.outputs.iter().enumerate() {
        let from_down = match PortRef::parse(&o.from) {
            Some(PortRef::Node { node, .. }) => down.contains(node),
            _ => false,
        };
        if !from_down {
            return Err(RuntimeError {
                errors: vec![hk_recipe::RecipeError {
                    path: format!("outputs[{i}].from"),
                    message: "outputs of a follow-hops recipe come from the merge node or downstream of it".into(),
                }],
                ..unrealisable("outputs of a follow-hops recipe come from the merge node or downstream of it")
            });
        }
    }
    let nodes = |want_down: bool| {
        recipe
            .nodes
            .iter()
            .filter(|n| down.contains(n.id.as_str()) == want_down)
            .map(|n| {
                let mut n = n.clone();
                n.inputs = inputs.get(n.id.as_str()).cloned().unwrap_or_default();
                n
            })
            .collect::<Vec<_>>()
    };
    let up_nodes = nodes(false);
    if up_nodes.is_empty() {
        return Err(unrealisable(
            "a follow-hops recipe needs per-channel nodes upstream of follow_hops",
        ));
    }
    let up = Recipe {
        input: InputSpec {
            channels: ChannelsSpec::Single,
            ..recipe.input.clone()
        },
        nodes: up_nodes,
        outputs: vec![OutputSpec {
            id: "hops_in".into(),
            kind: OutputKind::Stage,
            from: merge_src,
            view: None,
            decode: None,
        }],
        refine: None,
        ..recipe.clone()
    };
    let down_recipe = Recipe {
        input: InputSpec {
            port: PortType::Frames,
            sample_rate_hz: None,
            bandwidth_hz: None,
            channels: recipe.input.channels.clone(),
        },
        nodes: nodes(true),
        refine: recipe
            .refine
            .clone()
            .filter(|r| down.contains(r.objective.node.as_str())),
        ..recipe.clone()
    };
    Ok(Split {
        up,
        down: down_recipe,
    })
}

/// Channels a follow-hops pipeline attaches to, and where they came from.
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelSet {
    /// Channel centres, Hz, ascending.
    pub channels_hz: Vec<f64>,
    /// `list`, `hop-set` or `detections`.
    pub source: &'static str,
}

/// Keeps finite positive frequencies in rank order, drops those within `min_sep` of one kept,
/// stops at `max`, and sorts ascending.
pub fn normalize(ranked: impl IntoIterator<Item = f64>, min_sep: f64, max: usize) -> Vec<f64> {
    let mut kept: Vec<f64> = Vec::new();
    for f in ranked {
        if kept.len() >= max {
            break;
        }
        if f.is_finite() && f > 0.0 && !kept.iter().any(|k| (k - f).abs() < min_sep) {
            kept.push(f);
        }
    }
    kept.sort_by(f64::total_cmp);
    kept
}

fn hop_set_of(fingerprint: &Value) -> Vec<f64> {
    fingerprint
        .get("hop_set_hz")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_f64).collect())
        .unwrap_or_default()
}

/// `(channel bandwidth, max channels)` of a follow-hops recipe.
fn hops_params(recipe: &Recipe) -> Result<(f64, usize), RuntimeError> {
    match &recipe.input.channels {
        ChannelsSpec::FollowHops {
            channel_bandwidth_hz,
            max_channels,
            ..
        } => Ok((*channel_bandwidth_hz, usize::from(*max_channels))),
        ChannelsSpec::Single => Err(RuntimeError::new(
            422,
            "not_follow_hops",
            "the recipe follows one channel",
        )),
    }
}

/// Resolves the channel set of a follow-hops `recipe` on `target` (extent `(lo, hi)`, Hz).
pub(crate) fn resolve_channels(
    shared: &Shared,
    recipe: &Recipe,
    target: &Target,
    extent: (f64, f64),
) -> Result<ChannelSet, RuntimeError> {
    let (cbw, max) = hops_params(recipe)?;
    let ChannelsSpec::FollowHops {
        band_hz, list_hz, ..
    } = &recipe.input.channels
    else {
        unreachable!("checked by hops_params");
    };
    let sep = 0.5 * cbw;
    let found = |channels_hz: Vec<f64>, source| {
        if channels_hz.is_empty() {
            Err(RuntimeError::new(
                422,
                "no_channels",
                "no channels to follow: give input.channels.list_hz, a hop-set emitter, or a band \
                 with detections",
            ))
        } else {
            Ok(ChannelSet {
                channels_hz,
                source,
            })
        }
    };
    if !list_hz.is_empty() {
        return found(normalize(list_hz.iter().copied(), sep, max), "list");
    }
    if let Target::Emitter(id) = target {
        let e = shared
            .repo()
            .emitter(*id)
            .map_err(|_| RuntimeError::new(404, "not_found", "no such emitter"))?;
        let hz = hop_set_of(&e.fingerprint);
        if !hz.is_empty() {
            return found(normalize(hz, sep, max), "hop-set");
        }
    }
    let [lo, hi] = band_hz.unwrap_or([extent.0, extent.1]);
    let q = InventoryQuery {
        freq: Some(FreqRange {
            lo_hz: lo,
            hi_hz: hi,
        }),
        limit: 256,
        ..InventoryQuery::default()
    };
    let page = shared
        .repo()
        .query_inventory(&q)
        .map_err(|_| RuntimeError::new(500, "failed", "reading the inventory"))?;
    let mut ranked: Vec<(u64, f64)> = page
        .entries
        .iter()
        .map(|x| &x.emitter)
        .filter(|e| {
            (lo..=hi).contains(&e.f_center_hz)
                && e.bandwidth_hz <= 2.0 * cbw
                && hop_set_of(&e.fingerprint).is_empty()
        })
        .map(|e| (e.count, e.f_center_hz))
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.total_cmp(&b.1)));
    found(
        normalize(ranked.into_iter().map(|(_, f)| f), sep, max),
        "detections",
    )
}

/// One followed channel: its DDC and upstream graph instance. Owned by the pipeline thread.
pub(crate) struct Lane {
    pub index: u16,
    pub center_hz: f64,
    pub bandwidth_hz: f64,
    /// Window `(centre, rate)` the DDC is planned for.
    tune: (f64, f64),
    ddc: Ddc,
    graph: Graph,
    /// A `DISCONTINUITY` not yet delivered to the graph.
    disc: bool,
}

impl Lane {
    /// Re-plans the DDC for `tune` (window centre, rate); allocates, retune rate only.
    fn retune(&mut self, tune: (f64, f64)) -> Result<(), String> {
        if tune == self.tune {
            return Ok(());
        }
        let (lo, hi) = (
            self.center_hz - 0.5 * self.bandwidth_hz,
            self.center_hz + 0.5 * self.bandwidth_hz,
        );
        if !in_window(tune.0, tune.1, lo, hi) {
            return Err(format!(
                "retune: channel {} left the tuned window",
                self.index
            ));
        }
        let mut spec = self.ddc.spec().clone();
        spec.center_offset_hz = self.center_hz - tune.0;
        let ddc = Ddc::new(spec, tune.1)
            .map_err(|_| "rate-change: a channel cannot be down-converted".to_owned())?;
        if (ddc.output_rate_hz() - self.graph.input.rate_hz).abs() > 1e-6 {
            return Err("rate-change: the channel rate changed; start the pipeline again".into());
        }
        self.ddc = ddc;
        self.tune = tune;
        self.disc = true;
        Ok(())
    }
}

/// Builds channel `index` at `center` (off the real-time thread): the lane, its frames output
/// port and the upstream graph's shape.
fn build_lane(
    up: &Arc<Recipe>,
    registry: &Registry,
    index: u16,
    center: f64,
    cbw: f64,
    tune: (f64, f64),
) -> Result<(Lane, PortInfo, Shape), RuntimeError> {
    let plan = channel_plan(up, center, cbw, tune)?;
    let mut staged = graph::stage(None, Arc::clone(up), registry, plan.input)?;
    let shape = staged.shape.clone();
    let mut g = Graph::empty(plan.input);
    swap::apply(&mut g, &mut staged).map_err(|m| RuntimeError::new(500, "failed", m))?;
    let out = match g.outputs.first().map(|b| b.src) {
        Some(Src::Node { pos, port }) => shape.nodes[pos].out_info[port],
        _ => return Err(unrealisable("follow_hops has no per-channel source")),
    };
    if out.ty != PortType::Frames {
        return Err(unrealisable("follow_hops reads frames"));
    }
    Ok((
        Lane {
            index,
            center_hz: center,
            bandwidth_hz: plan.bandwidth_hz,
            tune: plan.tune,
            ddc: plan.ddc,
            graph: g,
            disc: true,
        },
        out,
        shape,
    ))
}

/// Claims `n` extra `recipe` slots at the per-channel cost (budget: module docs).
pub(crate) fn claim_extra(
    counters: &Arc<Counters>,
    cfg: &ListenConfig,
    n: usize,
    rate_hz: f64,
) -> Result<Vec<Slot>, RuntimeError> {
    let mut slots = Vec::with_capacity(n);
    for _ in 0..n {
        slots.push(Slot::claim(
            counters,
            &cfg.limits(),
            ChainKind::Recipe,
            cfg.chain_mcores(rate_hz, Some(false)),
            rate_hz,
        )?);
    }
    Ok(slots)
}

/// A follow-hops pipeline prepared at start.
pub(crate) struct Prepared {
    pub up: Arc<Recipe>,
    pub down: Arc<Recipe>,
    up_shape: Shape,
    pub lanes: Vec<Lane>,
    lane_input: PortInfo,
    /// The downstream graph's input port.
    pub merge_port: PortInfo,
    /// Lowest and highest channel edge, Hz.
    pub extent: (f64, f64),
    cbw: f64,
    max_channels: usize,
    source: &'static str,
}

/// Splits `recipe`, resolves its channels and builds one lane per channel at `tune`.
pub(crate) fn prepare(
    shared: &Shared,
    recipe: &Recipe,
    registry: &Registry,
    target: &Target,
    extent: (f64, f64),
    tune: (f64, f64),
) -> Result<Prepared, RuntimeError> {
    let (cbw, max_channels) = hops_params(recipe)?;
    let Split { up, down } = split(recipe, registry)?;
    let set = resolve_channels(shared, recipe, target, extent)?;
    let (up, down) = (Arc::new(up), Arc::new(down));
    let mut lanes = Vec::with_capacity(set.channels_hz.len());
    let mut first = None;
    for (k, &f) in set.channels_hz.iter().enumerate() {
        let (lane, out, shape) = build_lane(&up, registry, k as u16, f, cbw, tune)?;
        if first.is_none() {
            first = Some((lane.graph.input, out, shape));
        }
        lanes.push(lane);
    }
    let (lane_input, out, up_shape) =
        first.ok_or_else(|| RuntimeError::new(422, "no_channels", "no channels to follow"))?;
    let n = max_channels.max(lanes.len());
    let rate = if out.rate_hz.is_finite() && out.rate_hz > 0.0 {
        out.rate_hz
    } else {
        1.0
    };
    let merge_port = PortInfo {
        ty: PortType::Frames,
        rate_hz: rate * n as f64,
        max_items: out.max_items.max(1).saturating_mul(n),
        hold_items: 0,
    };
    let lo = lanes
        .iter()
        .map(|l| l.center_hz - 0.5 * l.bandwidth_hz)
        .fold(f64::INFINITY, f64::min);
    let hi = lanes
        .iter()
        .map(|l| l.center_hz + 0.5 * l.bandwidth_hz)
        .fold(f64::NEG_INFINITY, f64::max);
    Ok(Prepared {
        up,
        down,
        up_shape,
        lanes,
        lane_input,
        merge_port,
        extent: (lo, hi),
        cbw,
        max_channels,
        source: set.source,
    })
}

fn channel_info(l: &Lane) -> ChannelInfo {
    ChannelInfo {
        index: l.index,
        center_hz: l.center_hz,
        bandwidth_hz: l.bandwidth_hz,
    }
}

impl Prepared {
    /// The channels, for stream headers.
    pub fn channel_infos(&self) -> Vec<ChannelInfo> {
        self.lanes.iter().map(channel_info).collect()
    }

    /// Hands the lanes to the pipeline thread's state and the rest to the control state.
    pub fn finish(self, extra_slots: Vec<Slot>, fs: f64) -> (HopsCtl, Hops) {
        let channels = self.channel_infos();
        let table: Vec<f64> = self.lanes.iter().map(|l| l.center_hz).collect();
        let cap = self.merge_port.max_items;
        let next_index = self.lanes.len() as u16;
        let hops = Hops {
            lanes: self.lanes,
            merged: FrameBuf::with_capacity(cap, cap.saturating_mul(64).min(1 << 24)),
            merge_port: self.merge_port,
            channels_hz: table.clone(),
            merge_index: 0,
            pending_flags: ChunkFlags::NONE,
            fs,
        };
        let ctl = HopsCtl {
            control: Mutex::new(HopsControl {
                up: self.up,
                down: self.down,
                up_shape: self.up_shape,
                channels,
                table,
                next_index,
                cbw: self.cbw,
                max_channels: self.max_channels,
                lane_input: self.lane_input,
                source: self.source,
                extra_slots,
            }),
            pending: Mutex::new(None),
            edit_pending: AtomicBool::new(false),
        };
        (ctl, hops)
    }
}

/// The pipeline thread's follow-hops state.
pub(crate) struct Hops {
    lanes: Vec<Lane>,
    /// Every lane's frames of the current chunk (pre-sized for `max_channels`).
    merged: FrameBuf,
    merge_port: PortInfo,
    /// Channel centre by channel index (never shrinks), for frame records.
    pub channels_hz: Vec<f64>,
    merge_index: u64,
    /// `CHANNEL_CHANGE` after a channel set change, for the next merge input.
    pending_flags: ChunkFlags,
    fs: f64,
}

impl Hops {
    /// Runs every lane on `chunk` and the downstream `graph` once on their merged frames.
    /// Allocation-free once buffers are sized.
    pub fn process(
        &mut self,
        chunk: &ReadChunk,
        samples: &[Complex<i8>],
        flags: ChunkFlags,
        graph: &mut Graph,
    ) -> Result<(), String> {
        self.merged.clear();
        for lane in &mut self.lanes {
            let Lane {
                index,
                ddc,
                graph: lg,
                disc,
                ..
            } = lane;
            // Held until delivered: a DDC that yields nothing on this chunk must not lose it.
            *disc |= flags.contains(ChunkFlags::DISCONTINUITY);
            let block = ddc
                .process(InputInfo::from(chunk), samples)
                .map_err(|_| "error: channel down-conversion failed".to_owned())?;
            if block.samples.is_empty() {
                continue;
            }
            let mut f = flags;
            if *disc {
                f |= ChunkFlags::DISCONTINUITY;
                *disc = false;
            }
            let time = &block.header.time;
            let meta = ChunkMeta {
                index: time.out_index,
                source_index: time.source_index,
                source_per_item: time.source_per_output,
                rate_hz: block.header.sample_rate_hz,
                channel: *index,
                flags: f,
            };
            if let Err((pos, e)) = lg.process(Input {
                meta,
                data: PortSlice::Iq(block.samples),
            }) {
                return Err(format!(
                    "error: channel {index} node {}: {e}",
                    lg.nodes[pos].id
                ));
            }
            if let Some(src) = lg.outputs.first().map(|b| b.src)
                && let Some(out) = lg.output(src)
                && let PortVec::Frames(buf) = &out.data
            {
                for fr in buf.iter() {
                    let mut info = fr.info.clone();
                    info.channel = *index;
                    self.merged.push(fr.bytes, info);
                }
            }
        }
        self.run_merge(chunk.end_sample() as f64, flags, graph)
    }

    fn run_merge(
        &mut self,
        watermark: f64,
        flags: ChunkFlags,
        graph: &mut Graph,
    ) -> Result<(), String> {
        let rate = self.merge_port.rate_hz;
        let meta = ChunkMeta {
            index: self.merge_index,
            source_index: watermark,
            source_per_item: self.fs / rate,
            rate_hz: rate,
            channel: 0,
            flags: flags | std::mem::replace(&mut self.pending_flags, ChunkFlags::NONE),
        };
        self.merge_index += self.merged.len() as u64;
        graph
            .process(Input {
                meta,
                data: PortSlice::Frames(&self.merged),
            })
            .map_err(|(pos, e)| format!("error: node {}: {e}", graph.nodes[pos].id))
    }

    /// Flushes the merge (`END`) when the source ends.
    pub fn flush(&mut self, at_sample: u64, graph: &mut Graph) -> Result<(), String> {
        self.merged.clear();
        self.run_merge(at_sample as f64, ChunkFlags::END, graph)
    }

    /// Re-plans every lane's DDC for a new tune (window centre, rate).
    pub fn retune(&mut self, tune: (f64, f64)) -> Result<(), String> {
        self.lanes.iter_mut().try_for_each(|l| l.retune(tune))
    }

    /// Every lane's node status as `ch<index>.<node>.<metric>` (allocates: status rate).
    pub fn status_metadata(&self, m: &mut Map<String, Value>) {
        for lane in &self.lanes {
            for n in &lane.graph.nodes {
                n.instance
                    .status()
                    .to_metadata(&format!("ch{}.{}", lane.index, n.id), m);
            }
        }
    }

    /// Whether `edit` staged every running lane.
    pub fn lanes_ready(&self, edit: &LaneEdit) -> bool {
        self.lanes
            .iter()
            .all(|l| edit.staged.iter().any(|(i, _)| *i == l.index))
    }

    /// Swaps each lane's staged upstream graph in (after [`Hops::lanes_ready`]).
    pub fn apply_lanes(&mut self, edit: &mut LaneEdit) {
        for lane in &mut self.lanes {
            if let Some((_, st)) = edit.staged.iter_mut().find(|(i, _)| *i == lane.index) {
                let _ = swap::apply(&mut lane.graph, st);
            }
        }
    }

    /// Applies a channel set change at a chunk boundary: moves lanes, no allocation unless an
    /// added lane was planned for a tune older than `tune` (the tune the running lanes follow),
    /// which re-plans its DDC first. A lane that cannot follow `tune` refuses the whole change
    /// (nothing moves).
    pub fn apply_channels(&mut self, e: &mut ChannelEdit, tune: (f64, f64)) -> Result<(), String> {
        for l in &mut e.add {
            l.retune(tune)?;
        }
        e.lanes.clear();
        e.retired.clear();
        for l in self.lanes.drain(..) {
            if e.remove.contains(&l.index) {
                e.retired.push(l);
            } else {
                e.lanes.push(l);
            }
        }
        e.lanes.append(&mut e.add);
        std::mem::swap(&mut self.lanes, &mut e.lanes);
        std::mem::swap(&mut self.channels_hz, &mut e.channels_hz);
        self.pending_flags |= ChunkFlags::CHANNEL_CHANGE;
        Ok(())
    }
}

/// A hot edit's per-lane part: the upstream sub-recipe staged once per channel index.
pub(crate) struct LaneEdit {
    staged: Vec<(u16, Staged)>,
}

/// What the control state takes on once a hot edit applied.
pub(crate) struct HopsCommit {
    up: Arc<Recipe>,
    down: Arc<Recipe>,
    up_shape: Shape,
}

/// A staged hot edit of a follow-hops pipeline.
pub(crate) struct HopsEdit {
    /// The running downstream sub-recipe (plan base).
    pub old_down: Arc<Recipe>,
    /// The draft's downstream sub-recipe.
    pub new_down: Arc<Recipe>,
    /// The per-lane stages.
    pub lanes: LaneEdit,
    /// Control-state update.
    pub commit: HopsCommit,
}

/// A channel set change handed to the pipeline thread.
pub(crate) struct ChannelEdit {
    add: Vec<Lane>,
    remove: Vec<u16>,
    /// Scratch, pre-sized (after the swap: the old, empty vector).
    lanes: Vec<Lane>,
    /// Scratch, pre-sized (after the swap: the removed lanes, dropped on the control side).
    retired: Vec<Lane>,
    /// The new channel table (after the swap: the old one).
    channels_hz: Vec<f64>,
    reply: SyncSender<ChannelDone>,
}

/// The pipeline thread's answer to a [`ChannelEdit`].
pub(crate) struct ChannelDone {
    edit: ChannelEdit,
    /// `Ok(sample)` applied there, or why the pipeline thread refused it (nothing changed).
    applied_at_sample: Result<u64, String>,
}

impl ChannelEdit {
    /// The answer, holding what was retired (or, refused, what was to be added).
    pub fn done(self, applied_at_sample: Result<u64, String>) {
        let reply = self.reply.clone();
        let _ = reply.try_send(ChannelDone {
            edit: self,
            applied_at_sample,
        });
    }
}

/// The control side of a follow-hops pipeline (in `PipelineCtl`).
pub(crate) struct HopsCtl {
    pub control: Mutex<HopsControl>,
    pub pending: Mutex<Option<ChannelEdit>>,
    pub edit_pending: AtomicBool,
}

pub(crate) struct HopsControl {
    up: Arc<Recipe>,
    down: Arc<Recipe>,
    up_shape: Shape,
    channels: Vec<ChannelInfo>,
    table: Vec<f64>,
    next_index: u16,
    cbw: f64,
    max_channels: usize,
    lane_input: PortInfo,
    source: &'static str,
    extra_slots: Vec<Slot>,
}

impl HopsCtl {
    /// `{channels, channel_source, max_channels}` for `GET /api/pipelines/{id}`.
    pub fn json(&self) -> Value {
        let c = lock(&self.control);
        json!({
            "channels": serde_json::to_value(&c.channels).unwrap_or(Value::Null),
            "channel_source": c.source,
            "channel_bandwidth_hz": c.cbw,
            "max_channels": c.max_channels,
        })
    }

    /// Stages `draft` (already id-checked) for every running lane plus its downstream half.
    pub fn stage_edit(
        &self,
        draft: &Recipe,
        registry: &Registry,
    ) -> Result<HopsEdit, RuntimeError> {
        let Split { up, down } = split(draft, registry)?;
        let (up, down) = (Arc::new(up), Arc::new(down));
        let c = lock(&self.control);
        let mut staged = Vec::with_capacity(c.channels.len());
        let mut up_shape = None;
        for ch in &c.channels {
            let st = graph::stage(
                Some((&c.up, &c.up_shape)),
                Arc::clone(&up),
                registry,
                c.lane_input,
            )?;
            up_shape.get_or_insert_with(|| st.shape.clone());
            staged.push((ch.index, st));
        }
        Ok(HopsEdit {
            old_down: Arc::clone(&c.down),
            new_down: down.clone(),
            lanes: LaneEdit { staged },
            commit: HopsCommit {
                up,
                down,
                up_shape: up_shape.unwrap_or_else(|| c.up_shape.clone()),
            },
        })
    }

    /// Takes on an applied edit.
    pub fn commit(&self, commit: HopsCommit) {
        let mut c = lock(&self.control);
        c.up = commit.up;
        c.down = commit.down;
        c.up_shape = commit.up_shape;
    }
}

/// Changes the channel set of follow-hops pipeline `ctl` to `requested` (Hz): keeps channels
/// within a quarter channel bandwidth of a requested one, adds the rest, removes the others.
/// Returns `{id, channels, added, removed, applied_at_sample}`.
pub(crate) fn set_channels(
    ctl: &PipelineCtl,
    registry: &Registry,
    counters: &Arc<Counters>,
    cfg: &ListenConfig,
    requested: &[f64],
    timeout: Duration,
) -> Result<Value, RuntimeError> {
    let ended = || RuntimeError::new(409, "ended", "the pipeline has ended");
    let hc = ctl.hops.as_ref().ok_or_else(|| {
        RuntimeError::new(422, "not_follow_hops", "the pipeline follows one channel")
    })?;
    if !ctl.running.load(Ordering::SeqCst) {
        return Err(ended());
    }
    let _serial = lock(&ctl.edit_lock);
    let shared = ctl.shared.upgrade().ok_or_else(ended)?;
    let (up, cbw, max, lane_input, current, mut table, next) = {
        let c = lock(&hc.control);
        (
            Arc::clone(&c.up),
            c.cbw,
            c.max_channels,
            c.lane_input,
            c.channels.clone(),
            c.table.clone(),
            c.next_index,
        )
    };
    let want = normalize(requested.iter().copied(), 0.5 * cbw, usize::MAX);
    if want.is_empty() {
        return Err(RuntimeError::new(
            422,
            "no_channels",
            "a follow-hops pipeline follows at least one channel",
        ));
    }
    if want.len() > max {
        return Err(RuntimeError::new(
            422,
            "too_many_channels",
            format!("the recipe follows at most {max} channels"),
        ));
    }
    let tol = 0.25 * cbw;
    let matched = |f: f64| current.iter().any(|ch| (f - ch.center_hz).abs() <= tol);
    let remove: Vec<u16> = current
        .iter()
        .filter(|ch| !want.iter().any(|f| (f - ch.center_hz).abs() <= tol))
        .map(|ch| ch.index)
        .collect();
    let add_hz: Vec<f64> = want.iter().copied().filter(|f| !matched(*f)).collect();
    let listing = |channels: &[ChannelInfo]| serde_json::to_value(channels).unwrap_or(Value::Null);
    if add_hz.is_empty() && remove.is_empty() {
        return Ok(
            json!({"id": ctl.id, "channels": listing(&current), "added": [],
                         "removed": [], "applied_at_sample": Value::Null}),
        );
    }
    // Before the first block has published the tune, each new lane is planned provisionally (as
    // at start) and re-planned for the running tune at the swap (`Hops::apply_channels`).
    let rate = super::runtime::planning_tune(&shared, want[0]).1;
    let slots = claim_extra(counters, cfg, add_hz.len(), rate)?;
    let mut add = Vec::with_capacity(add_hz.len());
    let mut index = next;
    for f in add_hz {
        let tune = super::runtime::planning_tune(&shared, f);
        let (lane, _, _) = build_lane(&up, registry, index, f, cbw, tune)?;
        if lane.graph.input != lane_input {
            return Err(unrealisable(
                "the new channel's input differs from the running channels' (retuned?)",
            ));
        }
        table.resize(usize::from(index) + 1, f64::NAN);
        table[usize::from(index)] = f;
        index = index
            .checked_add(1)
            .ok_or_else(|| unrealisable("channel indices exhausted"))?;
        add.push(lane);
    }
    let added: Vec<ChannelInfo> = add.iter().map(channel_info).collect();
    let total = current.len() + add.len();
    let (tx, rx) = mpsc::sync_channel(1);
    *lock(&hc.pending) = Some(ChannelEdit {
        add,
        lanes: Vec::with_capacity(total),
        retired: Vec::with_capacity(remove.len()),
        remove: remove.clone(),
        channels_hz: table.clone(),
        reply: tx,
    });
    hc.edit_pending.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + timeout;
    let done = loop {
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(d) => break d,
            Err(RecvTimeoutError::Timeout) => {
                let running = ctl.running.load(Ordering::SeqCst);
                if (!running || Instant::now() > deadline) && lock(&hc.pending).take().is_some() {
                    return Err(if running {
                        RuntimeError::new(
                            504,
                            "timeout",
                            "the pipeline did not reach a chunk boundary in time; nothing changed",
                        )
                    } else {
                        ended()
                    });
                }
            }
            Err(RecvTimeoutError::Disconnected) => return Err(ended()),
        }
    };
    let ChannelDone {
        edit,
        applied_at_sample,
    } = done;
    // Removed (or refused) lanes are dropped here, off the pipeline thread.
    drop(edit);
    let applied_at_sample = applied_at_sample.map_err(|m| {
        RuntimeError::new(
            409,
            "outside_window",
            format!("the pipeline retuned before the change applied; nothing changed ({m})"),
        )
    })?;
    let mut c = lock(&hc.control);
    c.extra_slots.extend(slots);
    for _ in 0..remove.len() {
        drop(c.extra_slots.pop());
    }
    c.channels.retain(|ch| !remove.contains(&ch.index));
    c.channels.extend(added.iter().cloned());
    c.table = table;
    c.next_index = index;
    Ok(json!({
        "id": ctl.id,
        "channels": listing(&c.channels),
        "added": listing(&added),
        "removed": remove,
        "applied_at_sample": applied_at_sample,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_blocks::{Block, BlockError, BlockFactory, BuildCtx, Io, ParamUpdate, Status};
    use hk_core::{Discontinuity, ProvenanceHandle};
    use hk_model::{ClockSource, Provenance, SampleTime, Timestamp, TimestampMethod, Tune};
    use hk_recipe::{BlockDescriptor, Params, PortSpec};

    /// What a probe saw per call: `(channel, items, mean power, flags)`.
    type Log = Arc<Mutex<Vec<(u16, usize, f64, ChunkFlags)>>>;

    struct ProbeFactory(BlockDescriptor, Log);

    impl BlockFactory for ProbeFactory {
        fn descriptor(&self) -> &BlockDescriptor {
            &self.0
        }
        fn build(&self, _: &Params, _: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
            Ok(Box::new(Probe(Arc::clone(&self.1))))
        }
    }

    /// iq → frames, emitting nothing: logs each chunk's meta and power.
    struct Probe(Log);

    impl Block for Probe {
        fn init(&mut self, _: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
            Ok(vec![PortInfo {
                ty: PortType::Frames,
                rate_hz: 10.0,
                max_items: 4,
                hold_items: 0,
            }])
        }
        fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
            let input = io.input(0)?;
            let (n, p) = match input.data {
                PortSlice::Iq(x) => (
                    x.len(),
                    x.iter().map(|z| f64::from(z.norm_sqr())).sum::<f64>() / x.len().max(1) as f64,
                ),
                _ => (0, 0.0),
            };
            let meta = input.meta;
            lock(&self.0).push((meta.channel, n, p, meta.flags));
            let out = io.output(0)?;
            out.meta = ChunkMeta {
                index: out.meta.index,
                ..meta
            };
            Ok(())
        }
        fn reset(&mut self) {}
        fn update_params(
            &mut self,
            _: &Params,
            _: &BuildCtx<'_>,
        ) -> Result<ParamUpdate, BlockError> {
            Ok(ParamUpdate::Applied)
        }
        fn status(&self) -> Status {
            Status::default()
        }
    }

    const CBW: f64 = 12_500.0;
    const FS: f64 = 1e6;

    struct Rig {
        registry: Registry,
        up: Arc<Recipe>,
        hops: Hops,
        down: Graph,
        log: Log,
    }

    /// A follow-hops pipeline (`test_probe` per channel → `follow_hops`) on `centers` at `tune`.
    fn rig(centers: &[f64], tune: (f64, f64)) -> Rig {
        let log: Log = Arc::default();
        let mut registry = Registry::builtin();
        registry
            .register(Arc::new(ProbeFactory(
                hk_blocks::schema::descriptor(
                    "test_probe",
                    "test",
                    "logs chunk meta",
                    vec![PortSpec::new("in", PortType::Iq)],
                    vec![PortSpec::new("out", PortType::Frames)],
                    vec![],
                    true,
                ),
                Arc::clone(&log),
            )))
            .unwrap();
        let recipe = crate::recipes::runtime::parse_recipe(json!({
            "schema": "hackriff.recipe", "schema_version": 2, "id": "probe-net", "version": 1,
            "name": "T-107 probe net",
            "input": {"port": "iq", "sample_rate_hz": 25000.0,
                      "channels": {"mode": "follow-hops", "channel_bandwidth_hz": CBW,
                                   "max_channels": 4}},
            "nodes": [{"id": "probe", "block": "test_probe"}, {"id": "hops", "block": "follow_hops"}],
            "outputs": [{"id": "frames", "kind": "inspector", "from": "hops"}],
            "output_policy": {"content_class": "unrestricted"}
        }))
        .unwrap();
        let Split { up, down } = split(&recipe, &registry).unwrap();
        let up = Arc::new(up);
        let lanes: Vec<Lane> = centers
            .iter()
            .enumerate()
            .map(|(k, &f)| {
                build_lane(&up, &registry, k as u16, f, CBW, tune)
                    .unwrap()
                    .0
            })
            .collect();
        let merge_port = PortInfo {
            ty: PortType::Frames,
            rate_hz: 40.0,
            max_items: 16,
            hold_items: 0,
        };
        let mut staged = graph::stage(None, Arc::new(down), &registry, merge_port).unwrap();
        let mut g = Graph::empty(merge_port);
        swap::apply(&mut g, &mut staged).unwrap();
        let hops = Hops {
            lanes,
            merged: FrameBuf::with_capacity(16, 1024),
            merge_port,
            channels_hz: centers.to_vec(),
            merge_index: 0,
            pending_flags: ChunkFlags::NONE,
            fs: tune.1,
        };
        Rig {
            registry,
            up,
            hops,
            down: g,
            log,
        }
    }

    fn provenance(tune: (f64, f64)) -> ProvenanceHandle {
        ProvenanceHandle::new(Provenance {
            device_id: "synthetic:t107".into(),
            tune: Tune {
                center_hz: tune.0,
                sample_rate_hz: tune.1,
                lna_db: 16.0,
                vga_db: 20.0,
                amp_on: false,
                bandwidth_hz: tune.1,
            },
            quantisation_limited: false,
            noise_sigma_lsb: None,
            overload: false,
            temperature_c: None,
            antenna_port: None,
            bias_tee: hk_model::BiasTee::Unknown,
            clock_source: ClockSource::Internal,
            clock_locked: true,
            calibration_state_ref: None,
            spur_mask_ref: None,
            timestamp_method: TimestampMethod::Synthetic,
            timestamp_error_budget_ns: Some(0),
            capture_artefacts: Vec::new(),
        })
    }

    /// Ring samples `[first, first + len)` carrying a tone at `tone_hz` under `prov`'s tune.
    fn chunk(
        prov: &ProvenanceHandle,
        first: u64,
        len: usize,
        tone_hz: f64,
    ) -> (ReadChunk, Vec<Complex<i8>>) {
        let f = tone_hz - prov.tune.center_hz;
        let x = (0..len as u64)
            .map(|i| {
                let ph = std::f64::consts::TAU * f * (first + i) as f64 / prov.tune.sample_rate_hz;
                Complex::new(
                    (60.0 * ph.cos()).round() as i8,
                    (60.0 * ph.sin()).round() as i8,
                )
            })
            .collect();
        let c = ReadChunk {
            time: SampleTime {
                sample_index: first,
                host_time: Timestamp::from_unix_nanos(first as i64 * 1_000),
            },
            len,
            block_start: false,
            discontinuity: Discontinuity::NONE,
            dropped_before: 0,
            provenance: prov.clone(),
        };
        (c, x)
    }

    fn mean_power(log: &Log, ch: u16) -> f64 {
        let log = lock(log);
        let p: Vec<f64> = log
            .iter()
            .filter(|e| e.0 == ch && e.1 > 0)
            .map(|e| e.2)
            .collect();
        p.iter().sum::<f64>() / p.len().max(1) as f64
    }

    /// T-107: a channel added by `set_channels` is built at the tune read then; when a same-rate
    /// retune reaches the pipeline thread before the swap, the swap re-plans the new lane for the
    /// running tune, so it decodes its own frequency, not one shifted by the retune.
    #[test]
    fn a_retune_racing_set_channels_never_decodes_the_wrong_frequency() {
        let t0 = (100.0e6, FS);
        let t1 = (100.05e6, FS);
        let (a, b) = (100.1e6, 100.2e6);
        let mut r = rig(&[a], t0);
        // Control side: the new lane is built at the tune it read (t0)…
        let (lane, _, _) = build_lane(&r.up, &r.registry, 1, b, CBW, t0).unwrap();
        let (mut stale, _, _) = build_lane(&r.up, &r.registry, 2, b, CBW, t0).unwrap();
        let (tx, _rx) = mpsc::sync_channel(1);
        let mut edit = ChannelEdit {
            add: vec![lane],
            remove: vec![],
            lanes: Vec::with_capacity(2),
            retired: Vec::new(),
            channels_hz: vec![a, b],
            reply: tx.clone(),
        };
        // …while the pipeline thread already follows a retune to t1, then reaches the boundary.
        r.hops.retune(t1).unwrap();
        r.hops.apply_channels(&mut edit, t1).unwrap();
        assert_eq!(r.hops.lanes.len(), 2);
        for l in &r.hops.lanes {
            assert_eq!(l.tune, t1, "channel {}", l.index);
            let want = l.center_hz - t1.0;
            let got = l.ddc.spec().center_offset_hz;
            assert!(
                (got - want).abs() < 1e-6,
                "channel {}: offset {got}, want {want}",
                l.index
            );
        }
        // A tone on channel b under t1 is heard on channel 1 (b), not on channel 0 (a).
        let prov = provenance(t1);
        for k in 0..4 {
            let (c, x) = chunk(&prov, k * 50_000, 50_000, b);
            r.hops
                .process(&c, &x, ChunkFlags::NONE, &mut r.down)
                .unwrap();
        }
        let (on, off) = (mean_power(&r.log, 1), mean_power(&r.log, 0));
        assert!(on > 0.05, "channel b hears its tone: {on}");
        assert!(on > 100.0 * off, "channel a does not: {on} vs {off}");
        // The lane as built (left at t0) sits 50 kHz off its channel: the tone is gone there.
        let (c, x) = chunk(&prov, 0, 50_000, b);
        let out = stale.ddc.process(InputInfo::from(&c), &x).unwrap();
        let p = out
            .samples
            .iter()
            .map(|z| f64::from(z.norm_sqr()))
            .sum::<f64>()
            / out.samples.len().max(1) as f64;
        assert!(
            on > 100.0 * p,
            "an unplanned lane decodes elsewhere: {on} vs {p}"
        );

        // A channel the running tune cannot reach refuses the whole change: nothing moves.
        let far_hz = 100.6e6;
        let (far, _, _) = build_lane(&r.up, &r.registry, 3, far_hz, CBW, (100.4e6, FS)).unwrap();
        let mut refused = ChannelEdit {
            add: vec![far],
            remove: vec![0],
            lanes: Vec::with_capacity(2),
            retired: Vec::new(),
            channels_hz: vec![b, far_hz],
            reply: tx,
        };
        assert!(r.hops.apply_channels(&mut refused, t1).is_err());
        let idx: Vec<u16> = r.hops.lanes.iter().map(|l| l.index).collect();
        assert_eq!(idx, [0, 1]);
        assert_eq!(r.hops.channels_hz, [a, b]);
    }

    /// T-107: a `DISCONTINUITY` on a chunk too short for a lane's DDC to yield a sample reaches
    /// that lane's graph with its next samples, once.
    #[test]
    fn a_discontinuity_on_a_chunk_without_channel_samples_is_delivered_with_the_next() {
        let tune = (100.0e6, FS);
        let mut r = rig(&[100.1e6], tune);
        let prov = provenance(tune);
        let (c, x) = chunk(&prov, 0, 40_000, 100.1e6);
        r.hops
            .process(&c, &x, ChunkFlags::NONE, &mut r.down)
            .unwrap();
        lock(&r.log).clear();
        // 5 samples at a 40:1 decimation: no channel sample, and the flag must not be lost.
        let (c, x) = chunk(&prov, 40_000, 5, 100.1e6);
        r.hops
            .process(&c, &x, ChunkFlags::DISCONTINUITY, &mut r.down)
            .unwrap();
        {
            let log = lock(&r.log);
            assert!(
                log.iter().all(|e| e.1 == 0),
                "precondition: the short chunk yields no channel samples: {log:?}"
            );
        }
        lock(&r.log).clear();
        let (c, x) = chunk(&prov, 40_005, 40_000, 100.1e6);
        r.hops
            .process(&c, &x, ChunkFlags::NONE, &mut r.down)
            .unwrap();
        let log = lock(&r.log);
        let first = log.iter().find(|e| e.1 > 0).expect("the lane ran");
        assert!(
            first.3.contains(ChunkFlags::DISCONTINUITY),
            "the held discontinuity is delivered: {log:?}"
        );
        let n = log
            .iter()
            .filter(|e| e.3.contains(ChunkFlags::DISCONTINUITY))
            .count();
        assert_eq!(n, 1, "exactly once: {log:?}");
    }

    #[test]
    fn normalize_merges_close_channels_caps_by_rank_and_sorts() {
        let hz = normalize(
            [152.0e6, 151.0e6, 152.004e6, f64::NAN, 150.0e6, 149.0e6],
            6250.0,
            3,
        );
        assert_eq!(hz, vec![150.0e6, 151.0e6, 152.0e6]);
    }

    #[test]
    fn the_pocsag_tutorial_recipe_splits_at_its_merge_node() {
        let doc: Value =
            serde_json::from_str(include_str!("../../../../recipes/pocsag.recipe.json")).unwrap();
        let recipe = crate::recipes::runtime::parse_recipe(doc).unwrap();
        let s = split(&recipe, &Registry::builtin()).unwrap();
        let ids = |r: &Recipe| r.nodes.iter().map(|n| n.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&s.up), ["fsk", "clock", "slice", "sync", "bch", "msg"]);
        assert_eq!(ids(&s.down), ["hops", "page"]);
        assert_eq!(s.up.outputs[0].from, "msg.out");
        assert_eq!(s.down.nodes[0].inputs["in"], "input");
        assert_eq!(s.down.nodes[1].inputs["in"], "hops.out");
        assert_eq!(s.down.input.port, PortType::Frames);
        assert!(s.up.refine.is_none() && s.down.refine.is_none());
        let reg = Registry::builtin();
        s.up.validate(&reg).unwrap();
        s.down.validate(&reg).unwrap();
    }
}
