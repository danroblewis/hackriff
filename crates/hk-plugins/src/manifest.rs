//! Plugin manifest (ADR-0003; docs/stream-contract.md §9): one JSON file per plugin,
//! `plugins/<id>/manifest.json`. Unknown fields are errors (typos such as `license` must not pass
//! silently). `licence` and `output.content_class` are required; the host enforces the class.
//!
//! **Metadata allowlist (legal guardrail).** Under a class that forbids content, text placed in
//! `metadata`, `frame_model`, `identity` or an annotation label would leak content past the
//! `content` gate. A manifest whose `output.content_class` forbids content must therefore declare
//! `output.metadata_keys`: every metadata key the host may keep, with a type that cannot carry
//! free text (integer, number, boolean, bounded hex or digits string, or an enum). It may also
//! allowlist `frame_models`, annotation `labels` and an `identity` shape. The host applies the
//! allowlist whenever a line's effective class forbids content (see [`crate::output`]). The
//! allowlist model itself lives in [`hk_stream::policy`], shared with egress.
//!
//! **Trusted-but-reviewed defaults.** A policy only ever applies under a restricted class, so its
//! typed fields are covert-channel budgets. hex/digits keys and identities default to
//! `max_len` [`RESTRICTED_DEFAULT_MAX_LEN`] (8); a longer value needs an explicit `max_len` and a
//! `review_note` string, and is reported in [`PluginManifest::warnings`]. `integer`/`number`
//! keys (up to 64 bits per record) are also reported. [`PluginManifest::load`] prints the
//! warnings to stderr and the host copies them into the plugin's log ring.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hk_model::sigmf::Datatype;
use hk_model::{ContentClass, IdentityScheme};
use hk_stream::policy::is_token;
use hk_stream::{FeedFraming, StreamKind};
use serde::Deserialize;
use serde_json::Value;

use crate::host::InputStreamDesc;

pub use hk_stream::policy::{
    Charset, IdentitySpec, MAX_ALLOWLIST_LEN, MetadataPolicy, MetadataType,
    RESTRICTED_DEFAULT_MAX_LEN,
};

/// The manifest format version this build reads.
pub const MANIFEST_VERSION: u32 = 1;

/// The documented example restricted-paging `output` policy (not a plugin):
/// `crates/hk-plugins/policies/restricted-paging.json`. It allowlists only `capcode` (digits, at
/// most 8), `function` (enum 0-3), `baud` (enum 512/1200/2400) and `encoding` (enum
/// numeric/alpha/tone); `t` is host-stamped from `sample_index`. Message bodies, numeric pages
/// included, are content and are never allowlisted.
pub const EXAMPLE_RESTRICTED_PAGING_OUTPUT: &str =
    include_str!("../policies/restricted-paging.json");

/// What a plugin consumes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputKind {
    /// Wideband IQ (the dwell window).
    Iq,
    /// Channelised IQ (one emission).
    Channel,
    /// Demodulated audio.
    Audio,
    /// Hard bits.
    Bits,
}

impl InputKind {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "iq" => InputKind::Iq,
            "channel" => InputKind::Channel,
            "audio" => InputKind::Audio,
            "bits" => InputKind::Bits,
            _ => return None,
        })
    }

    /// Stream kind of the data-plane header.
    pub const fn stream_kind(self) -> StreamKind {
        match self {
            InputKind::Iq | InputKind::Channel => StreamKind::Iq,
            InputKind::Audio => StreamKind::Audio,
            InputKind::Bits => StreamKind::Bits,
        }
    }

    /// Manifest name.
    pub const fn as_str(self) -> &'static str {
        match self {
            InputKind::Iq => "iq",
            InputKind::Channel => "channel",
            InputKind::Audio => "audio",
            InputKind::Bits => "bits",
        }
    }
}

/// An inclusive frequency range; either end may be open.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HzRange {
    /// Lower bound, Hz.
    pub min_hz: Option<f64>,
    /// Upper bound, Hz.
    pub max_hz: Option<f64>,
}

impl HzRange {
    /// Whether `v` is inside.
    pub fn contains(&self, v: f64) -> bool {
        self.min_hz.is_none_or(|m| v >= m) && self.max_hz.is_none_or(|m| v <= m)
    }
}

/// Input constraints.
#[derive(Clone, Debug, PartialEq)]
pub struct InputSpec {
    /// Kind.
    pub kind: InputKind,
    /// Sample datatype (SigMF name).
    pub datatype: Datatype,
    /// Data-plane framing on stdin.
    pub framing: FeedFraming,
    /// Accepted sample rates; empty accepts any.
    pub sample_rates_hz: Vec<f64>,
    /// Accepted RF centre frequencies.
    pub center_hz: Option<HzRange>,
    /// Accepted bandwidths.
    pub bandwidth_hz: Option<HzRange>,
    /// The plugin emits a `ready` line (§9.3) once it can account for the input it is given.
    /// A producer that can pause (a lossless replay) holds its first record until then, so no
    /// input reaches a decoder that is still setting itself up (T-223).
    pub ready_signal: bool,
}

