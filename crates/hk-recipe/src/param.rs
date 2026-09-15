//! Block parameter schemas, block descriptors and the catalogue a recipe validates against
//! (ADR-0011 §1.2).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::port::PortType;

/// A node's parameter values: a JSON object, validated against the block's [`ParamSchema`]s.
pub type Params = serde_json::Map<String, Value>;

/// The type and constraints of one parameter.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ParamType {
    /// `true`/`false`.
    Bool,
    /// An integer in `[min, max]`.
    Int {
        /// Inclusive minimum.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<i64>,
        /// Inclusive maximum.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<i64>,
    },
    /// A finite number in `[min, max]`.
    Float {
        /// Inclusive minimum.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<f64>,
        /// Inclusive maximum.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<f64>,
        /// Unit for display (`Hz`, `Bd`, `s`, `dB`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unit: Option<String>,
    },
    /// One of `values`.
    Enum {
        /// Allowed tokens.
        values: Vec<String>,
    },
    /// A string of at most `max_len` bytes.
    String {
        /// Longest value, bytes.
        max_len: u32,
    },
    /// A hex string `0x…` (JSON has no hex literals) whose value fits `max_bits`: polynomials,
    /// sync words, offset words. Parse with [`parse_hex`].
    Hex {
        /// Widest value, bits (≤ 64).
        max_bits: u32,
    },
    /// The id of a field map in the recipe's `field_maps`.
    FieldMap,
    /// A dotted field path into a frame's layer tree, e.g. `group.pi`, `ps.chars`.
    FieldPath,
    /// A list of `item`s.
    List {
        /// Element type.
        item: Box<ParamType>,
        /// Fewest elements.
        #[serde(default)]
        min_len: u32,
        /// Most elements.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_len: Option<u32>,
    },
    /// A nested object with its own schema (unknown keys are errors).
    Object {
        /// Member schemas.
        fields: Vec<ParamSchema>,
    },
}

/// One parameter of a block.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParamSchema {
    /// Key in the node's `params` object.
    pub name: String,
    /// Type and constraints.
    #[serde(flatten)]
    pub ty: ParamType,
    /// Must be present.
    #[serde(default)]
    pub required: bool,
    /// Value used when absent (documentation for the UI; the block applies it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    /// Changing it is applied in place at the next chunk boundary without resetting the block
    /// (`Block::update_params` returns `Applied`); otherwise the block is rebuilt (ADR-0011 §2.3).
    #[serde(default)]
    pub hot: bool,
    /// One-line description.
    #[serde(default)]
    pub doc: String,
}

/// A parameter validation error. Messages never echo the offending value (they may reach the
/// API, docs/api.md "Errors").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamError {
    /// Dotted path of the parameter, e.g. `reference.multiple` or `offsets[2]`.
    pub path: String,
    /// What is wrong.
    pub message: String,
}

impl std::fmt::Display for ParamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// Parses a `0x…` hex string (at most 16 digits).
pub fn parse_hex(s: &str) -> Option<u64> {
    let digits = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    if digits.is_empty() || digits.len() > 16 {
        return None;
    }
    u64::from_str_radix(digits, 16).ok()
}

impl ParamSchema {
    /// Validates `params` against `schemas`: unknown keys, missing required keys, types and
    /// ranges. Field-map references are checked against `field_maps` (ids). Cross-parameter
    /// constraints (e.g. `sync_word` needed when `mode` is `sync-word`) are the block's own
    /// build-time check.
    pub fn validate_all(
        schemas: &[ParamSchema],
        params: &Params,
        field_maps: &[&str],
    ) -> Vec<ParamError> {
        let mut errors = Vec::new();
        validate_object(schemas, params, "", field_maps, &mut errors);
        errors
    }
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}.{name}")
    }
}

fn validate_object(
    schemas: &[ParamSchema],
    params: &Params,
    prefix: &str,
    field_maps: &[&str],
    errors: &mut Vec<ParamError>,
) {
    for key in params.keys() {
        if !schemas.iter().any(|s| &s.name == key) {
            errors.push(ParamError {
                path: join(prefix, key),
                message: "unknown parameter".into(),
            });
        }
    }
    for s in schemas {
        let path = join(prefix, &s.name);
        match params.get(&s.name) {
            None if s.required => errors.push(ParamError {
                path,
                message: "required parameter missing".into(),
            }),
            None => {}
            Some(v) => validate_value(&s.ty, v, &path, field_maps, errors),
        }
    }
}

