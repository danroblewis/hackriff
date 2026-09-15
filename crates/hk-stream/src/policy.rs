//! Typed metadata allowlist (docs/stream-contract.md §6.2, §9.3). Applies only when content
//! gating is opted in ([`hk_model::content_gating_enabled`]); by default every class permits content.
//!
//! Under a class that forbids content, text placed in `metadata`, `frame_model`, `identity` or an
//! annotation label would leak content past the `content` gate. A [`MetadataPolicy`] lists what
//! may survive: metadata keys with a type that cannot carry free text (integer, number, boolean,
//! bounded hex or digits string, enum), allowlisted frame models and labels, and an identity shape.
//!
//! One model, three enforcement points, all calling the functions here:
//! - **Plugin ingest** (`hk-plugins`): every plugin line whose effective class forbids content is
//!   reduced with the manifest's policy ([`sanitize_decode`], [`sanitize_annotation`]) before it
//!   reaches the `Repository`.
//! - **In-process producers** that write restricted `Decode`/`Annotation` rows must call the same
//!   functions before `Repository::insert_*`. The repository refuses restricted *content* but
//!   stores whatever metadata it is given; `hk_plugins::Ingest` re-checks the shape
//!   ([`decode_is_allowlist_shaped`]) and strips non-conforming rows.
//! - **Egress** ([`crate::Publisher`]): every message whose effective class forbids content is
//!   reduced with the publisher's policy before serialisation, whoever produced it. A messages
//!   publisher whose header class forbids content cannot be created without a policy.
//!
//! No policy (`None`) means the empty allowlist: nothing but the host-generated fields survives.

use std::collections::BTreeMap;

use hk_model::{Annotation, Decode, DecodedIdentity, IdentityScheme};
use serde_json::{Map, Value};

/// Longest allowlisted string (hex/digits values, enum values, labels, frame models).
pub const MAX_ALLOWLIST_LEN: usize = 64;

/// Default (and unreviewed maximum) `max_len` of a hex/digits key or identity under a restricted
/// class. Longer values need an explicit `max_len` and a `review_note` in the manifest.
pub const RESTRICTED_DEFAULT_MAX_LEN: usize = 8;

/// Annotation confidence is rounded to this step under a restricted class, so it carries at most
/// ~7 bits.
pub const CONFIDENCE_STEP: f64 = 0.01;

/// Character set of an allowlisted string value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Charset {
    /// Hex digits.
    Hex,
    /// Decimal digits.
    Digits,
}

fn charset_ok(s: &str, charset: Charset, max_len: usize) -> bool {
    !s.is_empty()
        && s.len() <= max_len
        && s.bytes().all(|b| match charset {
            Charset::Hex => b.is_ascii_hexdigit(),
            Charset::Digits => b.is_ascii_digit(),
        })
}

/// Type of an allowlisted metadata value. None can carry free text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MetadataType {
    /// A JSON integer.
    Integer,
    /// A JSON number.
    Number,
    /// A JSON boolean.
    Boolean,
    /// A non-empty string of hex digits, at most `max_len` long.
    Hex {
        /// Longest accepted value.
        max_len: usize,
    },
    /// A non-empty string of decimal digits, at most `max_len` long.
    Digits {
        /// Longest accepted value.
        max_len: usize,
    },
    /// One of the listed strings.
    Enum(Vec<String>),
}

impl MetadataType {
    /// Whether `v` has this type (over-long strings are rejected, not truncated: a truncated
    /// identifier is a wrong identifier).
    pub fn accepts(&self, v: &Value) -> bool {
        match self {
            MetadataType::Integer => v.is_i64() || v.is_u64(),
            MetadataType::Number => v.is_number(),
            MetadataType::Boolean => v.is_boolean(),
            MetadataType::Hex { max_len } => v
                .as_str()
                .is_some_and(|s| charset_ok(s, Charset::Hex, *max_len)),
            MetadataType::Digits { max_len } => v
                .as_str()
                .is_some_and(|s| charset_ok(s, Charset::Digits, *max_len)),
            MetadataType::Enum(values) => v.as_str().is_some_and(|s| values.iter().any(|x| x == s)),
        }
    }
}