/// Output declaration.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputSpec {
    /// Schema id of the messages' `metadata`/`content`.
    pub schema_id: String,
    /// Ceiling class: messages claiming less restrictive classes are clamped to it (and to the
    /// input channel's class).
    pub content_class: ContentClass,
    /// Allowlist applied under content-forbidding classes. Required when `content_class` forbids
    /// content; `None` means an empty allowlist (fail closed).
    pub metadata_policy: Option<MetadataPolicy>,
}

/// Restart policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestartPolicy {
    /// First backoff after an exit.
    pub backoff_initial: Duration,
    /// Backoff cap (doubling); a run longer than this resets the backoff.
    pub backoff_max: Duration,
    /// More exits than this within `window` stops restarting (crash loop).
    pub max_restarts: u32,
    /// Crash-loop window.
    pub window: Duration,
}

/// Best-effort resource limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceLimits {
    /// Input queue (ring) size, bytes.
    pub input_queue_bytes: usize,
    /// A plugin that **has shown a sign of life** (a byte on stdout or stderr) and whose input
    /// queue then stays full this long is killed (hang watchdog) and restarted. The clock runs
    /// from the later of that first byte and the moment the queue filled, never from the spawn:
    /// a process that has not started yet is not a process that has stopped responding (T-540).
    pub stall_timeout: Duration,
    /// A plugin that has produced **no output at all** since it was spawned, and whose input
    /// queue stays full this long, is killed (never-started watchdog) and restarted.
    ///
    /// It is a separate, longer budget because the thing it bounds is outside this repo's
    /// control: on macOS a freshly linked binary is `posix_spawn`ed in ~193 µs and then executes
    /// **nothing** for up to ~30 s while the loader/code-signing path warms (measured, T-493;
    /// `crates/hk-plugins/tests/common/mod.rs`). Killing such a child on `stall_timeout` kills a
    /// healthy decoder on its first launch after a rebuild or install. This budget is not a
    /// bigger `stall_timeout`: it applies **only** before the first sign of life, and the moment
    /// a byte arrives the tighter `stall_timeout` takes over (T-540).
    pub startup_timeout: Duration,
    /// Longest a producer that cannot pause (a live chain) holds its first record waiting for
    /// `input.ready_signal`; after it, input is offered anyway and the wait is counted. A
    /// producer that can pause (a lossless replay) may wait longer.
    pub ready_timeout: Duration,
    /// Longest accepted stdout line; longer lines are discarded and counted malformed.
    pub max_message_bytes: usize,
    /// Lines kept in the log ring.
    pub stderr_lines: usize,
    /// Scheduling niceness applied after spawn (Unix `setpriority`), if set.
    pub nice: Option<i32>,
}

/// A validated plugin manifest.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginManifest {
    /// Plugin id, `[a-z0-9._-]+`.
    pub id: String,
    /// Plugin (wrapper) version.
    pub version: String,
    /// Licence of the wrapped tool (SPDX expression where possible). Required: GPL tools are
    /// allowed only as subprocesses (ADR-0010) and must be visible in the ledger.
    pub licence: String,
    /// Description.
    pub description: Option<String>,
    /// Executable: a bare name is looked up on `PATH`; a path is relative to the manifest dir.
    /// Trusted code: see `plugins/README.md` (manifests are the trust boundary).
    pub executable: String,
    /// Argument templates (`{input.sample_rate_hz}`, `{input.center_hz}`,
    /// `{input.bandwidth_hz}`, `{input.datatype}`, `{plugin.dir}`, `{param.<name>}`).
    pub args: Vec<String>,
    /// Parameters for `{param.<name>}`.
    pub params: BTreeMap<String, String>,
    /// Input.
    pub input: InputSpec,
    /// Output.
    pub output: OutputSpec,
    /// Restart policy.
    pub restart: RestartPolicy,
    /// Limits.
    pub limits: ResourceLimits,
    /// Directory the manifest was loaded from.
    pub base_dir: Option<PathBuf>,
    /// Review warnings found at validation (long reviewed strings, integer/number keys under a
    /// policy). Never contain plugin output.
    pub warnings: Vec<String>,
}

