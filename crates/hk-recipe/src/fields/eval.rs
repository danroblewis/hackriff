//! Field-map evaluator (T-089, ADR-0011 §3.2–3.3): evaluates a [`FieldMap`] against one frame into
//! an [`hk_stream::inspector::LayerTree`].
//!
//! [`Evaluator::new`] validates the map and compiles it once: units become bit counts, enum
//! tables are sorted, and every reference (`condition`, `length`, `repeat`) is resolved to a
//! value slot, so evaluating a frame does no name lookups. [`Evaluator::eval`] then lays the
//! fields out over the frame's bits (MSB of byte 0 first) and returns the layer tree with
//! absolute bit and byte ranges per node, the per-byte leaf index for linked selection, and the
//! frame's fit.
//!
//! **Never aborts.** A field that does not fit (out of bounds, a missing reference, a negative
//! length, a failed parity check) is marked `error` on its node and listed in `errors`; its
//! later siblings are still evaluated. The frame's fit is `ok` (no errors), `partial` (errors,
//! but some field decoded) or `failed` (nothing decoded).
//!
//! Values: integers read MSB first; `skip_bits` drops bits (positions in air order) before the
//! remaining bits are concatenated; `bit_order: lsb` reverses the result (the first bit on air
//! is the least significant); otherwise `endianness: little` swaps whole bytes. `int` sign
//! extends from the width left after skipping. `scale`/`add` turn the raw integer into a JSON
//! number, rendered with `value_unit`. Conditions, lengths and repeats read the **raw** integer.

use hk_stream::inspector::{
    FitError, FitErrorKind, FitStatus, FrameRecord, LayerNode, LayerTree, MAX_LAYER_NODES,
    NodeType, byte_span, from_hex,
};
use serde_json::{Number, Value};

use super::{
    BitOrder, Charset, Compare, Condition, Display, Endianness, Field, FieldMap, FieldMapError,
    FieldType, Length, MAX_REPEAT, Parity,
};

/// Largest integer a JSON number carries exactly (2⁵³).
const JSON_SAFE: u64 = 1 << 53;
/// `bytes` nodes render at most this many bytes of hex in `text`.
const BYTES_TEXT_MAX: usize = 32;
/// POCSAG numeric characters (ITU-R M.584) for codes 0x0–0xF.
const POCSAG_BCD: &[u8; 16] = b"0123456789*U -][";

/// A field map compiled for evaluation. Cheap to share (`Clone`), immutable, `Send + Sync`.
#[derive(Clone, Debug)]
pub struct Evaluator {
    fields: Vec<CField>,
    slots: usize,
}

#[derive(Clone, Debug)]
enum CLen {
    /// Layers without a length: to the end of the enclosing layer.
    Unset,
    Bits(u64),
    Remainder,
    From {
        slot: usize,
        scale: i128,
        add: i128,
    },
}

#[derive(Clone, Debug)]
enum CCond {
    All(Vec<CCond>),
    Any(Vec<CCond>),
    Not(Box<CCond>),
    Cmp { slot: usize, cmp: Compare },
}

#[derive(Clone, Debug)]
struct CField {
    name: String,
    label: Option<String>,
    ty: FieldType,
    offset_bits: Option<u64>,
    length: CLen,
    repeat: Option<CLen>,
    condition: Option<CCond>,
    endianness: Endianness,
    bit_order: BitOrder,
    /// Value slot of a non-repeated integer field.
    slot: Option<usize>,
    /// Slots of this field and its descendants, cleared when an instance starts or is absent.
    slot_range: (usize, usize),
    values: Vec<(u64, String)>,
    flags: Vec<(String, u32)>,
    charset: Charset,
    char_bits: u32,
    parity: Option<Parity>,
    skip_bits: Vec<u32>,
    scale: Option<f64>,
    add: Option<f64>,
    value_unit: Option<String>,
    display: Option<Display>,
    children: Vec<CField>,
}

struct Declared {
    path: String,
    scope: String,
    name: String,
    slot: Option<usize>,
}

struct Compiler<'a> {
    map: &'a FieldMap,
    declared: Vec<Declared>,
    slots: usize,
    errors: Vec<FieldMapError>,
}

impl Evaluator {
    /// Validates `map` ([`FieldMap::validate`]) and compiles it.
    pub fn new(map: &FieldMap) -> Result<Self, Vec<FieldMapError>> {
        map.validate()?;
        let mut c = Compiler {
            map,
            declared: Vec::new(),
            slots: 0,
            errors: Vec::new(),
        };
        let fields = c.layer(&map.fields, "");
        if c.errors.is_empty() {
            Ok(Self {
                fields,
                slots: c.slots,
            })
        } else {
            Err(c.errors)
        }
    }

