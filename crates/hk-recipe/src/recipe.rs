//! The recipe schema (ADR-0011 §2): a decoder as data.

use std::collections::{BTreeMap, BTreeSet};

use hk_model::ContentClass;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::fields::{Display, FieldMap, is_field_path};
use crate::param::{BlockDescriptor, Catalogue, ParamSchema, ParamType, Params};
use crate::port::PortType;
use crate::{RECIPE_SCHEMA, RECIPE_SCHEMA_VERSIONS, is_id};

/// Block kind of the multi-channel merge point (ADR-0011 §2.5).
pub const FOLLOW_HOPS_BLOCK: &str = "follow_hops";

/// Block kind of the audio sink (ADR-0011 §8.4): the catalogue's first sink, an input and no
/// outputs. An `audio` output names exactly one such node.
pub const AUDIO_OUT_BLOCK: &str = "audio_out";

/// The backlog a `live-edge` reader tolerates when the recipe does not say (ADR-0011 §8.5):
/// Listen's per-consumer queue, ≈ 0.6 s.
pub const DEFAULT_LIVE_EDGE_BACKLOG_S: f64 = 0.6;

/// Largest `input.liveness.max_backlog_s` a recipe may declare, s. Beyond it "live" means
/// nothing a listener would recognise, and the pipeline's own ring-protection skip applies
/// anyway.
pub const MAX_LIVE_EDGE_BACKLOG_S: f64 = 10.0;

/// A recipe document (`recipes/<id>.recipe.json`, or a saved version in the data directory).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    /// `"hackriff.recipe"`.
    pub schema: String,
    /// Format version: 2, or 3 when the document uses a key schema 3 introduced
    /// ([`crate::RECIPE_SCHEMA_VERSIONS`]).
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
    /// How the pipeline's ring reader behaves when it falls behind (ADR-0011 §8.5, schema 3).
    /// Absent: `live-edge` for a recipe with an `audio` output, `throughput` otherwise
    /// ([`Recipe::liveness`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub liveness: Option<LivenessSpec>,
}

impl InputSpec {
    /// Whether `other` describes the same channel: every key but `liveness`, which changes how
    /// the reader keeps up, not what it reads, so editing it re-plumbs nothing.
    pub fn same_channel(&self, other: &InputSpec) -> bool {
        self.port == other.port
            && self.sample_rate_hz == other.sample_rate_hz
            && self.bandwidth_hz == other.bandwidth_hz
            && self.channels == other.channels
    }
}

/// Reader liveness (ADR-0011 §8.5). **Latency is a contract for audio and merely a statistic for
/// decoding.**
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LivenessSpec {
    /// Policy.
    pub mode: LivenessMode,
    /// `live-edge` only: the backlog, s, beyond which the reader seeks to the live edge
    /// (default [`DEFAULT_LIVE_EDGE_BACKLOG_S`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_backlog_s: Option<f64>,
}

/// Reader liveness policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LivenessMode {
    /// Stay live: when the backlog exceeds `max_backlog_s`, seek the reader to the live edge,
    /// count the skip and flag `DISCONTINUITY`. The default for an `audio` output.
    LiveEdge,
    /// Decode everything the ring still holds; the default for every other recipe. The runtime's
    /// ring-protection skip (`hk_pipeline::recipes::runtime::MAX_BACKLOG_S`) still applies.
    Throughput,
}

/// The liveness a pipeline runs with, after defaults.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Liveness {
    /// Seek to the live edge beyond this backlog, s.
    LiveEdge {
        /// Backlog bound, s.
        max_backlog_s: f64,
    },
    /// Never seek for latency.
    Throughput,
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
    /// Prose explaining what this node does and why, for the Decode step guide (T-168, ADR-0013
    /// §4.9 gap 12). Additive: absent on every recipe written before this field existed, and a
    /// recipe without it still loads and validates unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
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
    /// Listenable audio from an `audio_out` sink node, served as the stream contract §12.2
    /// audio profile on `audio/<pipeline>/<output>` (ADR-0011 §8.2, schema 3).
    Audio,
}

impl OutputKind {
    /// Wire token (`inspector`, `messages`, `stage`, `audio`).
    pub const fn as_str(self) -> &'static str {
        match self {
            OutputKind::Inspector => "inspector",
            OutputKind::Messages => "messages",
            OutputKind::Stage => "stage",
            OutputKind::Audio => "audio",
        }
    }
}

/// Channel layout of an `audio` output. Schema 3 accepts only `mono`; stereo is ADR-0015 §12.13
/// (LP-9/LP-10), a block and a wire change of its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AudioChannels {
    /// One channel.
    #[default]
    Mono,
}

/// Header hints of an `audio` output (ADR-0011 §8.2): what the recipe says it demodulates.
/// Measured values win wherever the runtime has them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioProfile {
    /// Mode token for the header's `audio.mode` (`wfm`, `nbfm`, `am`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// De-emphasis time constant the chain applies, s, for `audio.deemphasis_s`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deemphasis_s: Option<f64>,
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
    /// `audio` only: channel layout (absent: mono).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<AudioChannels>,
    /// `audio` only: header hints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<AudioProfile>,
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
    /// What the refinement maximises ([`RefineObjective::form`]).
    pub objective: RefineObjective,
    /// What is tuned: the channel's `center_hz` and/or `bandwidth_hz`; with the `evidence`
    /// objective (schema 3) also numeric node parameters by path,
    /// `nodes[<id>].params.<name>` (ADR-0015 §2.3's `Tuning.mode` axes: deviation, symbol rate,
    /// loop bandwidth).
    pub tune: Vec<String>,
}

