//! The recipe schema (ADR-0011 §2): a decoder as data.

use std::collections::{BTreeMap, BTreeSet};

use hk_model::ContentClass;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::fields::{Display, FieldMap, is_field_path};
use crate::param::{BlockDescriptor, Catalogue, ParamSchema, Params};
use crate::port::PortType;
use crate::{RECIPE_SCHEMA, RECIPE_SCHEMA_VERSION, is_id};

/// Block kind of the multi-channel merge point (ADR-0011 §2.5).
pub const FOLLOW_HOPS_BLOCK: &str = "follow_hops";

/// A recipe document (`recipes/<id>.recipe.json`, or a saved version in the data directory).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    /// `"hackriff.recipe"`.
    pub schema: String,
    /// Format version (1).
    pub schema_version: u32,
    /// Stable id ([`is_id`]); names the recipe across versions.
    pub id: String,
    /// Saved version, ≥ 1, monotonic per id. A saved version is immutable.
    pub version: u32,
    /// Display name.
    pub name: String,
    /// Description.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Hints for ranking this recipe against a blind-detected signal. Never a tuning source.
    #[serde(default, rename = "match")]
    pub match_hints: MatchHints,
    /// What the recipe consumes.
    pub input: InputSpec,
    /// Block nodes. Without explicit `inputs` a node reads the previous node (the recipe input
    /// for the first), so a plain list is a linear chain; explicit `inputs` make a DAG.
    pub nodes: Vec<NodeSpec>,
    /// Parser field maps by id, referenced by `fields` nodes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub field_maps: BTreeMap<String, FieldMap>,
    /// Named output streams.
    pub outputs: Vec<OutputSpec>,
    /// Content ceiling and metadata allowlist of everything this recipe emits.
    pub output_policy: OutputPolicy,
    /// Output-driven refinement of the channel (CLAUDE.md "tune from the processed output").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refine: Option<RefineSpec>,
}

/// Match hints: blind-first. The runtime ranks recipes for a detected emitter by how well its
/// *measured* parameters fit these; it never tunes to `freq_hz` or runs a recipe unasked.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchHints {
    /// Estimated families (`wfm`, `nbfm`, `am`, `fsk`, `msk`, `ook`, `bpsk`, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub families: Vec<String>,
    /// Frequency ranges where the signal is usually found, Hz (a prior, ranked, never a tune).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub freq_hz: Vec<[f64; 2]>,
    /// Occupied bandwidth range, Hz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bandwidth_hz: Option<[f64; 2]>,
    /// Symbol-rate range, Bd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol_rate_bd: Option<[f64; 2]>,
    /// Bursty (`true`) or continuous (`false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bursty: Option<bool>,
    /// Measured features that raise the rank (`pilot-19k`, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
}

/// What the recipe consumes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputSpec {
    /// Port type of the recipe input. `iq` runs on a live or recorded channel the runtime
    /// down-converts; `bits`/`soft`/`frames` run a recipe tail over a recorded decoded stream.
    pub port: PortType,
    /// Channel sample rate the runtime delivers (`iq`/`real`), Hz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate_hz: Option<f64>,
    /// Channel bandwidth, Hz (the target's estimated or refined bandwidth when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bandwidth_hz: Option<f64>,
    /// One channel, or several followed by one pipeline.
    #[serde(default, skip_serializing_if = "is_single")]
    pub channels: ChannelsSpec,
}

/// Channel topology.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case")]
pub enum ChannelsSpec {
    /// One channel.
    #[default]
    Single,
    /// Several channels (a multi-channel pager net, a hopper). Everything upstream of the
    /// `follow_hops` node is instantiated once per channel; frames merge there, tagged with
    /// their channel (ADR-0011 §2.5).
    FollowHops {
        /// Band the channels lie in, Hz (default: the target's extent).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        band_hz: Option<[f64; 2]>,
        /// Per-channel bandwidth, Hz.
        channel_bandwidth_hz: f64,
        /// Most channels followed at once.
        max_channels: u16,
        /// Channels come from blind detections in the band (default) or an explicit list.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        list_hz: Vec<f64>,
    },
}

