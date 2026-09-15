//! Small constructors for block descriptors and parameter schemas.

use hk_recipe::{BlockDescriptor, ParamSchema, ParamType, PortSpec};
use serde_json::Value;

/// An optional, cold parameter.
pub fn param(name: &str, ty: ParamType, doc: &str) -> ParamSchema {
    ParamSchema {
        name: name.into(),
        ty,
        required: false,
        default: None,
        hot: false,
        doc: doc.into(),
    }
}

/// Modifiers on a [`ParamSchema`].
pub trait ParamExt {
    /// Must be present.
    fn required(self) -> Self;
    /// Applied in place without a rebuild.
    fn hot(self) -> Self;
    /// Documented default.
    fn default_value(self, v: impl Into<Value>) -> Self;
}

impl ParamExt for ParamSchema {
    fn required(mut self) -> Self {
        self.required = true;
        self
    }
    fn hot(mut self) -> Self {
        self.hot = true;
        self
    }
    fn default_value(mut self, v: impl Into<Value>) -> Self {
        self.default = Some(v.into());
        self
    }
}

/// `bool`.
pub fn boolean() -> ParamType {
    ParamType::Bool
}

/// Integer in `[min, max]`.
pub fn int(min: i64, max: i64) -> ParamType {
    ParamType::Int {
        min: Some(min),
        max: Some(max),
    }
}

/// Number in `[min, max]` with a unit.
pub fn float(min: f64, max: f64, unit: &str) -> ParamType {
    ParamType::Float {
        min: Some(min),
        max: Some(max),
        unit: (!unit.is_empty()).then(|| unit.into()),
    }
}

/// One of `values`.
pub fn one_of(values: &[&str]) -> ParamType {
    ParamType::Enum {
        values: values.iter().map(|v| (*v).into()).collect(),
    }
}

/// Short string.
pub fn string(max_len: u32) -> ParamType {
    ParamType::String { max_len }
}

/// Hex value of at most `bits`.
pub fn hex(bits: u32) -> ParamType {
    ParamType::Hex { max_bits: bits }
}

/// List of `item` with at least `min_len` elements.
pub fn list(item: ParamType, min_len: u32) -> ParamType {
    ParamType::List {
        item: Box::new(item),
        min_len,
        max_len: None,
    }
}

/// Nested object.
pub fn object(fields: Vec<ParamSchema>) -> ParamType {
    ParamType::Object { fields }
}

/// A version-1 descriptor.
pub fn descriptor(
    name: &str,
    group: &str,
    doc: &str,
    inputs: Vec<PortSpec>,
    outputs: Vec<PortSpec>,
    params: Vec<ParamSchema>,
    params_pinned: bool,
) -> BlockDescriptor {
    BlockDescriptor {
        name: name.into(),
        version: 1,
        group: group.into(),
        doc: doc.into(),
        inputs,
        outputs,
        params,
        params_pinned,
    }
}
