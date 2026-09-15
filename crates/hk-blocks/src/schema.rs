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

/// Variable-length framing (ADR-0011 §1.5 "Frame length"): `length_from` (a length decided by
/// a field of the frame, e.g. the ADS-B DF) and `terminator` (a closing word, e.g. ACARS ETX),
/// shared by every frame-producing block. A frame ends at the first of: its `length_from`
/// length, its terminator (plus `trailer_bits`), or the block's `frame_bits` (the maximum).
pub fn frame_length() -> Vec<ParamSchema> {
    let range = |name: &str, doc: &str| param(name, int(0, 1_000_000), doc);
    vec![
        param(
            "length_from",
            object(vec![
                range(
                    "offset_bits",
                    "First bit of the length field, from the frame start (after sync/preamble).",
                )
                .required(),
                param("bits", int(1, 32), "Width of the length field, MSB first.").required(),
                param(
                    "cases",
                    list(
                        object(vec![
                            param("min", int(0, i64::from(u32::MAX)), "Lowest value.").required(),
                            param("max", int(0, i64::from(u32::MAX)), "Highest value.").required(),
                            range("frame_bits", "Frame length for values in [min, max].")
                                .required(),
                        ]),
                        1,
                    ),
                    "Table: the first case holding the value sets the length (ADS-B: DF 16–31 → 112).",
                ),
                param(
                    "scale",
                    int(0, 65_536),
                    "No case matched: frame_bits = value × scale + add (0: use default_bits).",
                )
                .default_value(0),
                param("add", int(-1_000_000, 1_000_000), "Addend, bits.").default_value(0),
                range(
                    "default_bits",
                    "Length when no case matches and scale is 0 (ADS-B: 56).",
                ),
            ]),
            "Frame length decided by a field of the frame; absent: fixed frame_bits.",
        ),
        param(
            "terminator",
            object(vec![
                param("words", list(hex(32), 1), "Closing words (ACARS ETX 0x83, ETB 0x97 with parity).")
                    .required(),
                param("bits", int(1, 32), "Word width.").required(),
                param(
                    "step_bits",
                    int(1, 64),
                    "Checked only at multiples of this from the frame start (8: character-aligned).",
                )
                .default_value(1),
                param(
                    "trailer_bits",
                    int(0, 1_024),
                    "Bits kept after the terminator (ACARS: the 16-bit BCS).",
                )
                .default_value(0),
            ]),
            "Frame ends after a closing word; absent: none.",
        ),
    ]
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