/// The identity a restricted-class producer may name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentitySpec {
    /// Required scheme.
    pub scheme: IdentityScheme,
    /// Value characters.
    pub charset: Charset,
    /// Longest value.
    pub max_len: usize,
}

impl IdentitySpec {
    /// Whether an identity matches the spec.
    pub fn accepts(&self, id: &DecodedIdentity) -> bool {
        id.scheme == self.scheme && charset_ok(&id.value, self.charset, self.max_len)
    }
}

/// What survives from a record whose effective class forbids content.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MetadataPolicy {
    /// Allowed metadata keys and their types; everything else is dropped.
    pub keys: BTreeMap<String, MetadataType>,
    /// Allowed decode `frame_model` values; others are replaced by the fallback schema id.
    pub frame_models: Vec<String>,
    /// Allowed annotation labels; others are replaced by the fallback schema id.
    pub labels: Vec<String>,
    /// Allowed identity shape; other identities are dropped.
    pub identity: Option<IdentitySpec>,
}

/// A short token: `[A-Za-z0-9_.:/-]{1,64}` (enum values, labels, frame models, key names).
pub fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_ALLOWLIST_LEN
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.:/-".contains(&b))
}

/// A producer id such as `readsb@3.16`: a token that may also contain `@` and `+`.
pub fn is_producer_token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_ALLOWLIST_LEN
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.:/-@+".contains(&b))
}

fn keep(policy: Option<&MetadataPolicy>, key: &str, value: &Value) -> bool {
    policy
        .and_then(|p| p.keys.get(key))
        .is_some_and(|t| t.accepts(value))
}

/// Keeps only allowlisted, correctly typed metadata keys. Returns the kept object and how many
/// keys (or a non-object value) were dropped.
pub fn sanitize_metadata(policy: Option<&MetadataPolicy>, metadata: Value) -> (Value, u64) {
    let Value::Object(map) = metadata else {
        return (Value::Object(Map::new()), 1);
    };
    let mut kept = Map::new();
    let mut dropped = 0;
    for (key, value) in map {
        if keep(policy, &key, &value) {
            kept.insert(key, value);
        } else {
            dropped += 1;
        }
    }
    (Value::Object(kept), dropped)
}

/// [`sanitize_metadata`] on a borrowed value (clones only what is kept).
pub fn sanitize_metadata_ref(policy: Option<&MetadataPolicy>, metadata: &Value) -> (Value, u64) {
    let Value::Object(map) = metadata else {
        return (Value::Object(Map::new()), 1);
    };
    let mut kept = Map::new();
    let mut dropped = 0;
    for (key, value) in map {
        if keep(policy, key, value) {
            kept.insert(key.clone(), value.clone());
        } else {
            dropped += 1;
        }
    }
    (Value::Object(kept), dropped)
}

/// Keeps `value` if it is in `allowed` or equals `fallback`; otherwise returns `fallback`.
/// The second element is 1 when the value was replaced.
pub fn allowlisted_or(allowed: &[String], value: &str, fallback: &str) -> (String, u64) {
    if value == fallback || allowed.iter().any(|a| a == value) {
        (value.to_owned(), 0)
    } else {
        (fallback.to_owned(), 1)
    }
}

/// Keeps an identity only if it matches the policy's identity shape.
pub fn sanitize_identity(
    policy: Option<&MetadataPolicy>,
    identity: Option<DecodedIdentity>,
) -> (Option<DecodedIdentity>, u64) {
    match identity {
        Some(id)
            if policy
                .and_then(|p| p.identity.as_ref())
                .is_some_and(|spec| spec.accepts(&id)) =>
        {
            (Some(id), 0)
        }
        Some(_) => (None, 1),
        None => (None, 0),
    }
}

