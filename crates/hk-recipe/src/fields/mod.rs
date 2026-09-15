//! Declarative parser field maps (ADR-0011 §3).
//!
//! A [`FieldMap`] describes how to read one frame (a byte record with a bit length) as a tree of
//! named fields nested into layers. It is pure data: the `fields` block (T-089) evaluates it
//! against each frame and produces an [`hk_stream::inspector::LayerTree`] whose nodes carry
//! absolute bit and byte ranges, so the inspector can link a field to its bytes and back.
//!
//! **Layout rules.**
//! - Bits are numbered from the first bit of the frame, **MSB of byte 0 first** (the frame
//!   packing rule, `docs/stream-contract.md` §14.2).
//! - `offset` is relative to the start of the enclosing layer (the frame for top-level fields),
//!   in the field's `unit`. Omitted, the field starts where the previous *present* sibling ended
//!   (the layer start for the first one).
//! - `length` is in `unit` (bits or bytes; the map's `unit`, default bytes, unless the field
//!   overrides it). A layer without `length` extends to the end of its enclosing layer.
//! - `endianness` orders the bytes of multi-byte integers (whole-byte fields only);
//!   `bit_order` orders bits within a sub-byte or odd-width field and within `ascii` characters
//!   (`lsb` for POCSAG's 4-bit BCD and 7-bit LSB-first text).
//! - Integer fields may drop bits (`skip_bits`, e.g. the ADS-B altitude Q bit) and carry a
//!   linear `scale`/`add` plus a `value_unit`, so the layer tree holds physical values and the
//!   UI does no arithmetic.
//! - `condition` is evaluated before the field is laid out; a false condition makes the field
//!   absent (not an error).
//! - `repeat` lays the field out `count` times in sequence; each instance is addressed as
//!   `name[i]`. `repeat: "remainder"` repeats while a whole instance still fits.
//! - References (`condition.field`, `length.field`, `repeat.field`) name an **earlier**
//!   integer-valued field: a bare name resolves to the nearest earlier sibling, then up through
//!   the ancestors; a dotted path (`group.version`) resolves from the root.
//!
//! **Evaluation never panics and never aborts the frame.** A field that does not fit is
//! reported on its node and in the tree's `errors` (`FitError`), its later siblings are still
//! tried, and the frame's `fit` is `partial` or `failed` (§3.3).

pub mod eval;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Largest integer field, bits.
pub const MAX_INT_BITS: u32 = 64;
/// Deepest layer nesting.
pub const MAX_DEPTH: usize = 16;
/// Most fields declared in one map (bounds evaluation cost per frame).
pub const MAX_FIELDS: usize = 4096;
/// Most instances of one repeated field in one frame.
pub const MAX_REPEAT: u32 = 65_536;

/// Length and offset unit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Unit {
    /// Bits.
    Bits,
    /// Bytes (8 bits).
    #[default]
    Bytes,
}

impl Unit {
    /// Bits per unit.
    pub const fn bits(self) -> u32 {
        match self {
            Unit::Bits => 1,
            Unit::Bytes => 8,
        }
    }
}

/// Byte order of multi-byte integers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Endianness {
    /// Most significant byte first.
    #[default]
    Big,
    /// Least significant byte first.
    Little,
}

/// Bit order within sub-byte fields and characters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BitOrder {
    /// First bit is the most significant.
    #[default]
    Msb,
    /// First bit is the least significant.
    Lsb,
}

/// Field type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FieldType {
    /// Unsigned integer, ≤ 64 bits.
    Uint,
    /// Two's-complement signed integer, ≤ 64 bits.
    Int,
    /// Unsigned integer with named `values`; an unnamed value is shown numerically (not an error).
    Enum,
    /// Characters of `char_bits` (8, 7, or 4 for `pocsag-bcd`) in `charset`, optionally with a
    /// parity bit per character.
    Ascii,
    /// Named single-bit `flags` over an integer ≤ 64 bits.
    Bitfield,
    /// Raw bytes/bits with no interpretation (unknown regions while authoring).
    Bytes,
    /// A container of `fields`.
    Layer,
}