/// One block node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeSpec {
    /// Node id ([`is_id`]), unique in the recipe; also the stage-stream name.
    pub id: String,
    /// Block kind from the catalogue.
    pub block: String,
    /// Pins the block's contract version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// Display label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Parameters.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub params: Params,
    /// Input port → source ([`PortRef`]: `input`, `node` or `node.port`). Empty: the block's
    /// single input reads the previous node's single output.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub inputs: BTreeMap<String, String>,
}

/// A reference to an output port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortRef<'a> {
    /// The recipe input.
    Input,
    /// A node output; `port: None` is the node's single (non-diagnostic) output.
    Node {
        /// Node id.
        node: &'a str,
        /// Port name.
        port: Option<&'a str>,
    },
}

impl<'a> PortRef<'a> {
    /// Parses `input`, `node` or `node.port`.
    pub fn parse(s: &'a str) -> Option<Self> {
        if s == "input" {
            return Some(PortRef::Input);
        }
        match s.split_once('.') {
            None if is_id(s) => Some(PortRef::Node {
                node: s,
                port: None,
            }),
            Some((n, p)) if is_id(n) && is_id(p) => Some(PortRef::Node {
                node: n,
                port: Some(p),
            }),
            _ => None,
        }
    }
}

/// Output stream kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutputKind {
    /// Frames with layer trees for the packet inspector (`docs/stream-contract.md` §14).
    Inspector,
    /// Decode messages (§5.1) mapped from parsed fields; also stored as Decode rows.
    Messages,
    /// A named default stage stream (any port is tappable on demand without declaring it).
    Stage,
}

/// Stage-stream rendering, reduced server-side (the UI is a thin client).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StageView {
    /// The port's elements as a binary stream (§14.4).
    #[default]
    Raw,
    /// Power-spectrum rows of an `iq`/`real` port (≤ 25 rows/s).
    Spectrum,
}

/// A named output.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSpec {
    /// Output id ([`is_id`]); stream ids are `inspector/<pipeline>/<id>` etc.
    pub id: String,
    /// Kind.
    pub kind: OutputKind,
    /// Source port ([`PortRef`]).
    pub from: String,
    /// `stage` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<StageView>,
    /// `messages` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decode: Option<DecodeMapping>,
}

/// Parsed fields → Decode message (docs/07 §2.15).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecodeMapping {
    /// `frame_model` of every message (a token).
    pub frame_model: String,
    /// Identity naming the emitter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<IdentityMapping>,
    /// Field paths copied into `metadata` (flat; key = last path segment unless duplicated).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metadata: Vec<String>,
    /// Field paths copied into `content`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<String>,
    /// Emit only frames where every listed field is present (default: any mapped field).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub require: Vec<String>,
    /// Decoder evidence for the family step (T-111): a service family or decoder label the
    /// family vocabulary maps (e.g. `adsb`, `fm-broadcast`, `rds`); the recipe id when absent.
    /// Emitters the decodes' identities resolve to get the mapped family as a Classification,
    /// exactly as plugin decodes do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
}

/// Identity from a field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityMapping {
    /// Scheme, e.g. `rds-pi`, `pocsag-ric`, `adsb-icao`.
    pub scheme: String,
    /// Field path.
    pub field: String,
    /// Rendering (`hex` or `dec`).
    #[serde(default = "dec")]
    pub format: Display,
}