/// Manifest errors.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum ManifestError {
    /// Not valid manifest JSON (includes unknown fields).
    #[error("manifest JSON: {0}")]
    Json(String),
    /// Could not read the file.
    #[error("reading manifest {path}: {message}")]
    Io {
        /// Path.
        path: PathBuf,
        /// Error.
        message: String,
    },
    /// A required field is absent or empty.
    #[error("missing required field `{0}`")]
    Missing(&'static str),
    /// A field has a bad value.
    #[error("invalid `{field}`: {reason}")]
    Invalid {
        /// Field path.
        field: &'static str,
        /// Why.
        reason: String,
    },
    /// An input stream does not satisfy the manifest.
    #[error("input stream not accepted: {0}")]
    Mismatch(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    manifest_version: Option<u32>,
    id: Option<String>,
    version: Option<String>,
    licence: Option<String>,
    description: Option<String>,
    executable: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    params: BTreeMap<String, String>,
    input: Option<RawInput>,
    output: Option<RawOutput>,
    #[serde(default)]
    restart: RawRestart,
    #[serde(default)]
    limits: RawLimits,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInput {
    kind: Option<String>,
    datatype: Option<String>,
    framing: Option<String>,
    #[serde(default)]
    sample_rates_hz: Vec<f64>,
    center_hz: Option<RawRange>,
    bandwidth_hz: Option<RawRange>,
    ready_signal: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRange {
    min: Option<f64>,
    max: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOutput {
    format: Option<String>,
    schema_id: Option<String>,
    content_class: Option<String>,
    metadata_keys: Option<BTreeMap<String, RawMetaKey>>,
    frame_models: Option<Vec<String>>,
    labels: Option<Vec<String>>,
    identity: Option<RawIdentity>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMetaKey {
    #[serde(rename = "type")]
    kind: String,
    max_len: Option<usize>,
    values: Option<Vec<String>>,
    review_note: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIdentity {
    scheme: String,
    charset: String,
    max_len: Option<usize>,
    review_note: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRestart {
    backoff_initial_ms: Option<u64>,
    backoff_max_ms: Option<u64>,
    max_restarts: Option<u32>,
    window_s: Option<u64>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLimits {
    input_queue_bytes: Option<usize>,
    stall_timeout_ms: Option<u64>,
    startup_timeout_ms: Option<u64>,
    ready_timeout_ms: Option<u64>,
    max_message_bytes: Option<usize>,
    stderr_lines: Option<usize>,
    nice: Option<i32>,
}

fn required(value: Option<String>, field: &'static str) -> Result<String, ManifestError> {
    match value {
        Some(v) if !v.trim().is_empty() => Ok(v),
        _ => Err(ManifestError::Missing(field)),
    }
}

fn invalid(field: &'static str, reason: impl Into<String>) -> ManifestError {
    ManifestError::Invalid {
        field,
        reason: reason.into(),
    }
}

fn token_list(list: Vec<String>, field: &'static str) -> Result<Vec<String>, ManifestError> {
    if let Some(bad) = list.iter().find(|s| !is_token(s)) {
        return Err(invalid(
            field,
            format!("{bad:?} is not [A-Za-z0-9_.:/-]{{1,64}}"),
        ));
    }
    Ok(list)
}

/// `max_len` of a hex/digits key or identity. Default [`RESTRICTED_DEFAULT_MAX_LEN`]; longer (up
/// to [`MAX_ALLOWLIST_LEN`]) only with a non-empty `review_note`, recorded as a warning.
fn string_max_len(
    what: &str,
    max_len: Option<usize>,
    review_note: Option<&str>,
    field: &'static str,
    warnings: &mut Vec<String>,
) -> Result<usize, ManifestError> {
    let n = max_len.unwrap_or(RESTRICTED_DEFAULT_MAX_LEN);
    if !(1..=MAX_ALLOWLIST_LEN).contains(&n) {
        return Err(invalid(
            field,
            format!("{what}: max_len {n} must be 1..={MAX_ALLOWLIST_LEN}"),
        ));
    }
    if n > RESTRICTED_DEFAULT_MAX_LEN {
        match review_note.map(str::trim).filter(|s| !s.is_empty()) {
            None => {
                return Err(invalid(
                    field,
                    format!(
                        "{what}: max_len {n} > {RESTRICTED_DEFAULT_MAX_LEN} needs a review_note (restricted-class covert-channel budget)"
                    ),
                ));
            }
            Some(note) => warnings.push(format!(
                "{field} {what}: max_len {n} > {RESTRICTED_DEFAULT_MAX_LEN} under a restricted class, reviewed: {note}"
            )),
        }
    }
    Ok(n)
}

fn metadata_policy(
    raw: &mut RawOutput,
    warnings: &mut Vec<String>,
) -> Result<Option<MetadataPolicy>, ManifestError> {
    let declared = raw.metadata_keys.is_some()
        || raw.frame_models.is_some()
        || raw.labels.is_some()
        || raw.identity.is_some();
    if !declared {
        return Ok(None);
    }
    let mut keys = BTreeMap::new();
    for (name, key) in raw.metadata_keys.take().unwrap_or_default() {
        if !is_token(&name) {
            return Err(invalid(
                "output.metadata_keys",
                format!("key {name:?} is not [A-Za-z0-9_.:/-]{{1,64}}"),
            ));
        }
        let simple = |t: MetadataType| {
            if key.max_len.is_some() || key.values.is_some() {
                Err(invalid(
                    "output.metadata_keys",
                    format!("{name}: {} takes no max_len/values", key.kind),
                ))
            } else {
                Ok(t)
            }
        };
        let t = match key.kind.as_str() {
            "integer" | "number" => {
                let t = simple(if key.kind == "integer" {
                    MetadataType::Integer
                } else {
                    MetadataType::Number
                })?;
                warnings.push(format!(
                    "output.metadata_keys {name}: {} values carry up to 64 bits per record under a restricted class; prefer an enum or bounded digits",
                    key.kind
                ));
                t
            }
            "boolean" => simple(MetadataType::Boolean)?,
            "hex" | "digits" if key.values.is_none() => {
                let max_len = string_max_len(
                    &name,
                    key.max_len,
                    key.review_note.as_deref(),
                    "output.metadata_keys",
                    warnings,
                )?;
                if key.kind == "hex" {
                    MetadataType::Hex { max_len }
                } else {
                    MetadataType::Digits { max_len }
                }
            }
            "enum" if key.max_len.is_none() => {
                let values = token_list(
                    key.values.clone().unwrap_or_default(),
                    "output.metadata_keys",
                )?;
                if values.is_empty() {
                    return Err(invalid(
                        "output.metadata_keys",
                        format!("{name}: enum needs values"),
                    ));
                }
                MetadataType::Enum(values)
            }
            other => {
                return Err(invalid(
                    "output.metadata_keys",
                    format!(
                        "{name}: type {other:?} (with these options) is not integer, number, boolean, hex, digits or enum; free-text strings cannot be allowlisted"
                    ),
                ));
            }
        };
        keys.insert(name, t);
    }
    let identity = match raw.identity.take() {
        None => None,
        Some(id) => {
            let scheme: IdentityScheme = id
                .scheme
                .parse()
                .map_err(|e: String| invalid("output.identity", e))?;
            let charset = match id.charset.as_str() {
                "hex" => Charset::Hex,
                "digits" => Charset::Digits,
                other => {
                    return Err(invalid(
                        "output.identity",
                        format!("charset {other:?} is not hex or digits"),
                    ));
                }
            };
            Some(IdentitySpec {
                scheme,
                charset,
                max_len: string_max_len(
                    "identity",
                    id.max_len,
                    id.review_note.as_deref(),
                    "output.identity",
                    warnings,
                )?,
            })
        }
    };
    Ok(Some(MetadataPolicy {
        keys,
        frame_models: token_list(
            raw.frame_models.take().unwrap_or_default(),
            "output.frame_models",
        )?,
        labels: token_list(raw.labels.take().unwrap_or_default(), "output.labels")?,
        identity,
    }))
}

/// The metadata policy an `output`-shaped object declares (§9.3: `metadata_keys`,
/// `frame_models`, `labels`, `identity`; `content_class` and the other manifest output keys are
/// accepted and ignored), parsed and checked exactly as a manifest's. A recipe's
/// `output_policy` has this shape, so its in-process decodes are sanitised by the same rules as
/// plugin decodes (T-111). `Ok(None)` when no policy key is declared.
pub fn output_metadata_policy(
    output: &serde_json::Value,
) -> Result<Option<MetadataPolicy>, ManifestError> {
    let mut raw: RawOutput =
        serde_json::from_value(output.clone()).map_err(|e| ManifestError::Json(e.to_string()))?;
    metadata_policy(&mut raw, &mut Vec::new())
}

fn range(raw: Option<RawRange>, field: &'static str) -> Result<Option<HzRange>, ManifestError> {
    let Some(r) = raw else { return Ok(None) };
    for v in [r.min, r.max].into_iter().flatten() {
        if !v.is_finite() || v < 0.0 {
            return Err(invalid(
                field,
                format!("{v} is not a finite non-negative Hz value"),
            ));
        }
    }
    if let (Some(lo), Some(hi)) = (r.min, r.max)
        && lo > hi
    {
        return Err(invalid(field, format!("min {lo} > max {hi}")));
    }
    Ok(Some(HzRange {
        min_hz: r.min,
        max_hz: r.max,
    }))
}

/// Expands `{key}` placeholders with `lookup`; `{{`/`}}` are literal braces.
fn expand(
    template: &str,
    mut lookup: impl FnMut(&str) -> Option<String>,
) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(i) = rest.find(['{', '}']) {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        if tail.starts_with("{{") || tail.starts_with("}}") {
            out.push_str(&tail[..1]);
            rest = &tail[2..];
        } else if tail.starts_with('}') {
            return Err(format!("unmatched `}}` in {template:?}"));
        } else {
            let end = tail
                .find('}')
                .ok_or_else(|| format!("unclosed `{{` in {template:?}"))?;
            let key = &tail[1..end];
            out.push_str(&lookup(key).ok_or_else(|| format!("unknown placeholder {{{key}}}"))?);
            rest = &tail[end + 1..];
        }
    }
    out.push_str(rest);
    Ok(out)
}

impl PluginManifest {
    /// Parses and validates manifest JSON.
    pub fn from_json_str(json: &str) -> Result<Self, ManifestError> {
        let raw: RawManifest =
            serde_json::from_str(json).map_err(|e| ManifestError::Json(e.to_string()))?;
        Self::validate(raw)
    }

    /// Loads `path` (usually `plugins/<id>/manifest.json`); relative executables resolve against
    /// its directory.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ManifestError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|e| ManifestError::Io {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        let mut m = Self::from_json_str(&text)?;
        m.base_dir = path.parent().map(Path::to_path_buf);
        for w in &m.warnings {
            eprintln!("hk-plugins: {}: warning: {w}", path.display());
        }
        Ok(m)
    }

    fn validate(raw: RawManifest) -> Result<Self, ManifestError> {
        match raw.manifest_version {
            None => return Err(ManifestError::Missing("manifest_version")),
            Some(MANIFEST_VERSION) => {}
            Some(v) => {
                return Err(invalid(
                    "manifest_version",
                    format!("{v}; this build reads {MANIFEST_VERSION}"),
                ));
            }
        }
        let id = required(raw.id, "id")?;
        if !id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
        {
            return Err(invalid("id", format!("{id:?} is not [a-z0-9._-]+")));
        }
        let version = required(raw.version, "version")?;
        let licence = required(raw.licence, "licence")?;
        let executable = required(raw.executable, "executable")?;

        let raw_input = raw.input.ok_or(ManifestError::Missing("input"))?;
        let kind_name = required(raw_input.kind, "input.kind")?;
        let kind = InputKind::parse(&kind_name).ok_or_else(|| {
            invalid(
                "input.kind",
                format!("{kind_name:?} is not one of iq, channel, audio, bits"),
            )
        })?;
        let datatype_name = required(raw_input.datatype, "input.datatype")?;
        let datatype: Datatype = serde_json::from_value(Value::String(datatype_name.clone()))
            .map_err(|_| {
                invalid(
                    "input.datatype",
                    format!("unknown datatype {datatype_name:?}"),
                )
            })?;
        let datatype_ok = match kind {
            InputKind::Iq | InputKind::Channel => datatype.is_complex(),
            InputKind::Audio => !datatype.is_complex(),
            InputKind::Bits => datatype == Datatype::Ru8,
        };
        if !datatype_ok {
            return Err(invalid(
                "input.datatype",
                format!("{datatype_name} does not fit input kind {kind_name}"),
            ));
        }
        let framing = match raw_input.framing.as_deref() {
            None | Some("hackriff-v1") => FeedFraming::HackriffV1,
            Some("raw") => FeedFraming::Raw,
            Some(other) => {
                return Err(invalid(
                    "input.framing",
                    format!("{other:?} is not hackriff-v1 or raw"),
                ));
            }
        };
        if let Some(bad) = raw_input
            .sample_rates_hz
            .iter()
            .find(|r| !r.is_finite() || **r <= 0.0)
        {
            return Err(invalid(
                "input.sample_rates_hz",
                format!("{bad} is not a positive rate"),
            ));
        }
        let input = InputSpec {
            kind,
            datatype,
            framing,
            sample_rates_hz: raw_input.sample_rates_hz,
            center_hz: range(raw_input.center_hz, "input.center_hz")?,
            bandwidth_hz: range(raw_input.bandwidth_hz, "input.bandwidth_hz")?,
            ready_signal: raw_input.ready_signal.unwrap_or(false),
        };

        let mut raw_output = raw.output.ok_or(ManifestError::Missing("output"))?;
        match raw_output.format.as_deref() {
            None | Some("ndjson") => {}
            Some(other) => {
                return Err(invalid("output.format", format!("{other:?} is not ndjson")));
            }
        }
        let schema_id = required(raw_output.schema_id.take(), "output.schema_id")?;
        // The schema id replaces non-allowlisted frame models and labels under restricted
        // classes, so it must itself be token-shaped.
        if !is_token(&schema_id) {
            return Err(invalid(
                "output.schema_id",
                format!("{schema_id:?} is not [A-Za-z0-9_.:/-]{{1,64}}"),
            ));
        }
        let class_name = required(raw_output.content_class.take(), "output.content_class")?;
        // A manifest class must be spelled correctly: a typo is an error here, not a silent
        // fail-closed downgrade (plugin *messages* still fail closed at run time).
        let content_class: ContentClass = serde_json::from_value(Value::String(class_name.clone()))
            .map_err(|_| {
                invalid(
                    "output.content_class",
                    format!("unknown content class {class_name:?}"),
                )
            })?;
        let mut warnings = Vec::new();
        let metadata_policy = metadata_policy(&mut raw_output, &mut warnings)?;
        if !content_class.permits_content() && raw_output_lacks_keys(&metadata_policy) {
            return Err(ManifestError::Missing("output.metadata_keys"));
        }

        let restart = RestartPolicy {
            backoff_initial: Duration::from_millis(raw.restart.backoff_initial_ms.unwrap_or(200)),
            backoff_max: Duration::from_millis(raw.restart.backoff_max_ms.unwrap_or(30_000)),
            max_restarts: raw.restart.max_restarts.unwrap_or(5),
            window: Duration::from_secs(raw.restart.window_s.unwrap_or(300)),
        };
        if restart.backoff_initial.is_zero() || restart.backoff_initial > restart.backoff_max {
            return Err(invalid(
                "restart",
                "need 0 < backoff_initial_ms <= backoff_max_ms",
            ));
        }
        let limits = ResourceLimits {
            input_queue_bytes: raw.limits.input_queue_bytes.unwrap_or(8 * 1024 * 1024),
            stall_timeout: Duration::from_millis(raw.limits.stall_timeout_ms.unwrap_or(10_000)),
            // 60 s: twice the worst start-up stall measured on this hardware (T-493), so a cold
            // link cannot be mistaken for a hang, while a child that truly never runs still
            // fails within a minute.
            startup_timeout: Duration::from_millis(raw.limits.startup_timeout_ms.unwrap_or(60_000)),
            ready_timeout: Duration::from_millis(raw.limits.ready_timeout_ms.unwrap_or(5_000)),
            max_message_bytes: raw.limits.max_message_bytes.unwrap_or(1024 * 1024),
            stderr_lines: raw.limits.stderr_lines.unwrap_or(200),
            nice: raw.limits.nice,
        };
        if limits.max_message_bytes == 0 || limits.stall_timeout.is_zero() {
            return Err(invalid(
                "limits",
                "max_message_bytes and stall_timeout_ms must be > 0",
            ));
        }
        if limits.startup_timeout < limits.stall_timeout {
            return Err(invalid(
                "limits",
                "startup_timeout_ms must be >= stall_timeout_ms: a child that has not started \
                 yet cannot be given less time than one that has",
            ));
        }
        if let Some(n) = limits.nice
            && !(-20..=19).contains(&n)
        {
            return Err(invalid("limits.nice", format!("{n} outside -20..=19")));
        }

        let m = Self {
            id,
            version,
            licence,
            description: raw.description,
            executable,
            args: raw.args,
            params: raw.params,
            input,
            output: OutputSpec {
                schema_id,
                content_class,
                metadata_policy,
            },
            restart,
            limits,
            base_dir: None,
            warnings,
        };
        for template in &m.args {
            expand(template, |key| m.placeholder(key, None))
                .map_err(|reason| invalid("args", reason))?;
        }
        Ok(m)
    }

    /// A placeholder's value; with no input stream, known input keys expand to "".
    fn placeholder(&self, key: &str, input: Option<&InputStreamDesc>) -> Option<String> {
        let num = |v: Option<f64>| v.map(|x| x.to_string()).unwrap_or_default();
        match key {
            "input.sample_rate_hz" => Some(
                input
                    .map(|i| i.sample_rate_hz.to_string())
                    .unwrap_or_default(),
            ),
            "input.center_hz" => Some(num(input.and_then(|i| i.center_hz))),
            "input.bandwidth_hz" => Some(num(input.and_then(|i| i.bandwidth_hz))),
            "input.datatype" => Some(self.input.datatype.as_str().to_owned()),
            "plugin.dir" => Some(
                self.base_dir
                    .as_ref()
                    .map(|d| d.display().to_string())
                    .unwrap_or_default(),
            ),
            _ => key
                .strip_prefix("param.")
                .and_then(|name| self.params.get(name).cloned()),
        }
    }

    /// The argument list for an input stream.
    pub fn render_args(&self, input: &InputStreamDesc) -> Result<Vec<String>, ManifestError> {
        self.args
            .iter()
            .map(|t| {
                expand(t, |key| self.placeholder(key, Some(input)))
                    .map_err(|reason| invalid("args", reason))
            })
            .collect()
    }

    /// The program to run.
    pub fn resolve_executable(&self) -> PathBuf {
        let exe = Path::new(&self.executable);
        match (
            &self.base_dir,
            exe.is_relative() && self.executable.contains('/'),
        ) {
            (Some(dir), true) => dir.join(exe),
            _ => exe.to_path_buf(),
        }
    }

    /// Checks that an input stream satisfies the manifest's input constraints.
    pub fn check_input(&self, input: &InputStreamDesc) -> Result<(), ManifestError> {
        let mismatch = |m: String| Err(ManifestError::Mismatch(m));
        if input.datatype != self.input.datatype {
            return mismatch(format!(
                "datatype {} but plugin reads {}",
                input.datatype, self.input.datatype
            ));
        }
        if !self.input.sample_rates_hz.is_empty()
            && !self
                .input
                .sample_rates_hz
                .iter()
                .any(|r| (r - input.sample_rate_hz).abs() <= r * 1e-9)
        {
            return mismatch(format!(
                "sample rate {} Hz not in {:?}",
                input.sample_rate_hz, self.input.sample_rates_hz
            ));
        }
        for (what, spec, value) in [
            ("centre", self.input.center_hz, input.center_hz),
            ("bandwidth", self.input.bandwidth_hz, input.bandwidth_hz),
        ] {
            match (spec, value) {
                (Some(r), Some(v)) if !r.contains(v) => {
                    return mismatch(format!("{what} {v} Hz outside {r:?}"));
                }
                (Some(_), None) => return mismatch(format!("{what} frequency required")),
                _ => {}
            }
        }
        Ok(())
    }
}

/// A content-forbidding manifest must declare `metadata_keys` explicitly (an empty object is an
/// explicit declaration); allowlisting only labels or identity does not count.
fn raw_output_lacks_keys(policy: &Option<MetadataPolicy>) -> bool {
    policy.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_model::{SampleTime, Timestamp};
    use serde_json::json;

    fn base() -> Value {
        json!({
            "manifest_version": 1,
            "id": "adsb-like",
            "version": "0.1.0",
            "licence": "GPL-3.0-or-later",
            "executable": "readsb",
            "args": ["--rate", "{input.sample_rate_hz}", "--fmt", "{input.datatype}", "--x", "{param.x}", "{{literal}}"],
            "params": {"x": "7"},
            "input": {"kind": "iq", "datatype": "ci16_le", "framing": "raw", "sample_rates_hz": [2400000],
                      "center_hz": {"min": 1089e6, "max": 1091e6}},
            "output": {"format": "ndjson", "schema_id": "hackriff.adsb/1", "content_class": "unrestricted"}
        })
    }

    fn parse(v: &Value) -> Result<PluginManifest, ManifestError> {
        PluginManifest::from_json_str(&v.to_string())
    }

    fn input(rate: f64, center: Option<f64>) -> InputStreamDesc {
        InputStreamDesc {
            datatype: Datatype::Ci16Le,
            sample_rate_hz: rate,
            center_hz: center,
            bandwidth_hz: None,
            content_class: ContentClass::FAIL_CLOSED,
            anchor: SampleTime {
                sample_index: 0,
                host_time: Timestamp::UNIX_EPOCH,
            },
            emitter_id: None,
            provenance_ref: None,
        }
    }

    /// A startup budget shorter than the responsiveness one would re-create the T-540 defect in a
    /// manifest: a child that has not run yet judged more harshly than one that has.
    #[test]
    fn a_startup_budget_under_the_stall_budget_is_refused() {
        let mut v = base();
        v["limits"] = json!({"stall_timeout_ms": 10_000, "startup_timeout_ms": 5_000});
        let err = parse(&v).unwrap_err();
        assert!(format!("{err}").contains("startup_timeout_ms"), "{err}");
        v["limits"] = json!({"stall_timeout_ms": 10_000});
        assert_eq!(
            parse(&v).unwrap().limits.startup_timeout,
            Duration::from_secs(60)
        );
    }

    #[test]
    fn valid_manifest_parses_with_defaults_and_renders_args() {
        let m = parse(&base()).unwrap();
        assert_eq!(m.input.framing, FeedFraming::Raw);
        assert_eq!(m.output.content_class, ContentClass::Unrestricted);
        assert_eq!(m.output.metadata_policy, None);
        assert_eq!(m.restart.max_restarts, 5);
        assert_eq!(m.limits.stall_timeout, Duration::from_secs(10));
        assert_eq!(m.limits.startup_timeout, Duration::from_secs(60));
        let args = m.render_args(&input(2.4e6, Some(1090e6))).unwrap();
        assert_eq!(
            args,
            [
                "--rate",
                "2400000",
                "--fmt",
                "ci16_le",
                "--x",
                "7",
                "{literal}"
            ]
        );
        m.check_input(&input(2.4e6, Some(1090e6))).unwrap();
        assert!(m.check_input(&input(2e6, Some(1090e6))).is_err());
        assert!(m.check_input(&input(2.4e6, Some(433.92e6))).is_err());
        assert!(m.check_input(&input(2.4e6, None)).is_err());
        let mut other = input(2.4e6, Some(1090e6));
        other.datatype = Datatype::Cu8;
        assert!(m.check_input(&other).is_err());
    }

    #[test]
    fn the_dummy_manifest_in_the_repo_is_valid() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/dummy/manifest.json");
        let m = PluginManifest::load(&path).unwrap();
        assert_eq!(m.id, "dummy");
        assert_eq!(m.input.kind, InputKind::Channel);
        assert!(m.base_dir.is_some());
    }

    fn expect_err(mutate: impl FnOnce(&mut Value)) -> ManifestError {
        let mut v = base();
        mutate(&mut v);
        parse(&v).expect_err("manifest should be rejected")
    }

    #[test]
    fn validation_errors() {
        assert_eq!(
            expect_err(|v| {
                v.as_object_mut().unwrap().remove("licence");
            }),
            ManifestError::Missing("licence")
        );
        assert_eq!(
            expect_err(|v| v["licence"] = json!("  ")),
            ManifestError::Missing("licence")
        );
        assert_eq!(
            expect_err(|v| {
                v["output"].as_object_mut().unwrap().remove("content_class");
            }),
            ManifestError::Missing("output.content_class")
        );
        assert!(matches!(
            expect_err(|v| v["output"]["content_class"] = json!("public")),
            ManifestError::Invalid {
                field: "output.content_class",
                ..
            }
        ));
        for bad_key in [
            json!({"text": {"type": "string", "max_len": 16}}),
            json!({"text": {"type": "hex", "max_len": 16}}),
            json!({"text": {"type": "hex", "max_len": 16, "review_note": "  "}}),
            json!({"text": {"type": "digits", "max_len": 65, "review_note": "x"}}),
            json!({"text": {"type": "digits", "max_len": 0}}),
            json!({"text": {"type": "integer", "max_len": 4}}),
            json!({"text": {"type": "enum", "values": []}}),
            json!({"text": {"type": "enum", "values": ["has space"]}}),
            json!({"bad key": {"type": "integer"}}),
        ] {
            let err = expect_err(|v| {
                v["output"]["content_class"] = json!("restricted-paging");
                v["output"]["metadata_keys"] = bad_key.clone();
            });
            assert!(
                matches!(
                    err,
                    ManifestError::Invalid {
                        field: "output.metadata_keys",
                        ..
                    }
                ),
                "{bad_key}: {err:?}"
            );
        }
        assert!(matches!(
            expect_err(|v| {
                v["output"]["content_class"] = json!("restricted-paging");
                v["output"]["metadata_keys"] = json!({});
                v["output"]["identity"] =
                    json!({"scheme": "adsb-icao", "charset": "alnum", "max_len": 6});
            }),
            ManifestError::Invalid {
                field: "output.identity",
                ..
            }
        ));
        assert!(matches!(
            expect_err(|v| {
                v["output"]["metadata_keys"] = json!({});
                v["output"]["labels"] = json!(["free text label"]);
            }),
            ManifestError::Invalid {
                field: "output.labels",
                ..
            }
        ));
        assert!(matches!(
            expect_err(|v| v["output"]["schema_id"] = json!("free text schema")),
            ManifestError::Invalid {
                field: "output.schema_id",
                ..
            }
        ));
        assert!(matches!(
            expect_err(|v| {
                v["output"]["content_class"] = json!("restricted-paging");
                v["output"]["metadata_keys"] = json!({});
                v["output"]["identity"] =
                    json!({"scheme": "adsb-icao", "charset": "hex", "max_len": 64});
            }),
            ManifestError::Invalid {
                field: "output.identity",
                ..
            }
        ));
        assert!(matches!(
            expect_err(|v| v["input"]["kind"] = json!("video")),
            ManifestError::Invalid {
                field: "input.kind",
                ..
            }
        ));
        assert_eq!(
            expect_err(|v| {
                v["input"].as_object_mut().unwrap().remove("kind");
            }),
            ManifestError::Missing("input.kind")
        );
        assert!(matches!(
            expect_err(|v| v["input"]["datatype"] = json!("rf32_le")),
            ManifestError::Invalid {
                field: "input.datatype",
                ..
            }
        ));
        assert!(matches!(
            expect_err(|v| {
                v["input"]["kind"] = json!("bits");
                v["input"]["datatype"] = json!("ci8");
            }),
            ManifestError::Invalid {
                field: "input.datatype",
                ..
            }
        ));
        assert!(matches!(
            expect_err(|v| v["input"]["framing"] = json!("zmq")),
            ManifestError::Invalid {
                field: "input.framing",
                ..
            }
        ));
        assert!(matches!(
            expect_err(|v| v["manifest_version"] = json!(2)),
            ManifestError::Invalid {
                field: "manifest_version",
                ..
            }
        ));
        assert!(matches!(
            expect_err(|v| v["args"] = json!(["{param.missing}"])),
            ManifestError::Invalid { field: "args", .. }
        ));
        assert!(matches!(
            expect_err(|v| v["args"] = json!(["{input.sample_rate_hz"])),
            ManifestError::Invalid { field: "args", .. }
        ));
        assert!(matches!(
            expect_err(|v| v["id"] = json!("Bad Id")),
            ManifestError::Invalid { field: "id", .. }
        ));
        // A misspelt required field is reported, not ignored.
        let ManifestError::Json(msg) = expect_err(|v| {
            let o = v.as_object_mut().unwrap();
            let l = o.remove("licence").unwrap();
            o.insert("license".into(), l);
        }) else {
            panic!()
        };
        assert!(msg.contains("license"), "{msg}");
    }
}