fn err(errors: &mut Vec<ParamError>, path: &str, message: impl Into<String>) {
    errors.push(ParamError {
        path: path.to_owned(),
        message: message.into(),
    });
}

fn validate_value(
    ty: &ParamType,
    v: &Value,
    path: &str,
    field_maps: &[&str],
    errors: &mut Vec<ParamError>,
) {
    match ty {
        ParamType::Bool => {
            if !v.is_boolean() {
                err(errors, path, "expected a boolean");
            }
        }
        ParamType::Int { min, max } => match v.as_i64() {
            None => err(errors, path, "expected an integer"),
            Some(n) => {
                if min.is_some_and(|m| n < m) || max.is_some_and(|m| n > m) {
                    err(errors, path, "integer out of range");
                }
            }
        },
        ParamType::Float { min, max, .. } => match v.as_f64() {
            Some(x) if x.is_finite() => {
                if min.is_some_and(|m| x < m) || max.is_some_and(|m| x > m) {
                    err(errors, path, "number out of range");
                }
            }
            _ => err(errors, path, "expected a finite number"),
        },
        ParamType::Enum { values } => match v.as_str() {
            Some(s) if values.iter().any(|x| x == s) => {}
            _ => err(errors, path, format!("expected one of {values:?}")),
        },
        ParamType::String { max_len } => match v.as_str() {
            Some(s) if s.len() <= *max_len as usize => {}
            Some(_) => err(errors, path, "string too long"),
            None => err(errors, path, "expected a string"),
        },
        ParamType::Hex { max_bits } => match v.as_str().and_then(parse_hex) {
            Some(x) if *max_bits >= 64 || x >> max_bits == 0 => {}
            Some(_) => err(
                errors,
                path,
                format!("hex value wider than {max_bits} bits"),
            ),
            None => err(errors, path, "expected a hex string 0x…"),
        },
        ParamType::FieldMap => match v.as_str() {
            Some(s) if field_maps.contains(&s) => {}
            Some(_) => err(errors, path, "no field map with this id in the recipe"),
            None => err(errors, path, "expected a field-map id"),
        },
        ParamType::FieldPath => match v.as_str() {
            Some(s) if crate::fields::is_field_path(s) => {}
            _ => err(errors, path, "expected a dotted field path"),
        },
        ParamType::List {
            item,
            min_len,
            max_len,
        } => match v.as_array() {
            None => err(errors, path, "expected a list"),
            Some(a) => {
                if a.len() < *min_len as usize || max_len.is_some_and(|m| a.len() > m as usize) {
                    err(errors, path, "list length out of range");
                }
                for (i, x) in a.iter().enumerate() {
                    validate_value(item, x, &format!("{path}[{i}]"), field_maps, errors);
                }
            }
        },
        ParamType::Object { fields } => match v.as_object() {
            None => err(errors, path, "expected an object"),
            Some(o) => validate_object(fields, o, path, field_maps, errors),
        },
    }
}

/// One port of a block.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PortSpec {
    /// Port name (`in`, `out`, `timing_error`, …).
    pub name: String,
    /// Accepted types (inputs may accept several, e.g. `clock_recovery` takes `iq` or `real`);
    /// outputs have exactly one.
    pub types: Vec<PortType>,
    /// A diagnostic output: computed only while a stage stream is tapped on it (ADR-0011 §1.3).
    #[serde(default)]
    pub diagnostic: bool,
}

impl PortSpec {
    /// A port of one type.
    pub fn new(name: &str, ty: PortType) -> Self {
        Self {
            name: name.into(),
            types: vec![ty],
            diagnostic: false,
        }
    }

    /// An input accepting any of `types`.
    pub fn any_of(name: &str, types: &[PortType]) -> Self {
        Self {
            name: name.into(),
            types: types.to_vec(),
            diagnostic: false,
        }
    }

    /// Marks the port diagnostic.
    pub fn diagnostic(mut self) -> Self {
        self.diagnostic = true;
        self
    }
}