    /// Evaluates the map over a frame of `bit_len` bits packed MSB-first in `bytes` (a
    /// `bit_len` beyond `bytes` is clamped to it). Never panics; misfits are reported in the
    /// tree.
    pub fn eval(&self, bytes: &[u8], bit_len: u32) -> LayerTree {
        let bit_len = u64::from(bit_len).min(bytes.len() as u64 * 8);
        let mut run = Run {
            bytes,
            slots: vec![None; self.slots],
            nodes: Vec::new(),
            errors: Vec::new(),
            decoded: 0,
            stopped: false,
            path: String::new(),
        };
        run.layer(&self.fields, 0, bit_len, None);
        let fit = if run.errors.is_empty() {
            FitStatus::Ok
        } else if run.decoded > 0 {
            FitStatus::Partial
        } else {
            FitStatus::Failed
        };
        let mut tree = LayerTree {
            nodes: run.nodes,
            byte_index: Vec::new(),
            fit,
            errors: run.errors,
        };
        tree.index_bytes(bit_len.div_ceil(8) as usize);
        tree
    }

    /// Evaluates the map over a recorded frame record's `content.hex` and `metadata.bit_len`
    /// (§14.7 re-parse). `None` when the record cannot be parsed: stored gated (no content),
    /// its class forbids content, or its hex is malformed.
    pub fn eval_record(&self, rec: &FrameRecord) -> Option<LayerTree> {
        if rec.gated || !rec.content_class.permits_content() {
            return None;
        }
        let content = rec.content.as_ref()?;
        let bytes = from_hex(&content.hex)?;
        let bit_len = rec
            .metadata
            .bit_len
            .unwrap_or(u32::try_from(bytes.len() * 8).unwrap_or(u32::MAX));
        Some(self.eval(&bytes, bit_len))
    }
}

impl FieldMap {
    /// Validates, compiles and evaluates in one call (compile once with [`Evaluator::new`] when
    /// evaluating many frames).
    pub fn evaluate(&self, bytes: &[u8], bit_len: u32) -> Result<LayerTree, Vec<FieldMapError>> {
        Ok(Evaluator::new(self)?.eval(bytes, bit_len))
    }
}

impl Compiler<'_> {
    fn layer(&mut self, fields: &[Field], scope: &str) -> Vec<CField> {
        fields.iter().map(|f| self.field(f, scope)).collect()
    }

    fn field(&mut self, f: &Field, scope: &str) -> CField {
        let path = if scope.is_empty() {
            f.name.clone()
        } else {
            format!("{scope}.{}", f.name)
        };
        let unit = u64::from(f.unit.unwrap_or(self.map.unit).bits());
        // References resolve against fields declared before this one (as in validation).
        let condition = f
            .condition
            .as_ref()
            .map(|c| self.condition(c, &path, scope));
        let length = match &f.length {
            None => CLen::Unset,
            Some(l) => self.length(l, unit, &path, scope),
        };
        let repeat = f.repeat.as_ref().map(|l| self.length(l, 1, &path, scope));
        let first_slot = self.slots;
        let slot = (f.ty.is_integer() && f.repeat.is_none()).then(|| {
            self.slots += 1;
            first_slot
        });
        let children = if f.ty == FieldType::Layer {
            self.layer(&f.fields, &path)
        } else {
            Vec::new()
        };
        self.declared.push(Declared {
            path,
            scope: scope.to_owned(),
            name: f.name.clone(),
            slot,
        });
        let bcd = f.charset == Some(Charset::PocsagBcd);
        let mut values: Vec<(u64, String)> = f
            .values
            .iter()
            .filter_map(|(k, v)| Some((k.parse().ok()?, v.clone())))
            .collect();
        values.sort_by_key(|(k, _)| *k);
        CField {
            name: f.name.clone(),
            label: f.label.clone(),
            ty: f.ty,
            offset_bits: f.offset.map(|o| u64::from(o) * unit),
            length,
            repeat,
            condition,
            endianness: f.endianness.unwrap_or(self.map.endianness),
            bit_order: f.bit_order.unwrap_or(self.map.bit_order),
            slot,
            slot_range: (first_slot, self.slots),
            values,
            flags: f.flags.iter().map(|x| (x.name.clone(), x.bit)).collect(),
            charset: f.charset.unwrap_or_default(),
            char_bits: u32::from(f.char_bits.unwrap_or(if bcd { 4 } else { 8 })),
            parity: f.parity,
            skip_bits: f.skip_bits.clone(),
            scale: f.scale,
            add: f.add,
            value_unit: f.value_unit.clone(),
            display: f.display,
            children,
        }
    }

    fn length(&mut self, l: &Length, unit: u64, path: &str, scope: &str) -> CLen {
        match l {
            Length::Fixed(n) => CLen::Bits(u64::from(*n) * unit),
            Length::Keyword(_) => CLen::Remainder,
            Length::FromField(from) => CLen::From {
                slot: self.resolve(&from.field, path, scope),
                scale: i128::from(from.scale) * i128::from(unit),
                add: i128::from(from.add) * i128::from(unit),
            },
        }
    }

    fn condition(&mut self, c: &Condition, path: &str, scope: &str) -> CCond {
        match c {
            Condition::All(a) => CCond::All(
                a.all
                    .iter()
                    .map(|c| self.condition(c, path, scope))
                    .collect(),
            ),
            Condition::Any(a) => CCond::Any(
                a.any
                    .iter()
                    .map(|c| self.condition(c, path, scope))
                    .collect(),
            ),
            Condition::Not(n) => CCond::Not(Box::new(self.condition(&n.not, path, scope))),
            Condition::Compare(cmp) => CCond::Cmp {
                slot: self.resolve(&cmp.field, path, scope),
                cmp: cmp.clone(),
            },
        }
    }

    /// The slot of the referenced field: a dotted path from the root, else the nearest earlier
    /// sibling, then up through the ancestors.
    fn resolve(&mut self, r: &str, path: &str, scope: &str) -> usize {
        let found = if r.contains('.') {
            self.declared.iter().rev().find(|d| d.path == r)
        } else {
            let mut s = scope;
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
                s = s.rsplit_once('.').map_or("", |(p, _)| p);
            }
        };
        match found.and_then(|d| d.slot) {
            Some(slot) => slot,
            None => {
                self.errors.push(FieldMapError {
                    path: path.to_owned(),
                    message: "reference to an unknown, later or non-integer field".into(),
                });
                0
            }
        }
    }
}