impl IdentityMapping {
    /// The Emitter identity scheme: a known scheme (`adsb-icao`, `rds-pi`, …, `other:<name>`),
    /// or `other:<scheme>` for any other token-shaped name such as `pocsag-ric`; `None` when the
    /// name is neither.
    pub fn identity_scheme(&self) -> Option<hk_model::IdentityScheme> {
        self.scheme.parse().ok().or_else(|| {
            (hk_stream::policy::is_token(&self.scheme) && !self.scheme.contains(':'))
                .then(|| hk_model::IdentityScheme::Other(self.scheme.clone()))
        })
    }
}

fn dec() -> Display {
    Display::Dec
}

fn is_single(c: &ChannelsSpec) -> bool {
    matches!(c, ChannelsSpec::Single)
}

/// Content ceiling of the recipe's outputs; same shape and rules as a plugin manifest's
/// `output` (`docs/stream-contract.md` §9.3). The effective class is
/// `clamp(content_class, source class)`: a recipe can restrict itself, never upgrade.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputPolicy {
    /// Ceiling class.
    pub content_class: ContentClass,
    /// Metadata allowlist, §9.3 shape; required when the class forbids content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_keys: Option<BTreeMap<String, Value>>,
    /// Allowed frame models under a restricted class.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub frame_models: Vec<String>,
    /// Allowed identity shape, §9.3 shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<Value>,
}

/// Output-driven refinement.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefineSpec {
    /// Objective read from a node's status.
    pub objective: RefineObjective,
    /// Channel parameters tuned: `center_hz`, `bandwidth_hz`.
    pub tune: Vec<String>,
}

/// A status metric to optimise.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefineObjective {
    /// Node id.
    pub node: String,
    /// Status key: `error_rate`, `quality`, `snr_db` or `lock`.
    pub metric: String,
    /// Direction.
    pub goal: RefineGoal,
}

/// Optimisation direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RefineGoal {
    /// Minimise.
    Min,
    /// Maximise.
    Max,
}

/// A recipe validation error. Messages never echo raw values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecipeError {
    /// JSON-pointer-like location, e.g. `nodes[3].params.poly`, `field_maps.rds_group.pi`.
    pub path: String,
    /// What is wrong.
    pub message: String,
}

impl std::fmt::Display for RecipeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// Where an edge comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// The recipe input.
    Input,
    /// A node output port.
    Node {
        /// Node id.
        node: String,
        /// Port name.
        port: String,
    },
}

/// A resolved connection into a node input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    /// Consuming node.
    pub node: String,
    /// Its input port.
    pub port: String,
    /// Source.
    pub from: Endpoint,
    /// Port type flowing on the edge.
    pub ty: PortType,
}

/// A recipe resolved against a catalogue: every edge typed, nodes in topological order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// Node indexes in an order where every node follows its sources.
    pub order: Vec<usize>,
    /// Every node input, resolved, in `order`.
    pub edges: Vec<Edge>,
    /// Non-fatal findings (unpinned placeholder parameters).
    pub warnings: Vec<RecipeError>,
}

struct Errors(Vec<RecipeError>);

impl Errors {
    fn push(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.0.push(RecipeError {
            path: path.into(),
            message: message.into(),
        });
    }
}

/// Resolved output port types per node.
type OutTypes = Vec<Option<BTreeMap<String, PortType>>>;

impl Recipe {
    /// The implicit or explicit sources of node `i`'s inputs: `(input port, or None for the
    /// implicit single input; reference)`.
    fn node_sources(&self, i: usize) -> Vec<(Option<&str>, String)> {
        let n = &self.nodes[i];
        if n.inputs.is_empty() {
            let prev = if i == 0 {
                "input".to_owned()
            } else {
                self.nodes[i - 1].id.clone()
            };
            vec![(None, prev)]
        } else {
            n.inputs
                .iter()
                .map(|(p, r)| (Some(p.as_str()), r.clone()))
                .collect()
        }
    }