impl FieldType {
    /// Integer-valued (can be referenced by conditions, lengths and repeats).
    pub const fn is_integer(self) -> bool {
        matches!(
            self,
            FieldType::Uint | FieldType::Int | FieldType::Enum | FieldType::Bitfield
        )
    }
}

/// Character set of an `ascii` field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Charset {
    /// 7-bit US-ASCII; other codes are shown as `\xNN`.
    #[default]
    Ascii,
    /// ISO 8859-1.
    Latin1,
    /// RDS basic character set G0 (EN 50067 Annex E; ASCII-compatible for 0x20–0x7D).
    Rds,
    /// POCSAG numeric (ITU-R M.584): 4-bit codes `0`–`9`, then A spare (shown `*`), B `U`,
    /// C space, D `-`, E `]`, F `[`. Only with `char_bits: 4`.
    PocsagBcd,
}

/// Parity bit of each `ascii` character: the **most significant** bit of the `char_bits`-bit
/// code after `bit_order` is applied (ACARS: 7-bit ASCII + odd parity, sent LSB first, so the
/// parity bit is the last on air). The bit is masked out of the character; with `odd`/`even` a
/// failing character renders as U+FFFD and the node carries a `parity` error (a fit error, not
/// a static one).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Parity {
    /// Odd parity, checked.
    Odd,
    /// Even parity, checked.
    Even,
    /// Masked out, not checked.
    Ignore,
}

/// How a value is rendered in the layer tree's `text`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Display {
    /// Decimal (integers' default).
    Dec,
    /// `0x…`, zero-padded to the field width.
    Hex,
    /// `0b…`.
    Bin,
    /// `true`/`false` (1-bit fields).
    Bool,
}

/// A length or count.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Length {
    /// A fixed number of units (or instances).
    Fixed(u32),
    /// `"remainder"`: to the end of the enclosing layer (or as many instances as fit).
    Keyword(LengthKeyword),
    /// `value(field) × scale + add`, e.g. a length byte counting bytes after a header.
    FromField(LengthFrom),
}

/// The `"remainder"` keyword.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LengthKeyword {
    /// To the end of the enclosing layer.
    Remainder,
}

fn one() -> u32 {
    1
}

fn is_one(x: &u32) -> bool {
    *x == 1
}

fn is_zero(x: &i64) -> bool {
    *x == 0
}

fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

/// A length read from an earlier field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LengthFrom {
    /// Reference to an earlier integer field.
    pub field: String,
    /// Multiplier.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub scale: u32,
    /// Addend (may be negative; a negative result is a fit error).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub add: i64,
}

/// A presence condition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Condition {
    /// Every condition holds.
    All(AllOf),
    /// At least one holds.
    Any(AnyOf),
    /// The condition does not hold.
    Not(NotOf),
    /// A comparison of an earlier integer field with constants.
    Compare(Compare),
}

/// `{"all": [...]}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllOf {
    /// Conditions.
    pub all: Vec<Condition>,
}

/// `{"any": [...]}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnyOf {
    /// Conditions.
    pub any: Vec<Condition>,
}

/// `{"not": {...}}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotOf {
    /// Condition.
    pub not: Box<Condition>,
}

/// `{"field": "group_type", "eq": 0}`: exactly one operator. Integers compare as `i128`, so a
/// 64-bit unsigned field compares correctly with any `i64` constant.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compare {
    /// Reference to an earlier integer field.
    pub field: String,
    /// Equal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eq: Option<i64>,
    /// Not equal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ne: Option<i64>,
    /// Less than.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lt: Option<i64>,
    /// Less or equal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub le: Option<i64>,
    /// Greater than.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gt: Option<i64>,
    /// Greater or equal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ge: Option<i64>,
    /// One of.
    #[serde(default, rename = "in", skip_serializing_if = "Option::is_none")]
    pub one_of: Option<Vec<i64>>,
}