/// One frame's evaluation state.
struct Run<'a> {
    bytes: &'a [u8],
    slots: Vec<Option<i128>>,
    nodes: Vec<LayerNode>,
    errors: Vec<FitError>,
    /// Non-layer nodes that decoded.
    decoded: usize,
    stopped: bool,
    /// Path of the enclosing layer instance.
    path: String,
}

/// How much of a field a node covers and whether it failed.
struct Placed {
    start: u64,
    len: u64,
    /// Bits actually available from `start` inside the enclosing layer.
    have: u64,
}

impl Run<'_> {
    fn error(&mut self, path: &str, kind: FitErrorKind, need: Option<u64>, have: Option<u64>) {
        self.errors.push(FitError {
            path: path.to_owned(),
            kind,
            need_bits: need,
            have_bits: have,
        });
    }

    fn clear(&mut self, (a, b): (usize, usize)) {
        self.slots[a..b].fill(None);
    }

    fn cond(&self, c: &CCond) -> Option<bool> {
        match c {
            CCond::All(v) => {
                for c in v {
                    if !self.cond(c)? {
                        return Some(false);
                    }
                }
                Some(true)
            }
            CCond::Any(v) => {
                for c in v {
                    if self.cond(c)? {
                        return Some(true);
                    }
                }
                Some(false)
            }
            CCond::Not(c) => self.cond(c).map(|b| !b),
            CCond::Cmp { slot, cmp } => self.slots[*slot].map(|v| cmp.holds(v)),
        }
    }

    /// Evaluates `fields` inside `[start, end)` bits.
    fn layer(&mut self, fields: &[CField], start: u64, end: u64, parent: Option<u32>) {
        let mut cursor = Some(start);
        for f in fields {
            if self.stopped {
                return;
            }
            let path = child_path(&self.path, &f.name, None);
            if let Some(c) = &f.condition {
                match self.cond(c) {
                    Some(true) => {}
                    Some(false) => {
                        self.clear(f.slot_range);
                        continue;
                    }
                    None => {
                        self.clear(f.slot_range);
                        self.error(&path, FitErrorKind::MissingReference, None, None);
                        continue;
                    }
                }
            }
            // Positions and lengths come from frame data: sums saturate or are checked. A
            // saturated position lies past every frame's end, so it reads as out of bounds.
            let Some(mut pos) = f.offset_bits.map(|o| start.saturating_add(o)).or(cursor) else {
                self.clear(f.slot_range);
                self.error(&path, FitErrorKind::MissingReference, None, None);
                continue;
            };
            let Some(repeat) = &f.repeat else {
                cursor = self
                    .instance(f, pos, end, parent, None)
                    .map(|n| pos.saturating_add(n));
                continue;
            };
            let count = match repeat {
                CLen::Bits(n) => Some(*n),
                CLen::Remainder | CLen::Unset => None,
                CLen::From { slot, scale, add } => match self.slots[*slot] {
                    None => {
                        self.error(&path, FitErrorKind::MissingReference, None, None);
                        cursor = None;
                        continue;
                    }
                    Some(v) => match scaled(v, *scale, *add) {
                        Some(n) => Some(n),
                        None => {
                            self.error(&path, FitErrorKind::BadLength, None, None);
                            cursor = None;
                            continue;
                        }
                    },
                },
            };
            let mut i = 0u64;
            let mut placed = true;
            loop {
                if count.is_some_and(|n| i >= n) || self.stopped {
                    break;
                }
                if count.is_none() {
                    // `remainder`: as many whole instances as fit, silently.
                    match self.length_of(f, pos, end) {
                        Ok(len) if len > 0 && pos.checked_add(len).is_some_and(|e| e <= end) => {}
                        _ => break,
                    }
                }
                if i >= u64::from(MAX_REPEAT) {
                    self.error(&path, FitErrorKind::RepeatLimit, None, None);
                    break;
                }
                match self.instance(f, pos, end, parent, Some(i)) {
                    Some(n) => pos = pos.saturating_add(n),
                    None => {
                        placed = false;
                        break;
                    }
                }
                i += 1;
            }
            cursor = placed.then_some(pos);
        }
    }

    /// A field's length in bits at `pos` inside a layer ending at `end`.
    fn length_of(&self, f: &CField, pos: u64, end: u64) -> Result<u64, FitErrorKind> {
        Ok(match &f.length {
            CLen::Bits(n) => *n,
            CLen::Unset | CLen::Remainder => {
                let rem = end.saturating_sub(pos);
                if f.ty == FieldType::Ascii {
                    rem - rem % u64::from(f.char_bits)
                } else {
                    rem
                }
            }
            CLen::From { slot, scale, add } => {
                let v = self.slots[*slot].ok_or(FitErrorKind::MissingReference)?;
                scaled(v, *scale, *add).ok_or(FitErrorKind::BadLength)?
            }
        })
    }

    /// Evaluates one instance of `f` at `pos`; returns the bits it occupies (`None` when its
    /// length could not be computed, so later siblings cannot be placed after it).
    fn instance(
        &mut self,
        f: &CField,
        pos: u64,
        end: u64,
        parent: Option<u32>,
        index: Option<u64>,
    ) -> Option<u64> {
        self.clear(f.slot_range);
        let path = child_path(&self.path, &f.name, index);
        if self.nodes.len() >= MAX_LAYER_NODES {
            self.error(&path, FitErrorKind::NodeLimit, None, None);
            self.stopped = true;
            return None;
        }
        let len = match self.length_of(f, pos, end) {
            Ok(len) => len,
            Err(kind) => {
                self.push(f, &path, index, parent, pos, 0, None, None, true);
                self.error(&path, kind, None, None);
                return None;
            }
        };
        let p = Placed {
            start: pos,
            len,
            have: end.saturating_sub(pos),
        };
        let fits = p.start.checked_add(p.len).is_some_and(|e| e <= end);
        if !fits {
            self.error(&path, FitErrorKind::OutOfBounds, Some(p.len), Some(p.have));
        }
        match f.ty {
            FieldType::Layer => {
                let id = self.push(
                    f,
                    &path,
                    index,
                    parent,
                    p.start,
                    p.len.min(p.have),
                    None,
                    None,
                    !fits,
                );
                let saved = std::mem::replace(&mut self.path, path);
                let inner_end = p.start.saturating_add(p.len).min(end);
                self.layer(&f.children, p.start, inner_end, Some(id));
                self.path = saved;
            }
            _ if !fits => {
                self.push(
                    f,
                    &path,
                    index,
                    parent,
                    p.start,
                    p.have.min(p.len),
                    None,
                    None,
                    true,
                );
            }
            FieldType::Uint | FieldType::Int | FieldType::Enum | FieldType::Bitfield => {
                self.integer(f, &path, index, parent, &p);
            }
            FieldType::Ascii => self.ascii(f, &path, index, parent, &p),
            FieldType::Bytes => {
                let text = self.bytes_text(p.start, p.len);
                self.push(
                    f,
                    &path,
                    index,
                    parent,
                    p.start,
                    p.len,
                    None,
                    Some(text),
                    false,
                );
                self.decoded += 1;
            }
        }
        Some(p.len)
    }

    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        f: &CField,
        path: &str,
        index: Option<u64>,
        parent: Option<u32>,
        start: u64,
        len: u64,
        value: Option<Value>,
        text: Option<String>,
        error: bool,
    ) -> u32 {
        let id = self.nodes.len() as u32;
        let name = match index {
            None => f.name.clone(),
            Some(i) => format!("{}[{i}]", f.name),
        };
        let (start, len) = (clamp_u32(start), clamp_u32(len));
        self.nodes.push(LayerNode {
            id,
            parent,
            name,
            path: path.to_owned(),
            ty: node_type(f.ty),
            bits: [start, len],
            bytes: byte_span(start, len),
            value,
            text,
            label: f.label.clone(),
            error,
        });
        id
    }

    fn integer(
        &mut self,
        f: &CField,
        path: &str,
        index: Option<u64>,
        parent: Option<u32>,
        p: &Placed,
    ) {
        let n = p.len as u32;
        let air = read_bits(self.bytes, p.start, n);
        let (mut raw, width) = if f.skip_bits.is_empty() {
            (air, n)
        } else {
            let mut v = 0u64;
            let mut w = 0u32;
            for k in 0..n {
                if f.skip_bits.binary_search(&k).is_err() {
                    v = (v << 1) | ((air >> (n - 1 - k)) & 1);
                    w += 1;
                }
            }
            (v, w)
        };
        if f.bit_order == BitOrder::Lsb {
            raw = reverse_bits(raw, width);
        } else if f.endianness == Endianness::Little && width % 8 == 0 {
            raw = swap_bytes(raw, width);
        }
        let signed = if f.ty == FieldType::Int {
            sign_extend(raw, width)
        } else {
            i128::from(raw)
        };
        if let (Some(slot), None) = (f.slot, index) {
            self.slots[slot] = Some(signed);
        }
        let (value, text) = if f.scale.is_some() || f.add.is_some() {
            let x = signed as f64 * f.scale.unwrap_or(1.0) + f.add.unwrap_or(0.0);
            let mut text = fmt_f64(x);
            if let Some(u) = &f.value_unit {
                text.push(' ');
                text.push_str(u);
            }
            (Number::from_f64(x).map(Value::Number), text)
        } else {
            let value = if signed.unsigned_abs() <= u128::from(JSON_SAFE) {
                if f.ty == FieldType::Int {
                    Value::from(signed as i64)
                } else {
                    Value::from(raw)
                }
            } else {
                Value::String(signed.to_string())
            };
            let mut text = match f.ty {
                FieldType::Enum => match f.values.binary_search_by_key(&raw, |(k, _)| *k) {
                    Ok(i) => f.values[i].1.clone(),
                    Err(_) => raw.to_string(),
                },
                _ => render_int(f.display, raw, signed, width),
            };
            if let Some(u) = &f.value_unit {
                text.push(' ');
                text.push_str(u);
            }
            (Some(value), text)
        };
        let id = self.push(
            f,
            path,
            index,
            parent,
            p.start,
            p.len,
            value,
            Some(text),
            false,
        );
        self.decoded += 1;
        if f.ty == FieldType::Bitfield {
            for (name, bit) in &f.flags {
                if self.nodes.len() >= MAX_LAYER_NODES {
                    self.error(path, FitErrorKind::NodeLimit, None, None);
                    self.stopped = true;
                    return;
                }
                let on = (air >> (n - 1 - bit)) & 1 == 1;
                let start = clamp_u32(p.start + u64::from(*bit));
                self.nodes.push(LayerNode {
                    id: self.nodes.len() as u32,
                    parent: Some(id),
                    name: name.clone(),
                    path: format!("{path}.{name}"),
                    ty: NodeType::Flag,
                    bits: [start, 1],
                    bytes: byte_span(start, 1),
                    value: Some(Value::Bool(on)),
                    text: Some(on.to_string()),
                    label: None,
                    error: false,
                });
            }
        }
    }

    fn ascii(
        &mut self,
        f: &CField,
        path: &str,
        index: Option<u64>,
        parent: Option<u32>,
        p: &Placed,
    ) {
        let cb = f.char_bits;
        let count = p.len / u64::from(cb);
        let mut s = String::with_capacity(count as usize);
        let mut parity_failed = false;
        for i in 0..count {
            let mut code = read_bits(self.bytes, p.start + i * u64::from(cb), cb);
            if f.bit_order == BitOrder::Lsb {
                code = reverse_bits(code, cb);
            }
            let (data, data_bits) = match f.parity {
                None => (code, cb),
                Some(parity) => {
                    let ones = code.count_ones();
                    let good = match parity {
                        Parity::Odd => ones % 2 == 1,
                        Parity::Even => ones % 2 == 0,
                        Parity::Ignore => true,
                    };
                    if !good {
                        parity_failed = true;
                        s.push('\u{FFFD}');
                        continue;
                    }
                    (code & ((1 << (cb - 1)) - 1), cb - 1)
                }
            };
            push_char(&mut s, f.charset, data as u8, data_bits);
        }
        if parity_failed {
            self.error(path, FitErrorKind::Parity, None, None);
        }
        let text = s.clone();
        self.push(
            f,
            path,
            index,
            parent,
            p.start,
            count * u64::from(cb),
            Some(Value::String(s)),
            Some(text),
            parity_failed,
        );
        self.decoded += 1;
    }

    fn bytes_text(&self, start: u64, len: u64) -> String {
        let n = len.div_ceil(8) as usize;
        let mut s = String::with_capacity(2 + 2 * n.min(BYTES_TEXT_MAX) + 3);
        s.push_str("0x");
        for i in 0..n.min(BYTES_TEXT_MAX) {
            let bits = (len - (i as u64) * 8).min(8) as u32;
            let b = read_bits(self.bytes, start + i as u64 * 8, bits) << (8 - bits);
            s.push_str(&format!("{b:02x}"));
        }
        if n > BYTES_TEXT_MAX {
            s.push('…');
        }
        s
    }
}