    /// Validation that needs no block catalogue: schema, ids, references, acyclicity, field
    /// maps, output kinds, `follow_hops` placement, content policy, refinement target.
    pub fn validate_structure(&self) -> Result<(), Vec<RecipeError>> {
        let mut e = Errors(Vec::new());
        self.structure(&mut e);
        if e.0.is_empty() { Ok(()) } else { Err(e.0) }
    }

    fn structure(&self, e: &mut Errors) -> Option<Vec<usize>> {
        if self.schema != RECIPE_SCHEMA {
            e.push("schema", format!("expected \"{RECIPE_SCHEMA}\""));
        }
        if self.schema_version != RECIPE_SCHEMA_VERSION {
            e.push("schema_version", "unsupported recipe format version");
        }
        if !is_id(&self.id) {
            e.push("id", "ids are [a-z0-9_-]{1,64}");
        }
        if self.version == 0 {
            e.push("version", "versions start at 1");
        }
        if self.name.trim().is_empty() {
            e.push("name", "a name is required");
        }
        for (key, v) in [
            ("input.sample_rate_hz", self.input.sample_rate_hz),
            ("input.bandwidth_hz", self.input.bandwidth_hz),
        ] {
            if v.is_some_and(|r| !(r.is_finite() && r > 0.0)) {
                e.push(key, "must be positive");
            }
        }
        if self.nodes.is_empty() {
            e.push("nodes", "a recipe needs at least one node");
        }
        let mut index = BTreeMap::new();
        for (i, n) in self.nodes.iter().enumerate() {
            if !is_id(&n.id) || n.id == "input" {
                e.push(
                    format!("nodes[{i}].id"),
                    "ids are [a-z0-9_-]{1,64}, not `input`",
                );
            }
            if index.insert(n.id.as_str(), i).is_some() {
                e.push(format!("nodes[{i}].id"), "duplicate node id");
            }
        }
        // Node-level graph for acyclicity.
        let mut preds: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); self.nodes.len()];
        for (i, pred) in preds.iter_mut().enumerate() {
            for (port, r) in self.node_sources(i) {
                let path = match port {
                    Some(p) => format!("nodes[{i}].inputs.{p}"),
                    None => format!("nodes[{i}]"),
                };
                match PortRef::parse(&r) {
                    None => e.push(path, "expected `input`, `node` or `node.port`"),
                    Some(PortRef::Input) => {}
                    Some(PortRef::Node { node, .. }) => match index.get(node) {
                        None => e.push(path, "unknown source node"),
                        Some(&j) if j == i => e.push(path, "a node cannot read itself"),
                        Some(&j) => {
                            pred.insert(j);
                        }
                    },
                }
            }
        }
        let order = topological(&preds);
        if order.is_none() {
            e.push("nodes", "the node graph has a cycle");
        }
        for (id, map) in &self.field_maps {
            if !is_id(id) {
                e.push(format!("field_maps.{id}"), "ids are [a-z0-9_-]{1,64}");
            }
            if let Err(errs) = map.validate() {
                for fe in errs {
                    e.push(format!("field_maps.{id}.{}", fe.path), fe.message);
                }
            }
        }
        let hops = self
            .nodes
            .iter()
            .filter(|n| n.block == FOLLOW_HOPS_BLOCK)
            .count();
        match &self.input.channels {
            ChannelsSpec::Single if hops > 0 => e.push(
                "nodes",
                "follow_hops needs input.channels.mode = follow-hops",
            ),
            ChannelsSpec::FollowHops {
                max_channels,
                channel_bandwidth_hz,
                ..
            } => {
                if hops != 1 {
                    e.push(
                        "nodes",
                        "follow-hops recipes have exactly one follow_hops node",
                    );
                }
                if *max_channels == 0 {
                    e.push("input.channels.max_channels", "must be at least 1");
                }
                if !(channel_bandwidth_hz.is_finite() && *channel_bandwidth_hz > 0.0) {
                    e.push("input.channels.channel_bandwidth_hz", "must be positive");
                }
            }
            ChannelsSpec::Single => {}
        }
        let mut out_ids = BTreeSet::new();
        for (i, o) in self.outputs.iter().enumerate() {
            let path = format!("outputs[{i}]");
            if !is_id(&o.id) || !out_ids.insert(o.id.as_str()) {
                e.push(
                    format!("{path}.id"),
                    "output ids are unique [a-z0-9_-]{1,64}",
                );
            }
            match PortRef::parse(&o.from) {
                Some(PortRef::Node { node, .. }) if index.contains_key(node) => {}
                Some(PortRef::Input) => {}
                _ => e.push(format!("{path}.from"), "unknown source port"),
            }
            if o.view.is_some() && o.kind != OutputKind::Stage {
                e.push(format!("{path}.view"), "only stage outputs have a view");
            }
            match (&o.decode, o.kind) {
                (Some(d), OutputKind::Messages) => {
                    if !hk_stream::policy::is_token(&d.frame_model) {
                        e.push(format!("{path}.decode.frame_model"), "must be a token");
                    }
                    let paths = d
                        .identity
                        .iter()
                        .map(|i| &i.field)
                        .chain(&d.metadata)
                        .chain(&d.content)
                        .chain(&d.require);
                    for p in paths {
                        if !is_field_path(p) {
                            e.push(format!("{path}.decode"), "field paths are dotted names");
                        }
                    }
                    if let Some(i) = &d.identity {
                        if i.identity_scheme().is_none() {
                            e.push(
                                format!("{path}.decode.identity.scheme"),
                                "identity scheme is a known scheme or a token",
                            );
                        }
                        if !matches!(i.format, Display::Hex | Display::Dec) {
                            e.push(
                                format!("{path}.decode.identity.format"),
                                "identity format is hex or dec",
                            );
                        }
                    }
                    if d.identity.is_none() && d.metadata.is_empty() && d.content.is_empty() {
                        e.push(
                            format!("{path}.decode"),
                            "a decode mapping maps at least one field",
                        );
                    }
                    if d.service
                        .as_deref()
                        .is_some_and(|s| !hk_stream::policy::is_token(s))
                    {
                        e.push(format!("{path}.decode.service"), "must be a token");
                    }
                }
                (None, OutputKind::Messages) => e.push(
                    format!("{path}.decode"),
                    "messages outputs need a decode mapping",
                ),
                (Some(_), _) => e.push(format!("{path}.decode"), "only messages outputs decode"),
                (None, _) => {}
            }
        }
        if self.outputs.is_empty() {
            e.push("outputs", "a recipe needs at least one output");
        }
        if !self.output_policy.content_class.permits_content()
            && self.output_policy.metadata_keys.is_none()
        {
            e.push(
                "output_policy.metadata_keys",
                "a class that forbids content must declare its metadata allowlist (may be empty)",
            );
        }
        if let Some(r) = &self.refine {
            if !index.contains_key(r.objective.node.as_str()) {
                e.push("refine.objective.node", "unknown node");
            }
            if !["error_rate", "quality", "snr_db", "lock"].contains(&r.objective.metric.as_str()) {
                e.push(
                    "refine.objective.metric",
                    "one of error_rate, quality, snr_db, lock",
                );
            }
            if r.tune.is_empty()
                || r.tune
                    .iter()
                    .any(|t| t != "center_hz" && t != "bandwidth_hz")
            {
                e.push("refine.tune", "center_hz and/or bandwidth_hz");
            }
        }
        order
    }

    /// Full validation against a block catalogue: [`Self::validate_structure`] plus block
    /// kinds and version pins, parameters against each block's schema, input wiring and port
    /// types (a polymorphic output takes its block's input type), and output port types.
    /// Returns the typed graph.
    pub fn validate(&self, catalogue: &dyn Catalogue) -> Result<Resolved, Vec<RecipeError>> {
        let mut e = Errors(Vec::new());
        let order = self.structure(&mut e);
        let mut warnings = Errors(Vec::new());
        let maps: Vec<&str> = self.field_maps.keys().map(String::as_str).collect();
        let descriptors: Vec<Option<&BlockDescriptor>> = self
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let d = catalogue.descriptor(&n.block);
                match d {
                    None => e.push(format!("nodes[{i}].block"), "unknown block"),
                    Some(d) => {
                        if n.version.is_some_and(|v| v != d.version) {
                            e.push(format!("nodes[{i}].version"), "block version mismatch");
                        }
                        if d.params_pinned {
                            for pe in ParamSchema::validate_all(&d.params, &n.params, &maps) {
                                e.push(format!("nodes[{i}].params.{}", pe.path), pe.message);
                            }
                        } else if !n.params.is_empty() {
                            warnings.push(
                                format!("nodes[{i}].params"),
                                "block parameters not pinned yet; accepted unchecked",
                            );
                        }
                    }
                }
                d
            })
            .collect();
        let Some(order) = order else { return Err(e.0) };
        let index: BTreeMap<&str, usize> = self
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id.as_str(), i))
            .collect();
        // Output port types per node, filled in topological order so a polymorphic output
        // (several types = "same as the input") can take its input's type.
        let mut out_types: OutTypes = vec![None; self.nodes.len()];
        let mut edges = Vec::new();
        for &i in &order {
            let n = &self.nodes[i];
            let Some(d) = descriptors[i] else { continue };
            let mut connected = BTreeSet::new();
            let mut first_input = None;
            for (port, r) in self.node_sources(i) {
                let (path, spec) = match port {
                    Some(p) => (format!("nodes[{i}].inputs.{p}"), d.input(p)),
                    None => match d.inputs.as_slice() {
                        [only] => (format!("nodes[{i}]"), Some(only)),
                        [] => {
                            e.push(format!("nodes[{i}]"), "block has no inputs");
                            continue;
                        }
                        _ => {
                            e.push(
                                format!("nodes[{i}].inputs"),
                                "block has several inputs; wire them explicitly",
                            );
                            continue;
                        }
                    },
                };
                let Some(spec) = spec else {
                    e.push(path, "block has no such input port");
                    continue;
                };
                connected.insert(spec.name.as_str());
                match self.source(&r, &index, &descriptors, &out_types) {
                    Err(m) => e.push(path, m),
                    Ok((from, ty)) => {
                        if !spec.types.contains(&ty) {
                            e.push(
                                path,
                                format!("port type {ty} not accepted (expects {:?})", spec.types),
                            );
                        }
                        first_input.get_or_insert(ty);
                        edges.push(Edge {
                            node: n.id.clone(),
                            port: spec.name.clone(),
                            from,
                            ty,
                        });
                    }
                }
            }
            for p in &d.inputs {
                if !connected.contains(p.name.as_str()) {
                    e.push(
                        format!("nodes[{i}].inputs.{}", p.name),
                        "input not connected",
                    );
                }
            }
            let mut types = BTreeMap::new();
            for p in &d.outputs {
                let ty = match (p.types.as_slice(), first_input) {
                    ([], _) => {
                        e.push(format!("nodes[{i}]"), "block output port has no type");
                        continue;
                    }
                    ([only], _) => *only,
                    (many, Some(t)) if many.contains(&t) => t,
                    (many, _) => {
                        e.push(
                            format!("nodes[{i}]"),
                            format!("output {} cannot carry the input type", p.name),
                        );
                        many[0]
                    }
                };
                types.insert(p.name.clone(), ty);
            }
            out_types[i] = Some(types);
        }
        for (i, o) in self.outputs.iter().enumerate() {
            let Ok((_, ty)) = self.source(&o.from, &index, &descriptors, &out_types) else {
                e.push(format!("outputs[{i}].from"), "unresolvable source port");
                continue;
            };
            if matches!(o.kind, OutputKind::Inspector | OutputKind::Messages)
                && ty != PortType::Frames
            {
                e.push(
                    format!("outputs[{i}].from"),
                    "inspector and messages read frames",
                );
            }
            if o.view == Some(StageView::Spectrum) && !matches!(ty, PortType::Iq | PortType::Real) {
                e.push(
                    format!("outputs[{i}].view"),
                    "spectrum views need iq or real",
                );
            }
        }
        if e.0.is_empty() {
            Ok(Resolved {
                order,
                edges,
                warnings: warnings.0,
            })
        } else {
            Err(e.0)
        }
    }

    /// Endpoint and type of an output reference, given the types resolved so far.
    fn source(
        &self,
        r: &str,
        index: &BTreeMap<&str, usize>,
        descriptors: &[Option<&BlockDescriptor>],
        out_types: &OutTypes,
    ) -> Result<(Endpoint, PortType), &'static str> {
        match PortRef::parse(r) {
            None => Err("bad reference"),
            Some(PortRef::Input) => Ok((Endpoint::Input, self.input.port)),
            Some(PortRef::Node { node, port }) => {
                let &j = index.get(node).ok_or("unknown source node")?;
                let d = descriptors[j].ok_or("source block unknown")?;
                let types = out_types[j].as_ref().ok_or("source not resolved")?;
                let name = match port {
                    Some(p) => d
                        .output(p)
                        .ok_or("source has no such output port")?
                        .name
                        .as_str(),
                    None => {
                        let mut main = d.outputs.iter().filter(|p| !p.diagnostic);
                        match (main.next(), main.next()) {
                            (Some(p), None) => p.name.as_str(),
                            _ => return Err("source has several outputs; name the port"),
                        }
                    }
                };
                let ty = *types.get(name).ok_or("source port has no type")?;
                Ok((
                    Endpoint::Node {
                        node: node.to_owned(),
                        port: name.to_owned(),
                    },
                    ty,
                ))
            }
        }
    }
}