impl RefineSpec {
    /// Whether the refinement may move the channel centre.
    pub fn tunes_center(&self) -> bool {
        self.tune.iter().any(|t| t == "center_hz")
    }

    /// Whether the refinement may change the channel bandwidth.
    pub fn tunes_bandwidth(&self) -> bool {
        self.tune.iter().any(|t| t == "bandwidth_hz")
    }
}

/// The builtin refinement objectives a recipe may name in `refine.objective.builtin`
/// (ADR-0011 §8.7). Each is a registered `hk_demod::refine::Objective` the runtime maps by name
/// (`hk_pipeline::recipes::refine`); `wfm-pilot` is T-070's WFM objective (19 kHz pilot C/N₀,
/// occupied-bandwidth floor, RDS validation over the channel IQ window).
pub const REFINE_BUILTINS: &[&str] = &["wfm-pilot"];

/// The refinement objective. Three forms, exactly one per document ([`Self::form`]):
///
/// - `{node, metric, goal}` — a node status metric (schema 2 and 3);
/// - `{builtin}` — a registered objective measured over the **channel IQ window**, not over one
///   node's port (ADR-0011 §8.7, **schema 3**; T-870): Listen's `wfm-pilot`
///   ([`REFINE_BUILTINS`]);
/// - `{evidence}` — the synthesis evidence ladder (ADR-0015 §2.3, **schema 3**): the pipeline is
///   measured the way the synthesis search measured it, per-stage evidence in bits, and a
///   tuning locks when the deepest stage clears its floor (`hk_synth::EvidenceObjective`).
///
/// The fields are flat options rather than an untagged enum so a malformed objective gets a
/// path-precise validation error instead of serde's "did not match any variant".
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefineObjective {
    /// Node id (`{node, metric, goal}` form).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// Status key: `error_rate`, `quality`, `snr_db` or `lock` (`{node, metric, goal}` form).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric: Option<String>,
    /// Direction (`{node, metric, goal}` form).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<RefineGoal>,
    /// The evidence ladder (`{evidence}` form, schema 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<EvidenceTarget>,
    /// A registered builtin objective's name (`{builtin}` form, schema 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin: Option<String>,
}

/// Which evidence a `{"evidence": …}` objective locks on (ADR-0015 §2.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EvidenceTarget {
    /// Quality is the pipeline's capped evidence bits; a tuning locks when the deepest stage the
    /// pipeline reached clears that stage's floor (`deepest b_k ≥ floor_k`).
    Deepest,
}

/// A validated view of a [`RefineObjective`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectiveForm<'a> {
    /// A node status metric.
    NodeMetric {
        /// Node id.
        node: &'a str,
        /// Status key.
        metric: &'a str,
        /// Direction.
        goal: RefineGoal,
    },
    /// The synthesis evidence ladder.
    Evidence(EvidenceTarget),
    /// A registered builtin objective over the channel IQ window.
    Builtin(&'a str),
}

impl RefineObjective {
    /// The `{node, metric, goal}` form.
    pub fn node_metric(node: &str, metric: &str, goal: RefineGoal) -> Self {
        Self {
            node: Some(node.to_owned()),
            metric: Some(metric.to_owned()),
            goal: Some(goal),
            evidence: None,
            builtin: None,
        }
    }

    /// The `{evidence}` form (schema 3).
    pub fn evidence(target: EvidenceTarget) -> Self {
        Self {
            evidence: Some(target),
            ..Self::default()
        }
    }

    /// The `{builtin}` form (schema 3).
    pub fn builtin(name: &str) -> Self {
        Self {
            builtin: Some(name.to_owned()),
            ..Self::default()
        }
    }

    /// The form this objective takes, or `None` when it mixes or half-fills them.
    pub fn form(&self) -> Option<ObjectiveForm<'_>> {
        match (
            &self.node,
            &self.metric,
            self.goal,
            self.evidence,
            self.builtin.as_deref(),
        ) {
            (Some(node), Some(metric), Some(goal), None, None) => {
                Some(ObjectiveForm::NodeMetric { node, metric, goal })
            }
            (None, None, None, Some(t), None) => Some(ObjectiveForm::Evidence(t)),
            (None, None, None, None, Some(b)) => Some(ObjectiveForm::Builtin(b)),
            _ => None,
        }
    }

    /// The builtin's name, when this is the `{builtin}` form.
    pub fn builtin_name(&self) -> Option<&str> {
        match self.form() {
            Some(ObjectiveForm::Builtin(b)) => Some(b),
            _ => None,
        }
    }
}