fn child_path(scope: &str, name: &str, index: Option<u64>) -> String {
    let mut p = String::with_capacity(scope.len() + name.len() + 8);
    if !scope.is_empty() {
        p.push_str(scope);
        p.push('.');
    }
    p.push_str(name);
    if let Some(i) = index {
        p.push('[');
        p.push_str(&i.to_string());
        p.push(']');
    }
    p
}

/// A length or count read from a field: `v × scale + add`, `None` when negative or beyond u64.
fn scaled(v: i128, scale: i128, add: i128) -> Option<u64> {
    v.checked_mul(scale)
        .and_then(|x| x.checked_add(add))
        .and_then(|x| u64::try_from(x).ok())
}

fn clamp_u32(x: u64) -> u32 {
    u32::try_from(x).unwrap_or(u32::MAX)
}

const fn node_type(t: FieldType) -> NodeType {
    match t {
        FieldType::Uint => NodeType::Uint,
        FieldType::Int => NodeType::Int,
        FieldType::Enum => NodeType::Enum,
        FieldType::Ascii => NodeType::Ascii,
        FieldType::Bitfield => NodeType::Bitfield,
        FieldType::Bytes => NodeType::Bytes,
        FieldType::Layer => NodeType::Layer,
    }
}

/// `n ≤ 64` bits at bit `off` (MSB of byte 0 = bit 0); bits past `bytes` read as 0.
fn read_bits(bytes: &[u8], off: u64, n: u32) -> u64 {
    if n == 0 {
        return 0;
    }
    let first = (off / 8) as usize;
    let lead = (off % 8) as u32;
    let total = lead + n;
    let nbytes = total.div_ceil(8) as usize;
    let mut acc: u128 = 0;
    for i in 0..nbytes {
        let b = first
            .checked_add(i)
            .and_then(|k| bytes.get(k))
            .copied()
            .unwrap_or(0);
        acc = (acc << 8) | u128::from(b);
    }
    let extra = nbytes as u32 * 8 - total;
    ((acc >> extra) & ((1u128 << n) - 1)) as u64
}