/// What a block publishes about itself: served by `GET /api/blocks` (planned) and used to
/// validate recipes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BlockDescriptor {
    /// Block kind, e.g. `sync_search` (`[a-z0-9_]+`).
    pub name: String,
    /// Contract version of this block's ports and parameters. A change that makes an existing
    /// recipe invalid or changes its meaning bumps it; a node may pin it (`NodeSpec::version`).
    pub version: u32,
    /// Group: `iq`, `symbol`, `framing`, `fec`, `parse`, `multi`, `util`.
    pub group: String,
    /// One-line description.
    pub doc: String,
    /// Input ports.
    pub inputs: Vec<PortSpec>,
    /// Output ports.
    pub outputs: Vec<PortSpec>,
    /// Parameter schemas.
    pub params: Vec<ParamSchema>,
    /// The parameter schema is final. `false` only for catalogue placeholders of blocks not yet
    /// implemented (T-086/T-087): their params are accepted unchecked, with a warning.
    #[serde(default = "yes")]
    pub params_pinned: bool,
}

fn yes() -> bool {
    true
}

impl BlockDescriptor {
    /// The input port named `name`.
    pub fn input(&self, name: &str) -> Option<&PortSpec> {
        self.inputs.iter().find(|p| p.name == name)
    }

    /// The output port named `name`.
    pub fn output(&self, name: &str) -> Option<&PortSpec> {
        self.outputs.iter().find(|p| p.name == name)
    }
}

/// Where recipe validation looks blocks up. Implemented by `hk_blocks::Registry` (implemented
/// blocks) and by `hk_blocks::catalogue::planned()` (the M1 library as pinned by ADR-0011).
pub trait Catalogue {
    /// The descriptor of block kind `name`.
    fn descriptor(&self, name: &str) -> Option<&BlockDescriptor>;
}

impl Catalogue for BTreeMap<String, BlockDescriptor> {
    fn descriptor(&self, name: &str) -> Option<&BlockDescriptor> {
        self.get(name)
    }
}

impl Catalogue for [BlockDescriptor] {
    fn descriptor(&self, name: &str) -> Option<&BlockDescriptor> {
        self.iter().find(|d| d.name == name)
    }
}

impl Catalogue for Vec<BlockDescriptor> {
    fn descriptor(&self, name: &str) -> Option<&BlockDescriptor> {
        self.as_slice().descriptor(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schemas() -> Vec<ParamSchema> {
        serde_json::from_value(json!([
            {"name": "mode", "type": "enum", "values": ["a", "b"], "required": true},
            {"name": "poly", "type": "hex", "max_bits": 11},
            {"name": "rate", "type": "float", "min": 0.0, "unit": "Bd", "hot": true},
            {"name": "offsets", "type": "list", "item": {"type": "list", "item": {"type": "hex", "max_bits": 10}}},
            {"name": "map", "type": "field-map"},
            {"name": "reference", "type": "object", "fields": [
                {"name": "multiple", "type": "int", "min": 1, "max": 8}
            ]}
        ]))
        .unwrap()
    }

    #[test]
    fn schema_round_trips_flattened() {
        let s = schemas();
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v[1]["type"], "hex");
        assert_eq!(v[1]["max_bits"], 11);
        let back: Vec<ParamSchema> = serde_json::from_value(v).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn validates_types_ranges_and_references() {
        let ok = json!({"mode": "a", "poly": "0x5B9", "rate": 1187.5,
            "offsets": [["0x0FC"], ["0x168", "0x350"]], "map": "g", "reference": {"multiple": 3}});
        assert!(ParamSchema::validate_all(&schemas(), ok.as_object().unwrap(), &["g"]).is_empty());

        let bad = json!({"poly": "0x800", "rate": -1, "offsets": [["x"]], "map": "nope",
            "reference": {"multiple": 9, "extra": 1}, "typo": true});
        let errors = ParamSchema::validate_all(&schemas(), bad.as_object().unwrap(), &["g"]);
        let paths: Vec<&str> = errors.iter().map(|e| e.path.as_str()).collect();
        for p in [
            "mode",
            "poly",
            "rate",
            "offsets[0][0]",
            "map",
            "reference.multiple",
            "reference.extra",
            "typo",
        ] {
            assert!(paths.contains(&p), "{p} not in {paths:?}");
        }
    }

    #[test]
    fn hex_parsing() {
        assert_eq!(parse_hex("0x5B9"), Some(0x5B9));
        assert_eq!(parse_hex("5B9"), None);
        assert_eq!(parse_hex("0x"), None);
    }
}