/// Parses a node-parameter path, `nodes[<id>].params.<name>` → `(id, name)`: the path format of
/// synthesis free parameters (ADR-0015 §1.2) and of schema-3 `refine.tune` entries.
pub fn parse_param_path(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("nodes[")?;
    let (id, rest) = rest.split_once(']')?;
    let name = rest.strip_prefix(".params.")?;
    (!id.is_empty() && !name.is_empty() && !name.contains('.')).then_some((id, name))
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

    /// The liveness this recipe runs with: the declared one, else `live-edge` at
    /// [`DEFAULT_LIVE_EDGE_BACKLOG_S`] when it has an `audio` output, else `throughput`
    /// (ADR-0011 §8.5).
    pub fn liveness(&self) -> Liveness {
        match &self.input.liveness {
            Some(LivenessSpec {
                mode: LivenessMode::Throughput,
                ..
            }) => Liveness::Throughput,
            Some(LivenessSpec {
                mode: LivenessMode::LiveEdge,
                max_backlog_s,
            }) => Liveness::LiveEdge {
                max_backlog_s: max_backlog_s.unwrap_or(DEFAULT_LIVE_EDGE_BACKLOG_S),
            },
            None if self.has_audio_output() => Liveness::LiveEdge {
                max_backlog_s: DEFAULT_LIVE_EDGE_BACKLOG_S,
            },
            None => Liveness::Throughput,
        }
    }

    /// Whether the recipe declares an `audio` output.
    pub fn has_audio_output(&self) -> bool {
        self.outputs.iter().any(|o| o.kind == OutputKind::Audio)
    }

    /// The structural rules of one `audio` output (ADR-0011 §8.2): schema 3, `from` names an
    /// `audio_out` node (a sink has no port to name), `channels` mono, profile hints sane.
    fn audio_output(
        &self,
        i: usize,
        o: &OutputSpec,
        index: &BTreeMap<&str, usize>,
        v3: bool,
        e: &mut Errors,
    ) {
        let path = format!("outputs[{i}]");
        if !v3 {
            e.push(
                format!("{path}.kind"),
                "audio outputs need schema_version 3",
            );
        }
        match PortRef::parse(&o.from) {
            Some(PortRef::Node { node, port: None }) => {
                if let Some(&j) = index.get(node)
                    && self.nodes[j].block != AUDIO_OUT_BLOCK
                {
                    e.push(
                        format!("{path}.from"),
                        format!("an audio output reads an {AUDIO_OUT_BLOCK} node"),
                    );
                }
            }
            _ => e.push(
                format!("{path}.from"),
                format!("an audio output names an {AUDIO_OUT_BLOCK} node (a sink has no port)"),
            ),
        }
        if let Some(p) = &o.profile {
            if p.mode
                .as_deref()
                .is_some_and(|m| !hk_stream::policy::is_token(m))
            {
                e.push(format!("{path}.profile.mode"), "must be a token");
            }
            if p.deemphasis_s
                .is_some_and(|t| !(t.is_finite() && (0.0..=1e-3).contains(&t)))
            {
                e.push(
                    format!("{path}.profile.deemphasis_s"),
                    "must be in [0, 1e-3] s",
                );
            }
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
        if !RECIPE_SCHEMA_VERSIONS.contains(&self.schema_version) {
            e.push("schema_version", "unsupported recipe format version");
        }
        // Keys schema 3 introduced (ADR-0011 §8.6) are errors in an older document, exactly as
        // an unknown field would have been.
        let v3 = self.schema_version >= 3;
        if self.input.liveness.is_some() && !v3 {
            e.push("input.liveness", "input.liveness needs schema_version 3");
        }
        if let Some(l) = &self.input.liveness {
            match (l.mode, l.max_backlog_s) {
                (LivenessMode::Throughput, Some(_)) => e.push(
                    "input.liveness.max_backlog_s",
                    "only a live-edge reader has a backlog bound",
                ),
                (LivenessMode::LiveEdge, Some(b))
                    if !(b.is_finite() && b > 0.0 && b <= MAX_LIVE_EDGE_BACKLOG_S) =>
                {
                    e.push(
                        "input.liveness.max_backlog_s",
                        format!("must be in (0, {MAX_LIVE_EDGE_BACKLOG_S}] s"),
                    )
                }
                _ => {}
            }
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
        let mut audio_outputs = 0;
        for (i, o) in self.outputs.iter().enumerate() {
            let path = format!("outputs[{i}]");
            if o.kind == OutputKind::Audio {
                audio_outputs += 1;
                self.audio_output(i, o, &index, v3, e);
            } else if o.channels.is_some() || o.profile.is_some() {
                e.push(
                    format!("{path}.kind"),
                    "only audio outputs have channels and a profile",
                );
            }
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
        if audio_outputs > 1 {
            // Liveness and the listener budget are per pipeline (ADR-0011 §8.5, §8.8).
            e.push("outputs", "at most one audio output per recipe");
        }
        let sinks = self
            .nodes
            .iter()
            .filter(|n| n.block == AUDIO_OUT_BLOCK)
            .count();
        if sinks > audio_outputs {
            e.push(
                "nodes",
                "an audio_out node is the sink of exactly one audio output",
            );
        }
        if audio_outputs > 0 && !self.output_policy.content_class.permits_content() {
            // It would serve permanently empty audio (ADR-0011 §8.3).
            e.push(
                "output_policy.content_class",
                "a recipe with an audio output needs a class that permits content",
            );
        }
        if audio_outputs > 0 && !matches!(self.input.channels, ChannelsSpec::Single) {
            e.push(
                "input.channels",
                "audio outputs follow one channel, not follow-hops",
            );
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
            let evidence = match r.objective.form() {
                None => {
                    e.push(
                        "refine.objective",
                        "exactly one of {node, metric, goal}, {evidence} or {builtin}",
                    );
                    false
                }
                Some(ObjectiveForm::NodeMetric { node, metric, .. }) => {
                    if !index.contains_key(node) {
                        e.push("refine.objective.node", "unknown node");
                    }
                    if !["error_rate", "quality", "snr_db", "lock"].contains(&metric) {
                        e.push(
                            "refine.objective.metric",
                            "one of error_rate, quality, snr_db, lock",
                        );
                    }
                    false
                }
                Some(ObjectiveForm::Evidence(_)) => {
                    if self.schema_version < 3 {
                        e.push("refine.objective.evidence", "needs schema_version 3");
                    }
                    true
                }
                // ADR-0011 §8.7: a registered objective over the channel IQ window.
                Some(ObjectiveForm::Builtin(b)) => {
                    if !v3 {
                        e.push(
                            "refine.objective.builtin",
                            "refine.objective.builtin needs schema_version 3",
                        );
                    }
                    if !REFINE_BUILTINS.contains(&b) {
                        e.push(
                            "refine.objective.builtin",
                            format!(
                                "unknown builtin objective (one of {})",
                                REFINE_BUILTINS.join(", ")
                            ),
                        );
                    }
                    if self.input.port != PortType::Iq {
                        e.push(
                            "refine.objective.builtin",
                            "a builtin objective measures the channel IQ: the input must be iq",
                        );
                    }
                    if !matches!(self.input.channels, ChannelsSpec::Single) {
                        e.push(
                            "refine.objective.builtin",
                            "a builtin objective refines one channel, not follow-hops",
                        );
                    }
                    false
                }
            };
            if r.tune.is_empty() {
                e.push("refine.tune", "center_hz and/or bandwidth_hz");
            }
            let mut seen = BTreeSet::new();
            for (i, t) in r.tune.iter().enumerate() {
                let channel = t == "center_hz" || t == "bandwidth_hz";
                if !seen.insert(t.as_str()) {
                    e.push(format!("refine.tune[{i}]"), "listed twice");
                } else if channel {
                    // The channel itself: every objective form tunes it.
                } else if !evidence {
                    e.push("refine.tune", "center_hz and/or bandwidth_hz");
                } else {
                    match parse_param_path(t) {
                        Some((id, _)) if index.contains_key(id) => {}
                        Some(_) => e.push(format!("refine.tune[{i}]"), "unknown node"),
                        None => e.push(
                            format!("refine.tune[{i}]"),
                            "center_hz, bandwidth_hz or nodes[<id>].params.<name>",
                        ),
                    }
                }
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
        // Schema 3: a tuned node parameter must be a numeric parameter of its block (the
        // structural pass already checked the node exists).
        if let Some(r) = &self.refine {
            for (i, t) in r.tune.iter().enumerate() {
                if let Some((id, name)) = parse_param_path(t)
                    && let Some(k) = self.nodes.iter().position(|n| n.id == id)
                    && let Some(d) = descriptors[k]
                    && d.params_pinned
                    && !d.params.iter().any(|p| {
                        p.name == name
                            && matches!(p.ty, ParamType::Int { .. } | ParamType::Float { .. })
                    })
                {
                    e.push(
                        format!("refine.tune[{i}]"),
                        "a numeric parameter of the node's block",
                    );
                }
            }
        }
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
                        // A diagnostic output is a presentation tap (a `stage` output), never a
                        // data path: wiring one into a node would add an unreviewed port type
                        // by the back door (ADR-0011 §9.2, T-609 — `psk_demod.symbols`).
                        let diagnostic = match &from {
                            Endpoint::Node { node, port } => index
                                .get(node.as_str())
                                .and_then(|&j| descriptors[j])
                                .and_then(|sd| sd.output(port))
                                .is_some_and(|p| p.diagnostic),
                            Endpoint::Input => false,
                        };
                        if diagnostic {
                            e.push(
                                path.clone(),
                                "a diagnostic output is a tap for outputs[], not a node input",
                            );
                        }
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
            if o.kind == OutputKind::Audio {
                // The sink itself is the output; `structure` checked it names an `audio_out`,
                // and the node's own input wiring was typed above. A registered `audio_out`
                // must really be a sink.
                if let Some(PortRef::Node { node, .. }) = PortRef::parse(&o.from)
                    && let Some(&j) = index.get(node)
                    && descriptors[j].is_some_and(|d| !d.outputs.is_empty())
                {
                    e.push(
                        format!("outputs[{i}].from"),
                        "the audio sink has output ports",
                    );
                }
                continue;
            }
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
                            (None, _) => return Err("a sink has no output to read"),
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
    fn a_diagnostic_output_may_be_tapped_but_not_wired_into_a_node() {
        // ADR-0011 §9.2 (T-609): `psk_demod.symbols` is a constellation tap. Wiring any
        // diagnostic port into a node input would add a port type by the back door.
        let mut cat = catalogue();
        cat.push(BlockDescriptor {
            name: "demod".into(),
            version: 1,
            group: "iq".into(),
            doc: String::new(),
            inputs: vec![PortSpec::new("in", PortType::Iq)],
            outputs: vec![
                PortSpec::new("out", PortType::Soft),
                PortSpec::new("symbols", PortType::Iq).diagnostic(),
            ],
            params: vec![],
            params_pinned: true,
        });
        let recipe = |from: &str| -> Recipe {
            serde_json::from_value(json!({
                "schema": "hackriff.recipe", "schema_version": 2, "id": "t", "version": 1,
                "name": "T", "input": {"port": "iq"},
                "nodes": [
                    {"id": "d", "block": "demod"},
                    {"id": "n", "block": "identity", "inputs": {"in": from}}
                ],
                "outputs": [
                    {"id": "c", "kind": "stage", "from": "d.symbols"},
                    {"id": "s", "kind": "stage", "from": "n"}
                ],
                "output_policy": {"content_class": "unrestricted"}
            }))
            .unwrap()
        };
        // Tapping it as a stage output, and wiring the main port, both validate.
        recipe("d.out").validate(&cat).expect("main port wires");
        recipe("d").validate(&cat).expect("default port wires");
        let errors = recipe("d.symbols").validate(&cat).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.path == "nodes[1].inputs.in" && e.message.contains("diagnostic")),
            "{errors:?}"
        );
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

    fn audio_catalogue() -> Vec<BlockDescriptor> {
        let mut cat = catalogue();
        cat.push(BlockDescriptor {
            name: "fm".into(),
            version: 1,
            group: "iq".into(),
            doc: String::new(),
            inputs: vec![PortSpec::new("in", PortType::Iq)],
            outputs: vec![PortSpec::new("out", PortType::Real)],
            params: vec![],
            params_pinned: true,
        });
        cat.push(BlockDescriptor {
            name: AUDIO_OUT_BLOCK.into(),
            version: 1,
            group: "audio".into(),
            doc: String::new(),
            inputs: vec![PortSpec::new("in", PortType::Real)],
            outputs: vec![],
            params: vec![],
            params_pinned: true,
        });
        cat
    }

    /// A schema-3 WFM-shaped audio recipe: `iq` → `fm` → `audio_out`, one `audio` output.
    fn audio_doc() -> Value {
        json!({
            "schema": "hackriff.recipe", "schema_version": 3, "id": "a", "version": 1,
            "name": "A", "input": {"port": "iq", "sample_rate_hz": 240000},
            "nodes": [{"id": "fm", "block": "fm"}, {"id": "out", "block": "audio_out"}],
            "outputs": [{"id": "audio", "kind": "audio", "from": "out", "channels": "mono",
                         "profile": {"mode": "wfm", "deemphasis_s": 75e-6}}],
            "output_policy": {"content_class": "unrestricted"}
        })
    }

    fn error_paths(v: Value) -> Vec<String> {
        let r: Recipe = serde_json::from_value(v).unwrap();
        r.validate(&audio_catalogue())
            .err()
            .unwrap_or_default()
            .into_iter()
            .map(|e| e.path)
            .collect()
    }

    /// ADR-0011 §8.2/§8.4/§8.6 (T-866): an `audio` output reads an `audio_out` sink, which may be
    /// a graph leaf; it round-trips; and it defaults the pipeline to `live-edge` at 0.6 s.
    #[test]
    fn an_audio_output_reads_a_sink_and_defaults_to_live_edge() {
        let r: Recipe = serde_json::from_value(audio_doc()).unwrap();
        r.validate(&audio_catalogue()).expect("valid audio recipe");
        assert!(r.has_audio_output());
        assert_eq!(
            r.liveness(),
            Liveness::LiveEdge {
                max_backlog_s: DEFAULT_LIVE_EDGE_BACKLOG_S
            }
        );
        let round: Recipe = serde_json::from_value(serde_json::to_value(&r).unwrap()).unwrap();
        assert_eq!(round, r, "round trip");
        assert_eq!(
            serde_json::to_value(&r).unwrap()["outputs"],
            audio_doc()["outputs"],
            "the audio keys serialise as written"
        );
        // A recipe with no audio output keeps today's behaviour.
        let plain: Recipe = serde_json::from_value(minimal()).unwrap();
        assert_eq!(plain.liveness(), Liveness::Throughput);
    }

    /// ADR-0011 §8.2: `from` names the sink itself (no port), the sink is an `audio_out`, at most
    /// one audio output, mono only, and the sink cannot feed another node.
    #[test]
    fn audio_output_wiring_is_validated() {
        let mut v = audio_doc();
        v["outputs"][0]["from"] = "fm".into();
        assert!(error_paths(v).contains(&"outputs[0].from".to_owned()));

        let mut v = audio_doc();
        v["outputs"][0]["from"] = "out.in".into();
        assert!(error_paths(v).contains(&"outputs[0].from".to_owned()));

        let mut v = audio_doc();
        v["outputs"][0]["channels"] = "stereo".into();
        assert!(
            serde_json::from_value::<Recipe>(v).is_err(),
            "schema 3 accepts only mono"
        );

        let mut v = audio_doc();
        v["outputs"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": "b", "kind": "audio", "from": "out"}));
        assert!(error_paths(v).contains(&"outputs".to_owned()));

        // A sink with no audio output, and a node reading a sink.
        let mut v = audio_doc();
        v["outputs"] = json!([{"id": "s", "kind": "stage", "from": "fm"}]);
        assert!(error_paths(v).contains(&"nodes".to_owned()));
        let mut v = audio_doc();
        v["nodes"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": "x", "block": "identity", "inputs": {"in": "out"}}));
        v["outputs"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": "s", "kind": "stage", "from": "x"}));
        assert!(error_paths(v).contains(&"nodes[2].inputs.in".to_owned()));

        // Only audio outputs carry channels / profile.
        let mut v = audio_doc();
        v["outputs"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": "s", "kind": "stage", "from": "fm", "channels": "mono"}));
        assert!(error_paths(v).contains(&"outputs[1].kind".to_owned()));

        let mut v = audio_doc();
        v["outputs"][0]["profile"] = json!({"mode": "not a token", "deemphasis_s": 1.0});
        let p = error_paths(v);
        assert!(p.contains(&"outputs[0].profile.mode".to_owned()), "{p:?}");
        assert!(
            p.contains(&"outputs[0].profile.deemphasis_s".to_owned()),
            "{p:?}"
        );
    }

    /// An audio output follows one channel: follow-hops recipes cannot declare one. (ADR-0011
    /// §8.3's companion rule — an audio output under a class that forbids content is a
    /// validation error — reads `ContentClass::permits_content`, which is dormant while content
    /// gating is off, the default; that opt-in path is deliberately untested, hk-model
    /// `content`.)
    #[test]
    fn audio_needs_a_single_channel() {
        let mut v = audio_doc();
        v["input"]["channels"] =
            json!({"mode": "follow-hops", "channel_bandwidth_hz": 12500, "max_channels": 2});
        assert!(error_paths(v).contains(&"input.channels".to_owned()));
    }

    /// ADR-0011 §8.6: schema 3's keys are errors in a version-2 document; a version-2 document
    /// without them still reads; any other version is refused.
    #[test]
    fn schema_3_keys_need_schema_version_3() {
        let mut v = audio_doc();
        v["schema_version"] = 2.into();
        assert!(error_paths(v).contains(&"outputs[0].kind".to_owned()));
        let mut v = minimal();
        v["input"]["liveness"] = json!({"mode": "throughput"});
        let p = {
            let r: Recipe = serde_json::from_value(v.clone()).unwrap();
            r.validate_structure().unwrap_err()
        };
        assert!(p.iter().any(|e| e.path == "input.liveness"), "{p:?}");
        v["schema_version"] = 3.into();
        let r: Recipe = serde_json::from_value(v).unwrap();
        r.validate_structure().expect("liveness is a schema-3 key");
        assert_eq!(r.liveness(), Liveness::Throughput);

        let r: Recipe = serde_json::from_value(minimal()).unwrap();
        r.validate_structure().expect("version 2 is still read");
        for bad in [1, 4] {
            let mut v = minimal();
            v["schema_version"] = bad.into();
            let r: Recipe = serde_json::from_value(v).unwrap();
            assert!(
                r.validate_structure()
                    .unwrap_err()
                    .iter()
                    .any(|e| e.path == "schema_version")
            );
        }
    }

    /// ADR-0011 §8.7 (T-870, ADR-0015 LP-6): `refine.objective` has two forms, `{builtin}` and
    /// `{node, metric, goal}`; each round-trips in its own shape, and a mixed or partial object
    /// is refused at parse.
    #[test]
    fn refine_objective_has_a_builtin_form_beside_the_node_metric_form() {
        let mut v = audio_doc();
        v["refine"] =
            json!({"objective": {"builtin": "wfm-pilot"}, "tune": ["center_hz", "bandwidth_hz"]});
        let r: Recipe = serde_json::from_value(v.clone()).unwrap();
        r.validate(&audio_catalogue()).expect("a builtin objective");
        let spec = r.refine.as_ref().unwrap();
        assert_eq!(spec.objective, RefineObjective::builtin("wfm-pilot"));
        assert_eq!(spec.objective.form(), Some(ObjectiveForm::Builtin("wfm-pilot")));
        assert_eq!(spec.objective.builtin_name(), Some("wfm-pilot"));
        assert_eq!(spec.objective.node, None);
        assert!(spec.tunes_center() && spec.tunes_bandwidth());
        assert_eq!(
            serde_json::to_value(&r).unwrap()["refine"],
            v["refine"],
            "round-trips as {{builtin}}"
        );

        let mut v = audio_doc();
        v["refine"] = json!({"objective": {"node": "fm", "metric": "snr_db", "goal": "max"},
                             "tune": ["center_hz"]});
        let r: Recipe = serde_json::from_value(v.clone()).unwrap();
        r.validate(&audio_catalogue())
            .expect("a node metric objective");
        let spec = r.refine.as_ref().unwrap();
        assert_eq!(spec.objective.node.as_deref(), Some("fm"));
        assert_eq!(spec.objective.builtin_name(), None);
        assert!(spec.tunes_center() && !spec.tunes_bandwidth());
        assert_eq!(serde_json::to_value(&r).unwrap()["refine"], v["refine"]);

        // A mixed or partial objective is refused at validation, on the objective's path
        // (T-858's flat form); an unknown key is a parse error.
        for bad in [
            json!({"builtin": "wfm-pilot", "goal": "max"}),
            json!({"builtin": "wfm-pilot", "evidence": "deepest"}),
            json!({"node": "fm", "metric": "snr_db"}),
            json!({}),
        ] {
            let mut v = audio_doc();
            v["refine"] = json!({"objective": bad, "tune": ["center_hz"]});
            assert!(
                error_paths(v).contains(&"refine.objective".to_owned()),
                "{bad} is not an objective"
            );
        }
        let mut v = audio_doc();
        v["refine"] = json!({"objective": {"builtin": "wfm-pilot", "extra": 1}, "tune": ["center_hz"]});
        assert!(serde_json::from_value::<Recipe>(v).is_err());
    }

    /// ADR-0011 §8.6/§8.7: `builtin` is a schema-3 key, names a registered objective, and
    /// measures one iq channel.
    #[test]
    fn a_builtin_objective_is_schema_3_registered_and_on_one_iq_channel() {
        let with = |f: &dyn Fn(&mut Value), builtin: &str| {
            let mut v = audio_doc();
            v["refine"] = json!({"objective": {"builtin": builtin}, "tune": ["center_hz"]});
            f(&mut v);
            error_paths(v)
        };
        let path = "refine.objective.builtin".to_owned();
        assert!(with(&|_| {}, "wfm-pilot").is_empty());
        assert!(with(&|_| {}, "no-such-objective").contains(&path));
        // A version-2 document: the builtin key is refused (and so is the audio output).
        assert!(with(&|v| v["schema_version"] = 2.into(), "wfm-pilot").contains(&path));
        assert!(
            with(
                &|v| v["input"]["channels"] = json!({"mode": "follow-hops", "channel_bandwidth_hz": 12500.0, "max_channels": 4}),
                "wfm-pilot"
            )
            .contains(&path)
        );
        let mut v = minimal();
        v["schema_version"] = 3.into();
        v["refine"] = json!({"objective": {"builtin": "wfm-pilot"}, "tune": ["center_hz"]});
        let r: Recipe = serde_json::from_value(v).unwrap();
        assert!(
            r.validate_structure()
                .unwrap_err()
                .iter()
                .any(|e| e.path == path),
            "a bits-input recipe has no channel IQ to measure"
        );
        for name in REFINE_BUILTINS {
            assert!(with(&|_| {}, name).is_empty(), "{name} validates");
        }
    }

    /// ADR-0011 §8.5: an explicit liveness wins over the audio default; a throughput reader has
    /// no backlog bound; the bound is positive and capped.
    #[test]
    fn liveness_is_declared_and_bounded() {
        let mut v = audio_doc();
        v["input"]["liveness"] = json!({"mode": "live-edge", "max_backlog_s": 0.25});
        let r: Recipe = serde_json::from_value(v).unwrap();
        r.validate(&audio_catalogue()).unwrap();
        assert_eq!(
            r.liveness(),
            Liveness::LiveEdge {
                max_backlog_s: 0.25
            }
        );
        let mut v = audio_doc();
        v["input"]["liveness"] = json!({"mode": "throughput"});
        let r: Recipe = serde_json::from_value(v).unwrap();
        assert_eq!(r.liveness(), Liveness::Throughput);

        for bad in [
            json!({"mode": "throughput", "max_backlog_s": 1.0}),
            json!({"mode": "live-edge", "max_backlog_s": 0.0}),
            json!({"mode": "live-edge", "max_backlog_s": 11.0}),
        ] {
            let mut v = audio_doc();
            v["input"]["liveness"] = bad;
            assert!(error_paths(v).contains(&"input.liveness.max_backlog_s".to_owned()));
        }
        let mut v = audio_doc();
        v["input"]["liveness"] = json!({"mode": "sometimes"});
        assert!(serde_json::from_value::<Recipe>(v).is_err());
    }

    /// T-168 (ADR-0013 §4.9 gap 12): the per-node `doc` field is additive. A recipe written before
    /// it existed (no `doc` key on any node) still loads, `NodeSpec::doc` reads `None`, and
    /// re-serializing omits the key entirely rather than writing `"doc":null`. A recipe that does
    /// set `doc` round-trips it unchanged.
    #[test]
    fn node_doc_field_is_additive() {
        let r: Recipe = serde_json::from_value(minimal()).unwrap();
        assert_eq!(r.nodes[0].doc, None);
        assert!(
            r.validate(&catalogue()).is_ok(),
            "a recipe without node doc still validates"
        );
        let round = serde_json::to_value(&r).unwrap();
        assert!(
            round["nodes"][0].get("doc").is_none(),
            "doc omitted when absent: {round}"
        );

        let mut v = minimal();
        v["nodes"][0]["doc"] = json!("Demodulates the FSK deviation into soft symbols.");
        let r: Recipe = serde_json::from_value(v).unwrap();
        assert_eq!(
            r.nodes[0].doc.as_deref(),
            Some("Demodulates the FSK deviation into soft symbols.")
        );
        let round = serde_json::to_value(&r).unwrap();
        assert_eq!(
            round["nodes"][0]["doc"],
            json!("Demodulates the FSK deviation into soft symbols.")
        );
    }

    // --- Schema 3: `refine.objective.evidence` (T-858 = MAUTO M-7, ADR-0015 §2.3) ----------

    /// A soft→bits chain with a `clock` node carrying a numeric and a non-numeric parameter.
    fn refine_recipe(schema_version: u32, refine: Value) -> Recipe {
        serde_json::from_value(json!({
            "schema": "hackriff.recipe", "schema_version": schema_version, "id": "t",
            "version": 1, "name": "T", "input": {"port": "soft"},
            "nodes": [
                {"id": "clock", "block": "clock", "params": {"symbol_rate_bd": 4800}},
                {"id": "slice", "block": "slicer"}
            ],
            "outputs": [{"id": "s", "kind": "stage", "from": "slice"}],
            "output_policy": {"content_class": "unrestricted"},
            "refine": refine
        }))
        .expect("parses")
    }

    fn refine_catalogue() -> Vec<BlockDescriptor> {
        let mut cat = catalogue();
        cat.push(BlockDescriptor {
            name: "clock".into(),
            version: 1,
            group: "symbol".into(),
            doc: String::new(),
            inputs: vec![PortSpec::new("in", PortType::Soft)],
            outputs: vec![PortSpec::new("out", PortType::Soft)],
            params: serde_json::from_value(json!([
                {"name": "symbol_rate_bd", "type": "float", "min": 1.0},
                {"name": "pulse", "type": "enum", "values": ["nrz", "rz"]}
            ]))
            .unwrap(),
            params_pinned: true,
        });
        cat
    }

    fn paths(r: Result<Resolved, Vec<RecipeError>>) -> Vec<String> {
        r.err()
            .unwrap_or_default()
            .into_iter()
            .map(|e| e.path)
            .collect()
    }

    #[test]
    fn the_evidence_objective_is_schema_3_and_round_trips() {
        let r = refine_recipe(
            3,
            json!({"objective": {"evidence": "deepest"},
                   "tune": ["center_hz", "bandwidth_hz", "nodes[clock].params.symbol_rate_bd"]}),
        );
        r.validate(&refine_catalogue())
            .expect("a schema-3 evidence objective validates");
        let spec = r.refine.as_ref().unwrap();
        assert_eq!(
            spec.objective.form(),
            Some(ObjectiveForm::Evidence(EvidenceTarget::Deepest))
        );
        assert_eq!(
            spec.objective,
            RefineObjective::evidence(EvidenceTarget::Deepest)
        );
        // Serialises back to exactly the ADR's shape: no null node/metric/goal keys.
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["refine"]["objective"], json!({"evidence": "deepest"}));
        assert_eq!(serde_json::from_value::<Recipe>(v).unwrap(), r);
    }

    #[test]
    fn a_schema_2_document_may_not_use_the_evidence_objective() {
        let r = refine_recipe(
            2,
            json!({"objective": {"evidence": "deepest"}, "tune": ["center_hz"]}),
        );
        assert_eq!(
            paths(r.validate(&refine_catalogue())),
            ["refine.objective.evidence"]
        );
    }

    #[test]
    fn the_node_metric_objective_is_unchanged_in_schema_2_and_3() {
        for v in [2, 3] {
            let r = refine_recipe(
                v,
                json!({"objective": {"node": "slice", "metric": "error_rate", "goal": "min"},
                       "tune": ["center_hz"]}),
            );
            r.validate(&refine_catalogue())
                .expect("node-metric form validates");
            assert_eq!(
                r.refine.as_ref().unwrap().objective,
                RefineObjective::node_metric("slice", "error_rate", RefineGoal::Min)
            );
            // …and serialises without an `evidence` key.
            let o = serde_json::to_value(&r).unwrap()["refine"]["objective"].clone();
            assert_eq!(
                o,
                json!({"node": "slice", "metric": "error_rate", "goal": "min"})
            );
        }
        // Unknown versions are still refused.
        for v in [1, 4] {
            let r = refine_recipe(v, Value::Null);
            assert_eq!(paths(r.validate(&refine_catalogue())), ["schema_version"]);
        }
    }

    #[test]
    fn an_objective_is_exactly_one_form() {
        for bad in [
            json!({"node": "slice", "metric": "error_rate", "goal": "min", "evidence": "deepest"}),
            json!({"node": "slice", "metric": "error_rate"}),
            json!({}),
        ] {
            let r = refine_recipe(3, json!({"objective": bad, "tune": ["center_hz"]}));
            assert_eq!(
                paths(r.validate(&refine_catalogue())),
                ["refine.objective"],
                "{bad}"
            );
        }
        // An unknown target and an unknown key are parse errors (unknown fields are errors).
        for bad in [
            json!({"evidence": "shallowest"}),
            json!({"builtin": "wfm-pilot", "extra": 1}),
        ] {
            let v = json!({
                "schema": "hackriff.recipe", "schema_version": 3, "id": "t", "version": 1,
                "name": "T", "input": {"port": "soft"},
                "nodes": [{"id": "slice", "block": "slicer"}],
                "outputs": [{"id": "s", "kind": "stage", "from": "slice"}],
                "output_policy": {"content_class": "unrestricted"},
                "refine": {"objective": bad, "tune": ["center_hz"]}
            });
            assert!(serde_json::from_value::<Recipe>(v).is_err(), "{bad}");
        }
    }

    #[test]
    fn only_the_evidence_objective_tunes_numeric_node_parameters() {
        let cat = refine_catalogue();
        let tune = |objective: Value, t: &[&str]| {
            paths(refine_recipe(3, json!({"objective": objective, "tune": t})).validate(&cat))
        };
        let ev = json!({"evidence": "deepest"});
        let nm = json!({"node": "slice", "metric": "error_rate", "goal": "min"});
        assert!(tune(ev.clone(), &["nodes[clock].params.symbol_rate_bd"]).is_empty());
        // A node-metric objective tunes the channel only.
        assert_eq!(
            tune(nm, &["nodes[clock].params.symbol_rate_bd"]),
            ["refine.tune"]
        );
        // An unknown node, a malformed path, a non-numeric or undeclared parameter, a repeat.
        assert_eq!(
            tune(ev.clone(), &["nodes[nope].params.symbol_rate_bd"]),
            ["refine.tune[0]"]
        );
        assert_eq!(tune(ev.clone(), &["symbol_rate_bd"]), ["refine.tune[0]"]);
        assert_eq!(
            tune(ev.clone(), &["nodes[clock].params.pulse"]),
            ["refine.tune[0]"]
        );
        assert_eq!(
            tune(ev.clone(), &["nodes[clock].params.gain"]),
            ["refine.tune[0]"]
        );
        assert_eq!(
            tune(ev.clone(), &["center_hz", "center_hz"]),
            ["refine.tune[1]"]
        );
        // The channel itself is tunable under both forms.
        assert!(tune(ev.clone(), &["center_hz", "bandwidth_hz"]).is_empty());
        assert_eq!(tune(ev, &[]), ["refine.tune"]);
    }

    #[test]
    fn param_paths_parse_only_in_the_free_parameter_shape() {
        assert_eq!(
            parse_param_path("nodes[clock].params.symbol_rate_bd"),
            Some(("clock", "symbol_rate_bd"))
        );
        for bad in [
            "nodes[].params.x",
            "nodes[a].params.",
            "nodes[a].params.x.y",
            "nodes[a].x",
            "center_hz",
        ] {
            assert_eq!(parse_param_path(bad), None, "{bad}");
        }
    }
}