impl Compare {
    fn operators(&self) -> usize {
        [self.eq, self.ne, self.lt, self.le, self.gt, self.ge]
            .iter()
            .filter(|o| o.is_some())
            .count()
            + usize::from(self.one_of.is_some())
    }

    /// Whether `value` satisfies the comparison.
    pub fn holds(&self, value: i128) -> bool {
        let c = |x: Option<i64>| x.map(i128::from);
        c(self.eq).is_none_or(|x| value == x)
            && c(self.ne).is_none_or(|x| value != x)
            && c(self.lt).is_none_or(|x| value < x)
            && c(self.le).is_none_or(|x| value <= x)
            && c(self.gt).is_none_or(|x| value > x)
            && c(self.ge).is_none_or(|x| value >= x)
            && self
                .one_of
                .as_ref()
                .is_none_or(|s| s.iter().any(|x| i128::from(*x) == value))
    }
}

/// A named bit of a `bitfield`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Flag {
    /// Flag name.
    pub name: String,
    /// Bit position counted from the field's first bit (0 = first bit on the air).
    pub bit: u32,
}

/// One field or layer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    /// Name, `[a-z][a-z0-9_]*`, unique among its siblings.
    pub name: String,
    /// Type.
    #[serde(rename = "type")]
    pub ty: FieldType,
    /// Human label for the layer tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Offset from the enclosing layer's start, in `unit`; omitted = after the previous present
    /// sibling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
    /// Length in `unit`; required except for layers (default: remainder).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<Length>,
    /// Unit override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<Unit>,
    /// Endianness override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endianness: Option<Endianness>,
    /// Bit-order override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_order: Option<BitOrder>,
    /// Presence condition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<Condition>,
    /// Repeat count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<Length>,
    /// `enum`: value (decimal string) → name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub values: BTreeMap<String, String>,
    /// `bitfield`: named bits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<Flag>,
    /// `ascii`: character set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charset: Option<Charset>,
    /// `ascii`: bits per character (8, 7, or 4 with `pocsag-bcd`; default 8, or 4 for
    /// `pocsag-bcd`). A trailing partial character of a `remainder`-length field is padding,
    /// not an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub char_bits: Option<u8>,
    /// `ascii`: per-character parity bit (absent: none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parity: Option<Parity>,
    /// `uint`/`int`: bit positions (0 = the field's first bit) left out of the value; the
    /// remaining bits are concatenated in order (ADS-B AC12 altitude without its Q bit).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skip_bits: Vec<u32>,
    /// `uint`/`int`: the value is `raw × scale + add` (JSON number; `text` renders it with
    /// `value_unit`). Absent: 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    /// `uint`/`int`: addend after `scale`. Absent: 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add: Option<f64>,
    /// `uint`/`int`: unit of the (scaled) value, a short token (`ft`, `kt`, `ft/min`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_unit: Option<String>,
    /// Rendering of the value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<Display>,
    /// `layer`: children.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<Field>,
}

/// A field map.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldMap {
    /// Description.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Default unit for offsets and lengths.
    #[serde(default, skip_serializing_if = "is_default")]
    pub unit: Unit,
    /// Default endianness.
    #[serde(default, skip_serializing_if = "is_default")]
    pub endianness: Endianness,
    /// Default bit order.
    #[serde(default, skip_serializing_if = "is_default")]
    pub bit_order: BitOrder,
    /// Top-level fields (the root layer is the whole frame).
    pub fields: Vec<Field>,
}

/// A static field-map error (the map itself is malformed, whatever the frame).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldMapError {
    /// Dotted path of the field (`radiotext.chars_a`), empty for the map.
    pub path: String,
    /// What is wrong.
    pub message: String,
}