fn reverse_bits(v: u64, width: u32) -> u64 {
    if width == 0 {
        0
    } else {
        v.reverse_bits() >> (64 - width)
    }
}

fn swap_bytes(v: u64, width: u32) -> u64 {
    if width == 0 {
        0
    } else {
        v.swap_bytes() >> (64 - width)
    }
}

fn sign_extend(v: u64, width: u32) -> i128 {
    if width == 0 || width >= 64 {
        return i128::from(v as i64);
    }
    let shift = 64 - width;
    i128::from(((v << shift) as i64) >> shift)
}

fn render_int(display: Option<Display>, raw: u64, signed: i128, width: u32) -> String {
    match display {
        None | Some(Display::Dec) => signed.to_string(),
        Some(Display::Hex) => format!("0x{raw:0w$X}", w = width.div_ceil(4) as usize),
        Some(Display::Bin) => format!("0b{raw:0w$b}", w = width as usize),
        Some(Display::Bool) => (raw != 0).to_string(),
    }
}

fn fmt_f64(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 1e15 {
        format!("{x:.0}")
    } else {
        format!("{x}")
    }
}

/// Renders 8-bit character codes in `charset` exactly as `ascii` fields render them
/// (unprintable codes as `\xNN`); shared with the `text` block.
pub fn decode_chars(charset: Charset, codes: &[u8]) -> String {
    let mut s = String::with_capacity(codes.len());
    for &c in codes {
        push_char(&mut s, charset, c, 8);
    }
    s
}