/// Rounds a confidence to [`CONFIDENCE_STEP`] (NaN and out-of-range values clamp into 0..=1).
pub fn quantize_confidence(c: f64) -> f64 {
    if !c.is_finite() {
        return 0.0;
    }
    ((c.clamp(0.0, 1.0) / CONFIDENCE_STEP).round() * CONFIDENCE_STEP).clamp(0.0, 1.0)
}

/// Reduces a Decode row to its policy **if its class forbids content** (otherwise a no-op):
/// metadata allowlisted, `frame_model` allowlisted or replaced by `fallback_frame_model` (the
/// producer's schema id), identity checked. `content` is left for the repository and stream gates
/// to refuse. Returns the number of fields removed or replaced.
pub fn sanitize_decode(
    policy: Option<&MetadataPolicy>,
    fallback_frame_model: &str,
    d: &mut Decode,
) -> u64 {
    if d.content_class.permits_content() {
        return 0;
    }
    let (metadata, mut n) = sanitize_metadata(policy, std::mem::take(&mut d.metadata));
    d.metadata = metadata;
    let (fm, k) = allowlisted_or(
        policy.map_or(&[], |p| p.frame_models.as_slice()),
        &d.frame_model,
        fallback_frame_model,
    );
    d.frame_model = fm;
    n += k;
    let (identity, k) = sanitize_identity(policy, d.identity.take());
    d.identity = identity;
    n + k
}

/// Reduces an Annotation row to its policy **if its class forbids content** (otherwise a no-op):
/// metadata allowlisted, label allowlisted or replaced by `fallback_label`, confidence rounded to
/// [`CONFIDENCE_STEP`]. Returns the number of fields removed or replaced (rounding is not counted).
pub fn sanitize_annotation(
    policy: Option<&MetadataPolicy>,
    fallback_label: &str,
    a: &mut Annotation,
) -> u64 {
    if a.content_class.permits_content() {
        return 0;
    }
    let (metadata, mut n) = sanitize_metadata(policy, std::mem::take(&mut a.metadata));
    a.metadata = metadata;
    let (label, k) = allowlisted_or(
        policy.map_or(&[], |p| p.labels.as_slice()),
        &a.value,
        fallback_label,
    );
    a.value = label;
    n += k;
    a.confidence = quantize_confidence(a.confidence);
    n
}

/// Whether metadata has a shape some policy could have produced: an object of numbers, booleans
/// and token strings (no nesting, no whitespace or punctuation text). Policy-agnostic: a
/// necessary, not sufficient, condition for having been sanitised.
pub fn metadata_is_allowlist_shaped(metadata: &Value) -> bool {
    metadata.as_object().is_some_and(|m| {
        m.iter().all(|(k, v)| {
            is_token(k)
                && match v {
                    Value::Bool(_) | Value::Number(_) => true,
                    Value::String(s) => is_token(s),
                    _ => false,
                }
        })
    })
}

fn identity_is_allowlist_shaped(id: Option<&DecodedIdentity>) -> bool {
    id.is_none_or(|id| {
        charset_ok(&id.value, Charset::Hex, MAX_ALLOWLIST_LEN)
            || charset_ok(&id.value, Charset::Digits, MAX_ALLOWLIST_LEN)
    })
}

/// Policy-agnostic shape check for a restricted Decode row (always true when the class permits
/// content): see [`metadata_is_allowlist_shaped`].
pub fn decode_is_allowlist_shaped(d: &Decode) -> bool {
    d.content_class.permits_content()
        || (metadata_is_allowlist_shaped(&d.metadata)
            && is_token(&d.frame_model)
            && identity_is_allowlist_shaped(d.identity.as_ref()))
}

/// Policy-agnostic shape check for a restricted Annotation row (always true when the class
/// permits content).
pub fn annotation_is_allowlist_shaped(a: &Annotation) -> bool {
    a.content_class.permits_content()
        || (metadata_is_allowlist_shaped(&a.metadata)
            && is_token(&a.value)
            && quantize_confidence(a.confidence) == a.confidence)
}