impl std::fmt::Display for FieldMapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// `[a-z][a-z0-9_]*`, at most 64 bytes.
pub fn is_field_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.as_bytes()[0].is_ascii_lowercase()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// A dotted path of field names, each optionally indexed: `group.pi`, `items[3].id`.
pub fn is_field_path(s: &str) -> bool {
    !s.is_empty()
        && s.split('.').all(|seg| match seg.split_once('[') {
            None => is_field_name(seg),
            Some((name, idx)) => {
                is_field_name(name)
                    && idx
                        .strip_suffix(']')
                        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            }
        })
}

/// A field declared earlier in pre-order, for reference resolution.
struct Declared {
    path: String,
    /// Path of the enclosing layer ("" for the root).
    scope: String,
    name: String,
    integer: bool,
}

struct Checker<'a> {
    map: &'a FieldMap,
    declared: Vec<Declared>,
    count: usize,
    errors: Vec<FieldMapError>,
}

impl FieldMap {
    /// Static validation: names, type-specific keys, integer widths, fixed-size fit, references
    /// to earlier integer fields, conditions with exactly one operator, depth and size bounds.
    /// Frame-dependent problems (a variable length that runs past the frame) are fit errors at
    /// evaluation time instead.
    pub fn validate(&self) -> Result<(), Vec<FieldMapError>> {
        let mut c = Checker {
            map: self,
            declared: Vec::new(),
            count: 0,
            errors: Vec::new(),
        };
        if self.fields.is_empty() {
            c.error("", "a field map needs at least one field");
        }
        c.layer(&self.fields, "", None, 1);
        if c.errors.is_empty() {
            Ok(())
        } else {
            Err(c.errors)
        }
    }
}