fn push_char(s: &mut String, charset: Charset, code: u8, bits: u32) {
    let printable = match charset {
        Charset::PocsagBcd => {
            s.push(char::from(POCSAG_BCD[usize::from(code & 0x0f)]));
            return;
        }
        Charset::Ascii => (0x20..=0x7e).contains(&code) && bits <= 8,
        Charset::Latin1 => (0x20..=0x7e).contains(&code) || code >= 0xa0,
        Charset::Rds => (0x20..=0x7d).contains(&code),
    };
    if printable {
        s.push(char::from(code));
    } else {
        s.push_str(&format!("\\x{code:02X}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(v: Value) -> FieldMap {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn bit_helpers() {
        assert_eq!(read_bits(&[0b1010_0101, 0xff], 4, 8), 0b0101_1111);
        assert_eq!(
            read_bits(
                &[0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x11],
                4,
                64
            ),
            0x2345_6789_abcd_ef01
        );
        assert_eq!(
            read_bits(&[0xff], 6, 8),
            0b1100_0000,
            "past the end reads 0"
        );
        assert_eq!(reverse_bits(0b0001, 4), 0b1000);
        assert_eq!(swap_bytes(0x1234, 16), 0x3412);
        assert_eq!(sign_extend(0b1110, 4), -2);
    }

    #[test]
    fn endianness_bit_order_int_and_display() {
        let m = map(json!({
            "fields": [
                {"name": "be", "type": "uint", "length": 2, "display": "hex"},
                {"name": "le", "type": "uint", "length": 2, "endianness": "little", "display": "hex"},
                {"name": "neg", "type": "int", "length": 1},
                {"name": "nib", "type": "uint", "length": 4, "unit": "bits", "bit_order": "lsb", "display": "bin"}
            ]
        }));
        let t = m
            .evaluate(&[0x12, 0x34, 0x12, 0x34, 0xfe, 0x10], 48)
            .unwrap();
        assert_eq!(t.fit, FitStatus::Ok, "{:?}", t.errors);
        let n = |p: &str| t.node(p).unwrap();
        assert_eq!(n("be").text.as_deref(), Some("0x1234"));
        assert_eq!(n("le").value, Some(json!(0x3412)));
        assert_eq!(n("neg").value, Some(json!(-2)));
        assert_eq!(n("nib").bits, [40, 4]);
        assert_eq!(n("nib").text.as_deref(), Some("0b1000"));
        assert_eq!(t.fields_at_byte(5), &[3]);
    }

    #[test]
    fn repeats_lengths_and_remainder() {
        let m = map(json!({
            "fields": [
                {"name": "count", "type": "uint", "length": 1},
                {"name": "item", "type": "layer", "length": 2, "repeat": {"field": "count"},
                 "fields": [{"name": "id", "type": "uint", "length": 1},
                            {"name": "v", "type": "uint", "length": 1, "condition": {"field": "id", "eq": 7}}]},
                {"name": "tail_len", "type": "uint", "length": 1},
                {"name": "tail", "type": "bytes", "length": {"field": "tail_len"}},
                {"name": "rest", "type": "uint", "length": 1, "repeat": "remainder"}
            ]
        }));
        let t = m.evaluate(&[2, 7, 9, 1, 0, 1, 0xaa, 5, 6], 72).unwrap();
        assert_eq!(t.fit, FitStatus::Ok, "{:?}", t.errors);
        assert_eq!(t.node("item[0].v").unwrap().value, Some(json!(9)));
        assert!(
            t.node("item[1].v").is_none(),
            "condition false in instance 1"
        );
        assert_eq!(t.node("tail").unwrap().text.as_deref(), Some("0xaa"));
        assert_eq!(t.node("rest[1]").unwrap().value, Some(json!(6)));
        assert_eq!(t.node("item[1]").unwrap().bytes, [3, 5]);
        // Layers are not leaves: byte 1 maps to item[0].id only.
        let id = t.node("item[0].id").unwrap().id;
        assert_eq!(t.fields_at_byte(1), &[id]);
    }

    #[test]
    fn misfits_are_reported_per_field_and_never_abort() {
        let m = map(json!({
            "unit": "bits",
            "fields": [
                {"name": "a", "type": "uint", "length": 8},
                {"name": "long", "type": "uint", "length": 32},
                {"name": "after", "type": "uint", "offset": 8, "length": 4},
                {"name": "n", "type": "uint", "offset": 12, "length": 4},
                {"name": "items", "type": "bytes", "offset": 16, "length": {"field": "n", "add": -20}},
                {"name": "next", "type": "uint", "length": 1}
            ]
        }));
        let t = m.evaluate(&[0xab, 0xc3], 16).unwrap();
        assert_eq!(t.fit, FitStatus::Partial);
        let kinds: Vec<_> = t.errors.iter().map(|e| (e.path.as_str(), e.kind)).collect();
        assert_eq!(
            kinds,
            [
                ("long", FitErrorKind::OutOfBounds),
                ("items", FitErrorKind::BadLength),
                ("next", FitErrorKind::MissingReference)
            ]
        );
        assert_eq!(t.errors[0].need_bits, Some(32));
        assert_eq!(t.errors[0].have_bits, Some(8));
        assert!(t.node("long").unwrap().error);
        assert_eq!(t.node("after").unwrap().value, Some(json!(0xc)));

        let empty = m.evaluate(&[], 0).unwrap();
        assert_eq!(empty.fit, FitStatus::Failed);
        assert!(empty.byte_index.is_empty());
    }

    /// Over-the-air lengths near u64::MAX must misfit, never overflow (debug panic) or wrap into
    /// a huge allocation (release abort).
    #[test]
    fn data_derived_lengths_near_u64_max_misfit_without_overflow() {
        let frame = [0xff; 16];
        for ty in ["bytes", "ascii"] {
            let m = map(json!({
                "unit": "bits",
                "fields": [
                    {"name": "len", "type": "uint", "length": 64},
                    {"name": "body", "type": ty, "length": {"field": "len"}},
                    {"name": "next", "type": "uint", "length": 8}
                ]
            }));
            let t = m.evaluate(&frame, 128).unwrap();
            assert_eq!(t.fit, FitStatus::Partial, "{ty}");
            let kinds: Vec<_> = t.errors.iter().map(|e| (e.path.as_str(), e.kind)).collect();
            assert_eq!(
                kinds,
                [
                    ("body", FitErrorKind::OutOfBounds),
                    ("next", FitErrorKind::OutOfBounds)
                ],
                "{ty}"
            );
            assert_eq!(t.errors[0].need_bits, Some(u64::MAX));
            assert_eq!(t.errors[0].have_bits, Some(64));
            assert!(t.node("body").unwrap().error);
        }

        // Repeats: a huge instance length under `remainder`, a huge count, and huge instances
        // repeated by count all end in misfits.
        let m = map(json!({
            "unit": "bits",
            "fields": [
                {"name": "len", "type": "uint", "length": 64},
                {"name": "rest", "type": "bytes", "length": {"field": "len"}, "repeat": "remainder"},
                {"name": "items", "type": "uint", "length": 8, "repeat": {"field": "len"}}
            ]
        }));
        let t = m.evaluate(&frame, 128).unwrap();
        assert!(t.node("rest[0]").is_none(), "no whole instance fits");
        assert_eq!(t.node("items[7]").unwrap().value, Some(json!(0xff)));
        assert_eq!(
            t.errors.first().map(|e| (e.path.as_str(), e.kind)),
            Some(("items[8]", FitErrorKind::OutOfBounds))
        );
        let limited = |t: &LayerTree| {
            t.errors
                .iter()
                .any(|e| matches!(e.kind, FitErrorKind::RepeatLimit | FitErrorKind::NodeLimit))
        };
        assert!(limited(&t));

        let m = map(json!({
            "unit": "bits",
            "fields": [
                {"name": "len", "type": "uint", "length": 64},
                {"name": "big", "type": "layer", "length": {"field": "len"}, "repeat": {"field": "len"},
                 "fields": [{"name": "x", "type": "uint", "length": 8}]}
            ]
        }));
        let t = m.evaluate(&frame, 128).unwrap();
        assert_eq!(t.node("big[0].x").unwrap().value, Some(json!(0xff)));
        assert!(t.node("big[1]").unwrap().error);
        assert!(
            t.errors
                .iter()
                .any(|e| e.path == "big[1].x" && e.kind == FitErrorKind::OutOfBounds)
        );
        assert!(limited(&t));
    }

    #[test]
    fn ascii_charsets_parity_and_bcd() {
        let m = map(json!({
            "unit": "bits",
            "fields": [
                {"name": "odd", "type": "ascii", "length": 16, "parity": "odd"},
                {"name": "bcd", "type": "ascii", "length": 8, "charset": "pocsag-bcd"},
                {"name": "ctl", "type": "ascii", "length": 8}
            ]
        }));
        // 'A' = 0x41 has two ones → odd parity sets bit 7: 0xC1. 'B' = 0x42 sent with bad parity.
        let t = m.evaluate(&[0xc1, 0x42, 0x1c, 0x0d], 32).unwrap();
        assert_eq!(t.node("odd").unwrap().value, Some(json!("A\u{FFFD}")));
        assert_eq!(t.errors[0].kind, FitErrorKind::Parity);
        assert_eq!(t.fit, FitStatus::Partial);
        assert_eq!(t.node("bcd").unwrap().value, Some(json!("1 ")));
        assert_eq!(t.node("ctl").unwrap().text.as_deref(), Some("\\x0D"));
    }
}