/// Kahn's algorithm over predecessor sets; `None` on a cycle. Ties keep document order.
fn topological(preds: &[BTreeSet<usize>]) -> Option<Vec<usize>> {
    let n = preds.len();
    let mut remaining: Vec<usize> = preds.iter().map(BTreeSet::len).collect();
    let mut done = vec![false; n];
    let mut order = Vec::with_capacity(n);
    while order.len() < n {
        let next = (0..n).find(|&i| !done[i] && remaining[i] == 0)?;
        done[next] = true;
        order.push(next);
        for (i, p) in preds.iter().enumerate() {
            if p.contains(&next) {
                remaining[i] -= 1;
            }
        }
    }
    Some(order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::param::PortSpec;
    use serde_json::json;

    fn minimal() -> Value {
        json!({
            "schema": "hackriff.recipe", "schema_version": 2, "id": "t", "version": 1,
            "name": "T", "input": {"port": "bits"},
            "nodes": [{"id": "a", "block": "identity"}, {"id": "b", "block": "identity"}],
            "outputs": [{"id": "s", "kind": "stage", "from": "b"}],
            "output_policy": {"content_class": "unrestricted"}
        })
    }

    fn catalogue() -> Vec<BlockDescriptor> {
        let d = |name: &str, i: PortSpec, o: PortSpec| BlockDescriptor {
            name: name.into(),
            version: 1,
            group: "util".into(),
            doc: String::new(),
            inputs: vec![i],
            outputs: vec![o],
            params: vec![],
            params_pinned: true,
        };
        vec![
            d(
                "identity",
                PortSpec::any_of("in", &PortType::ALL),
                PortSpec::any_of("out", &PortType::ALL),
            ),
            d(
                "slicer",
                PortSpec::new("in", PortType::Soft),
                PortSpec::new("out", PortType::Bits),
            ),
        ]
    }

    #[test]
    fn unknown_fields_are_errors() {
        let mut v = minimal();
        v["nodse"] = json!([]);
        assert!(serde_json::from_value::<Recipe>(v).is_err());
    }

    #[test]
    fn cycles_and_bad_references_are_reported() {
        let mut v = minimal();
        v["nodes"][0]["inputs"] = json!({"in": "b"});
        v["outputs"][0]["from"] = "nope.out".into();
        let r: Recipe = serde_json::from_value(v).unwrap();
        let errors = r.validate_structure().unwrap_err();
        assert!(
            errors.iter().any(|e| e.message.contains("cycle")),
            "{errors:?}"
        );
        assert!(
            errors.iter().any(|e| e.path == "outputs[0].from"),
            "{errors:?}"
        );
    }

    #[test]
    fn polymorphic_outputs_follow_the_input_and_types_are_checked() {
        let r: Recipe = serde_json::from_value(minimal()).unwrap();
        let resolved = r.validate(&catalogue()).unwrap();
        assert!(resolved.edges.iter().all(|e| e.ty == PortType::Bits));

        let mut v = minimal();
        v["nodes"][1]["block"] = "slicer".into();
        let r: Recipe = serde_json::from_value(v).unwrap();
        let errors = r.validate(&catalogue()).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.path == "nodes[1]" && e.message.contains("bits")),
            "{errors:?}"
        );
    }

    /// T-111: the `messages` decode mapping (identity scheme, format, at least one field,
    /// `service` token) is validated; unknown token schemes resolve to `other:<scheme>`.
    #[test]
    fn decode_mappings_are_validated() {
        let with = |decode: Value| {
            let mut v = minimal();
            v["outputs"] = json!([{"id": "m", "kind": "messages", "from": "b", "decode": decode}]);
            serde_json::from_value::<Recipe>(v).unwrap()
        };
        let paths = |r: &Recipe| -> Vec<String> {
            r.validate_structure()
                .err()
                .unwrap_or_default()
                .into_iter()
                .map(|e| e.path)
                .collect()
        };
        let ok = with(json!({"frame_model": "adsb-es", "service": "adsb",
            "identity": {"scheme": "adsb-icao", "field": "icao", "format": "hex"},
            "metadata": ["df", "me.tc"], "require": ["icao"]}));
        assert!(
            !paths(&ok).iter().any(|p| p.starts_with("outputs")),
            "{:?}",
            paths(&ok)
        );
        let i = ok.outputs[0]
            .decode
            .as_ref()
            .unwrap()
            .identity
            .clone()
            .unwrap();
        assert_eq!(
            i.identity_scheme(),
            Some(hk_model::IdentityScheme::AdsbIcao)
        );
        let ric = IdentityMapping {
            scheme: "pocsag-ric".into(),
            field: "ric".into(),
            format: Display::Dec,
        };
        assert_eq!(
            ric.identity_scheme(),
            Some(hk_model::IdentityScheme::Other("pocsag-ric".into()))
        );

        let bad = with(json!({"frame_model": "x", "service": "not a token",
            "identity": {"scheme": "bad scheme!", "field": "a", "format": "bin"}}));
        let p = paths(&bad);
        for want in [
            "outputs[0].decode.service",
            "outputs[0].decode.identity.scheme",
            "outputs[0].decode.identity.format",
        ] {
            assert!(p.iter().any(|x| x == want), "{want} in {p:?}");
        }
        let empty = with(json!({"frame_model": "x", "require": ["a"]}));
        assert!(paths(&empty).iter().any(|x| x == "outputs[0].decode"));
        let unknown = serde_json::from_value::<Recipe>({
            let mut v = minimal();
            v["outputs"] = json!([{"id": "m", "kind": "messages", "from": "b",
                "decode": {"frame_model": "x", "content": ["t"], "nope": 1}}]);
            v
        });
        assert!(unknown.is_err(), "unknown mapping keys are errors");
    }
}