impl Checker<'_> {
    fn error(&mut self, path: &str, message: impl Into<String>) {
        self.errors.push(FieldMapError {
            path: path.to_owned(),
            message: message.into(),
        });
    }

    /// Checks `fields` laid out inside a layer at `scope` of fixed size `layer_bits` (if known).
    fn layer(&mut self, fields: &[Field], scope: &str, layer_bits: Option<u64>, depth: usize) {
        if depth > MAX_DEPTH {
            self.error(scope, format!("layers nest deeper than {MAX_DEPTH}"));
            return;
        }
        let mut names = std::collections::BTreeSet::new();
        // End of the previous sibling, bits, when statically known.
        let mut cursor: Option<u64> = Some(0);
        for f in fields {
            self.count += 1;
            if self.count > MAX_FIELDS {
                self.error(scope, format!("more than {MAX_FIELDS} fields"));
                return;
            }
            let path = if scope.is_empty() {
                f.name.clone()
            } else {
                format!("{scope}.{}", f.name)
            };
            if !is_field_name(&f.name) {
                self.error(&path, "field names are [a-z][a-z0-9_]*");
            }
            if !names.insert(f.name.as_str()) {
                self.error(&path, "duplicate field name in this layer");
            }
            if let Some(cond) = &f.condition {
                self.condition(cond, &path, scope);
            }
            let unit = f.unit.unwrap_or(self.map.unit).bits();
            let fixed_bits = self.field(f, &path, scope, unit, depth);
            if let Some(r) = &f.repeat {
                self.length_ref(r, &path, scope, "repeat");
                if matches!(r, Length::Fixed(0)) || matches!(r, Length::Fixed(n) if *n > MAX_REPEAT)
                {
                    self.error(&path, format!("repeat count must be 1..={MAX_REPEAT}"));
                }
            }
            let start = match f.offset {
                Some(o) => Some(u64::from(o) * u64::from(unit)),
                None => cursor,
            };
            let count = match &f.repeat {
                None => Some(1u64),
                Some(Length::Fixed(n)) => Some(u64::from(*n)),
                Some(_) => None,
            };
            let end = match (start, fixed_bits, count) {
                (Some(s), Some(b), Some(n)) => Some(s + b * n),
                _ => None,
            };
            if let (Some(e), Some(l)) = (end, layer_bits)
                && e > l
            {
                self.error(
                    &path,
                    format!("ends at bit {e}, past the end of its {l}-bit layer"),
                );
            }
            // A conditional field may be absent: the next sibling's start is then unknown.
            cursor = if f.condition.is_some() { None } else { end };
            self.declared.push(Declared {
                path,
                scope: scope.to_owned(),
                name: f.name.clone(),
                integer: f.ty.is_integer() && f.repeat.is_none(),
            });
        }
    }

    /// Checks one field; returns its fixed size in bits when statically known.
    fn field(
        &mut self,
        f: &Field,
        path: &str,
        scope: &str,
        unit: u32,
        depth: usize,
    ) -> Option<u64> {
        let len_bits = match &f.length {
            Some(Length::Fixed(0)) => {
                self.error(path, "length must be positive");
                None
            }
            Some(Length::Fixed(n)) => Some(u64::from(*n) * u64::from(unit)),
            Some(other) => {
                self.length_ref(other, path, scope, "length");
                None
            }
            None => None,
        };
        let is = |t: FieldType| f.ty == t;
        if !f.values.is_empty() && !is(FieldType::Enum) {
            self.error(path, "`values` is only valid on enum fields");
        }
        if !f.flags.is_empty() && !is(FieldType::Bitfield) {
            self.error(path, "`flags` is only valid on bitfield fields");
        }
        if (f.charset.is_some() || f.char_bits.is_some() || f.parity.is_some())
            && !is(FieldType::Ascii)
        {
            self.error(
                path,
                "`charset`/`char_bits`/`parity` are only valid on ascii fields",
            );
        }
        let scaled = f.scale.is_some() || f.add.is_some() || f.value_unit.is_some();
        if (scaled || !f.skip_bits.is_empty()) && !matches!(f.ty, FieldType::Uint | FieldType::Int)
        {
            self.error(
                path,
                "`skip_bits`/`scale`/`add`/`value_unit` are only valid on uint and int fields",
            );
        }
        if f.scale.is_some_and(|x| !x.is_finite() || x == 0.0)
            || f.add.is_some_and(|x| !x.is_finite())
        {
            self.error(path, "scale must be finite and non-zero, add finite");
        }
        if f.value_unit.as_deref().is_some_and(|u| {
            u.is_empty() || u.len() > 16 || !u.bytes().all(|b| b.is_ascii_graphic())
        }) {
            self.error(path, "value_unit is 1–16 printable ASCII characters");
        }
        if f.skip_bits.windows(2).any(|w| w[0] >= w[1])
            || len_bits.is_some_and(|b| {
                f.skip_bits.last().is_some_and(|&k| u64::from(k) >= b)
                    || f.skip_bits.len() as u64 >= b
            })
        {
            self.error(
                path,
                "skip_bits are ascending, inside the field, and leave at least one bit",
            );
        }
        if !f.fields.is_empty() && !is(FieldType::Layer) {
            self.error(path, "`fields` is only valid on layers");
        }
        if f.display.is_some() && !f.ty.is_integer() {
            self.error(path, "`display` is only valid on integer fields");
        }
        match f.ty {
            FieldType::Layer => {
                if f.fields.is_empty() {
                    self.error(path, "a layer needs at least one field");
                }
                self.layer(&f.fields, path, len_bits, depth + 1);
                return len_bits;
            }
            FieldType::Uint | FieldType::Int | FieldType::Enum | FieldType::Bitfield => {
                match len_bits {
                    None => self.error(path, "integer fields need a fixed length"),
                    Some(b) if b > u64::from(MAX_INT_BITS) => self.error(
                        path,
                        format!("integer fields are at most {MAX_INT_BITS} bits"),
                    ),
                    Some(_) => {}
                }
                if f.endianness == Some(Endianness::Little) && len_bits.is_some_and(|b| b % 8 != 0)
                {
                    self.error(path, "little-endian fields must be whole bytes");
                }
            }
            FieldType::Ascii => {
                let bcd = f.charset == Some(Charset::PocsagBcd);
                let cb = f.char_bits.unwrap_or(if bcd { 4 } else { 8 });
                if bcd != (cb == 4) {
                    self.error(
                        path,
                        "char_bits 4 goes with charset pocsag-bcd, and only it",
                    );
                }
                if f.parity.is_some() && cb == 4 {
                    self.error(path, "4-bit characters have no parity bit");
                }
                if ![4, 7, 8].contains(&cb) {
                    self.error(path, "char_bits is 4, 7 or 8");
                } else if len_bits.is_some_and(|b| b % u64::from(cb) != 0) {
                    self.error(path, "ascii length must be a whole number of characters");
                }
                if f.length.is_none() {
                    self.error(path, "ascii fields need a length");
                }
            }
            FieldType::Bytes => {
                if f.length.is_none() {
                    self.error(path, "bytes fields need a length");
                }
            }
        }
        if is(FieldType::Enum) {
            if f.values.is_empty() {
                self.error(path, "enum fields need `values`");
            }
            if f.values.keys().any(|k| k.parse::<u64>().is_err()) {
                self.error(path, "enum value keys are decimal integers");
            }
        }
        if is(FieldType::Bitfield) {
            if f.flags.is_empty() {
                self.error(path, "bitfield fields need `flags`");
            }
            let mut seen = std::collections::BTreeSet::new();
            for flag in &f.flags {
                if !is_field_name(&flag.name) || !seen.insert(flag.name.as_str()) {
                    self.error(path, "flag names are unique [a-z][a-z0-9_]*");
                }
                if len_bits.is_some_and(|b| u64::from(flag.bit) >= b) {
                    self.error(path, "flag bit outside the field");
                }
            }
        }
        len_bits
    }

    fn length_ref(&mut self, l: &Length, path: &str, scope: &str, what: &str) {
        if let Length::FromField(from) = l {
            if from.scale == 0 {
                self.error(path, format!("{what} scale must be positive"));
            }
            self.reference(&from.field, path, scope);
        }
    }

    fn condition(&mut self, c: &Condition, path: &str, scope: &str) {
        match c {
            Condition::All(a) => a.all.iter().for_each(|c| self.condition(c, path, scope)),
            Condition::Any(a) => a.any.iter().for_each(|c| self.condition(c, path, scope)),
            Condition::Not(n) => self.condition(&n.not, path, scope),
            Condition::Compare(cmp) => {
                if cmp.operators() != 1 {
                    self.error(path, "a comparison has exactly one operator");
                }
                self.reference(&cmp.field, path, scope);
            }
        }
    }

    /// Resolves a reference against fields declared so far.
    fn reference(&mut self, r: &str, path: &str, scope: &str) {
        if !is_field_path(r) || r.contains('[') {
            self.error(
                path,
                "references are field names or dotted paths (no indexes)",
            );
            return;
        }
        let found = if r.contains('.') {
            self.declared.iter().find(|d| d.path == r)
        } else {
            // Nearest enclosing scope first: the current layer, then each ancestor.
            let mut s = scope.to_owned();
            loop {
                if let Some(d) = self
                    .declared
                    .iter()
                    .rev()
                    .find(|d| d.scope == s && d.name == r)
                {
                    break Some(d);
                }
                if s.is_empty() {
                    break None;
                }
                s = s
                    .rsplit_once('.')
                    .map(|(p, _)| p.to_owned())
                    .unwrap_or_default();
            }
        };
        match found {
            None => self.error(path, "reference to an unknown or later field"),
            Some(d) if !d.integer => {
                self.error(path, "references must name a non-repeated integer field")
            }
            Some(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn paths() {
        assert!(is_field_path("group.pi"));
        assert!(is_field_path("items[3].id"));
        assert!(!is_field_path("group..pi"));
        assert!(!is_field_path("Group"));
        assert!(!is_field_path("items[x]"));
    }

    #[test]
    fn reports_static_errors_by_path() {
        let map: FieldMap = serde_json::from_value(json!({
            "unit": "bits",
            "fields": [
                {"name": "hdr", "type": "layer", "length": 8, "fields": [
                    {"name": "kind", "type": "uint", "length": 4},
                    {"name": "wide", "type": "uint", "length": 8}
                ]},
                {"name": "text", "type": "ascii", "length": 12},
                {"name": "late", "type": "uint", "length": 4,
                 "condition": {"field": "later", "eq": 1}},
                {"name": "later", "type": "uint", "length": 70},
                {"name": "e", "type": "enum", "length": 2, "values": {"x": "bad"}},
                {"name": "two_ops", "type": "uint", "length": 1,
                 "condition": {"field": "hdr.kind", "eq": 1, "ne": 2}}
            ]
        }))
        .unwrap();
        let errors = map.validate().unwrap_err();
        let has = |p: &str, m: &str| errors.iter().any(|e| e.path == p && e.message.contains(m));
        assert!(has("hdr.wide", "past the end"), "{errors:?}");
        assert!(has("text", "whole number of characters"), "{errors:?}");
        assert!(has("late", "unknown or later"), "{errors:?}");
        assert!(has("later", "at most 64"), "{errors:?}");
        assert!(has("e", "decimal integers"), "{errors:?}");
        assert!(has("two_ops", "exactly one operator"), "{errors:?}");
    }

    #[test]
    fn pocsag_acars_and_adsb_value_keys_validate() {
        let map: FieldMap = serde_json::from_value(json!({
            "unit": "bits",
            "fields": [
                {"name": "ric", "type": "uint", "length": 23, "skip_bits": [18, 19]},
                {"name": "function", "type": "uint", "offset": 18, "length": 2},
                {"name": "numeric", "type": "ascii", "offset": 23, "length": "remainder",
                 "charset": "pocsag-bcd", "bit_order": "lsb",
                 "condition": {"field": "function", "eq": 0}},
                {"name": "label", "type": "ascii", "offset": 23, "length": 16,
                 "parity": "odd", "condition": {"field": "function", "eq": 3}},
                {"name": "alt", "type": "uint", "offset": 40, "length": 12, "skip_bits": [7],
                 "scale": 25, "add": -1000, "value_unit": "ft"}
            ]
        }))
        .unwrap();
        map.validate().unwrap();

        let bad: FieldMap = serde_json::from_value(json!({
            "unit": "bits",
            "fields": [
                {"name": "a", "type": "ascii", "length": 8, "char_bits": 4},
                {"name": "b", "type": "ascii", "length": 8, "charset": "pocsag-bcd", "parity": "odd"},
                {"name": "c", "type": "enum", "length": 4, "values": {"0": "x"}, "scale": 2},
                {"name": "d", "type": "uint", "length": 4, "skip_bits": [2, 1]},
                {"name": "e", "type": "uint", "length": 4, "scale": 0}
            ]
        }))
        .unwrap();
        let errors = bad.validate().unwrap_err();
        let has = |p: &str, m: &str| errors.iter().any(|e| e.path == p && e.message.contains(m));
        assert!(has("a", "pocsag-bcd"), "{errors:?}");
        assert!(has("b", "no parity"), "{errors:?}");
        assert!(has("c", "only valid on uint and int"), "{errors:?}");
        assert!(has("d", "ascending"), "{errors:?}");
        assert!(has("e", "non-zero"), "{errors:?}");
    }

    #[test]
    fn compare_semantics() {
        let c = Compare {
            field: "x".into(),
            one_of: Some(vec![1, 3]),
            ..Default::default()
        };
        assert!(c.holds(3) && !c.holds(2));
        let big = Compare {
            field: "x".into(),
            gt: Some(0),
            ..Default::default()
        };
        assert!(big.holds(i128::from(u64::MAX)));
    }
}
