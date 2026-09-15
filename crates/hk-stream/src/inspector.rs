//! Inspector stream profile (`docs/stream-contract.md` §14, contract 1.2 draft; ADR-0011 §4).
//! **Core interface.**
//!
//! A decoder-workbench pipeline serves its frames to the packet inspector and to external
//! programs as a `messages` stream whose records are `frame` records: one per frame, carrying
//! the frame's raw bytes and bit length, where it came from (sample index, channel), which
//! recipe revision produced it, its check status and, when a field map ran, the layer tree with
//! bit and byte ranges for linked selection. `status` and `edit` records interleave.
//!
//! Frame records go through the §6 message gate like any message: `metadata` always flows
//! (reduced to the recipe's allowlist under a restricted class); `content` (bytes and layers) is
//! withheld when the effective class forbids content. Publishing is
//! [`crate::Publisher::publish_frame`] (the §6 gate and metadata policy) and
//! [`crate::Publisher::publish_record`] (`status`/`edit`).
//!
//! **Recorded decoded streams** (§14.7) are the §3 byte stream itself. [`RecordedFrames`] reads
//! one from any byte source (a capture file, a buffer), yielding its frame records;
//! [`CaptureSource`] is the interface a capture store implements so the inspector API can open a
//! recording by id. [`FitSummary`] aggregates field-map fit over many frames.

use std::collections::BTreeMap;
use std::io::Read;

use hk_model::{ContentClass, CrcStatus, EmitterId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::client::ClientError;
use crate::frame::{FrameDecoder, HEADER_MAX_LEN};
use crate::header::{StreamHeader, StreamKind};

/// Record `type` of a frame record.
pub const FRAME_RECORD_TYPE: &str = "frame";
/// Record `type` of a pipeline status record on an inspector stream.
pub const STATUS_RECORD_TYPE: &str = "status";
/// Record `type` of a hot-edit boundary record.
pub const EDIT_RECORD_TYPE: &str = "edit";
/// Header `message_schema` of an inspector stream.
pub const INSPECTOR_MESSAGE_SCHEMA: &str = "hackriff.inspector/1";
/// Largest layer tree served per frame (nodes); a bigger tree is truncated with a
/// `node-limit` fit error.
pub const MAX_LAYER_NODES: usize = 4096;

/// The header's `inspector` object (optional header field, 1.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InspectorProfile {
    /// Running pipeline id.
    pub pipeline_id: String,
    /// Recipe id.
    pub recipe_id: String,
    /// Saved recipe version the pipeline started from.
    pub recipe_version: u32,
    /// Output id within the recipe.
    pub output_id: String,
    /// Live pipeline or a recorded decoded stream.
    pub source: InspectorSource,
    /// Channels known at open (`follow_hops` adds more; records carry `channel_hz`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<ChannelInfo>,
}

/// Where the frames come from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum InspectorSource {
    /// A running pipeline.
    Live,
    /// A recorded decoded stream (T-092), optionally re-parsed with a different field map.
    Capture {
        /// Capture id.
        capture_id: String,
        /// Layers come from a re-parse, not the recording's own recipe revision.
        #[serde(default)]
        reparse: bool,
    },
}

/// One followed channel.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChannelInfo {
    /// Channel index (the record's `channel`).
    pub index: u16,
    /// Centre, Hz.
    pub center_hz: f64,
    /// Bandwidth, Hz.
    pub bandwidth_hz: f64,
}

/// A frame's evaluation against its field map.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FitStatus {
    /// No field map ran.
    #[default]
    None,
    /// Every present field fit.
    Ok,
    /// Some fields failed; others decoded.
    Partial,
    /// No field decoded.
    Failed,
}

/// Frame-record metadata. Flat numbers and tokens (`policy::metadata_is_allowlist_shaped`).
/// Every key is optional on the wire: under a restricted class the gate keeps only allowlisted
/// keys, so readers must not require any of them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FrameMetadata {
    /// Frame counter per pipeline output, from 0; a gap means frames were not produced
    /// (e.g. dropped as check-invalid), not lost in transit (that is `seq`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<u64>,
    /// Source sample index of the frame's first bit (the ring's stream counter, C03).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_index: Option<u64>,
    /// Channel index (0 unless `follow_hops`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u16>,
    /// Channel centre, Hz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_hz: Option<f64>,
    /// Frame length, bits (bytes are packed MSB-first, the last byte zero-padded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_len: Option<u32>,
    /// Saved recipe version the running revision derives from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe_version: Option<u32>,
    /// Live-edit revision of the pipeline (0 = as saved); increments per applied edit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit_rev: Option<u32>,
    /// Bits corrected by FEC before the check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fec_corrected_bits: Option<u32>,
    /// Field-map fit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit: Option<FitStatus>,
}

/// Frame content: present only when the effective class permits content.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FrameContent {
    /// Raw bytes, lower-case hex, `ceil(bit_len / 8)` bytes.
    pub hex: String,
    /// Parsed layer tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layers: Option<LayerTree>,
}

/// A `frame` record (§14.2), as serialised on the wire.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FrameRecord {
    /// `"frame"`.
    #[serde(rename = "type")]
    pub record_type: String,
    /// Per-stream sequence number (§5.1).
    pub seq: u64,
    /// Time of the first bit, ns since the Unix epoch (UTC).
    pub t: i64,
    /// Effective class after clamping (§6).
    pub content_class: ContentClass,
    /// `content` is withheld by class.
    pub gated: bool,
    /// Frame check (after FEC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crc_status: Option<CrcStatus>,
    /// Producer token, `recipe:<id>@<version>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decoder: Option<String>,
    /// Frame model: the recipe id, or the decode mapping's `frame_model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_model: Option<String>,
    /// Emitter, if the pipeline's target resolved to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emitter_id: Option<EmitterId>,
    /// Metadata (always flows, policy-reduced when restricted).
    pub metadata: FrameMetadata,
    /// Content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<FrameContent>,
}

/// Type of a layer-tree node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeType {
    /// Container.
    Layer,
    /// Unsigned integer.
    Uint,
    /// Signed integer.
    Int,
    /// Enumerated integer.
    Enum,
    /// Characters.
    Ascii,
    /// Integer with named flags (its flags are `flag` children).
    Bitfield,
    /// One named bit of a bitfield.
    Flag,
    /// Uninterpreted bits.
    Bytes,
}

/// One node of a parsed frame, in pre-order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayerNode {
    /// Index in [`LayerTree::nodes`].
    pub id: u32,
    /// Parent node (`None` for top-level fields).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<u32>,
    /// Field name (`name[i]` for a repeat instance).
    pub name: String,
    /// Dotted path from the root, e.g. `radiotext.chars_a`, `items[2].id`.
    pub path: String,
    /// Node type.
    #[serde(rename = "type")]
    pub ty: NodeType,
    /// `[offset, length]` in bits from the frame's first bit.
    pub bits: [u32; 2],
    /// `[first, end)` byte range covering `bits` (computed, [`byte_span`]).
    pub bytes: [u32; 2],
    /// Value: a number (|v| ≤ 2⁵³), a decimal string beyond that, a string for ascii, a bool
    /// for a flag; absent for layers, bytes and failed fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    /// Rendered value (`0x5B9`, `News`, `"BBC R4  "`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Human label from the field map.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// This node is where a fit error was reported (see [`LayerTree::errors`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub error: bool,
}

/// Why a field did not fit a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FitErrorKind {
    /// The field runs past the end of its layer or the frame.
    OutOfBounds,
    /// A length or repeat count computed from a field is negative.
    BadLength,
    /// A referenced field is absent in this frame (its condition was false or it failed), or
    /// the field's placement depends on a sibling whose length could not be computed.
    MissingReference,
    /// A repeat exceeded the instance limit.
    RepeatLimit,
    /// The tree exceeded [`MAX_LAYER_NODES`]; later fields were not evaluated.
    NodeLimit,
    /// An `ascii` character failed its parity check (rendered U+FFFD); reported once per field.
    Parity,
}

impl FitErrorKind {
    /// Wire token.
    pub const fn as_str(self) -> &'static str {
        match self {
            FitErrorKind::OutOfBounds => "out-of-bounds",
            FitErrorKind::BadLength => "bad-length",
            FitErrorKind::MissingReference => "missing-reference",
            FitErrorKind::RepeatLimit => "repeat-limit",
            FitErrorKind::NodeLimit => "node-limit",
            FitErrorKind::Parity => "parity",
        }
    }
}

/// A fit error: a frame-dependent failure of a valid field map.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FitError {
    /// Field path.
    pub path: String,
    /// Kind.
    pub kind: FitErrorKind,
    /// Bits the field needed from its start (out-of-bounds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub need_bits: Option<u64>,
    /// Bits available from its start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub have_bits: Option<u64>,
}

/// A parsed frame: nodes in pre-order plus the byte → field index for linked selection.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LayerTree {
    /// Nodes, pre-order; a node's children follow it.
    pub nodes: Vec<LayerNode>,
    /// For each frame byte, the ids of the **leaf** nodes overlapping it, in bit order. Click a
    /// byte → select `byte_index[b][0]` (repeat clicks cycle); click a field → highlight its
    /// `bytes` (and `bits` for sub-byte precision).
    pub byte_index: Vec<Vec<u32>>,
    /// Frame fit.
    pub fit: FitStatus,
    /// Fit errors, in evaluation order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<FitError>,
}

/// The `[first, end)` bytes covering `bit_len` bits at `bit_offset` (MSB-first packing). A
/// zero-length range is empty at its start byte.
pub const fn byte_span(bit_offset: u32, bit_len: u32) -> [u32; 2] {
    let first = bit_offset / 8;
    let end = (bit_offset as u64 + bit_len as u64).div_ceil(8) as u32;
    [first, if end < first { first } else { end }]
}

impl LayerTree {
    /// Fills [`Self::byte_index`] from the nodes' ranges for a frame of `byte_len` bytes. Leaves
    /// are nodes without children, excluding layers (a layer whose fields are all absent is not
    /// a selectable field).
    pub fn index_bytes(&mut self, byte_len: usize) {
        let mut is_parent = vec![false; self.nodes.len()];
        for n in &self.nodes {
            if let Some(p) = n.parent
                && let Some(slot) = is_parent.get_mut(p as usize)
            {
                *slot = true;
            }
        }
        let mut leaves: Vec<&LayerNode> = self
            .nodes
            .iter()
            .filter(|n| {
                !is_parent.get(n.id as usize).copied().unwrap_or(true)
                    && n.ty != NodeType::Layer
                    && n.bits[1] > 0
            })
            .collect();
        leaves.sort_by_key(|n| (n.bits[0], n.id));
        self.byte_index = vec![Vec::new(); byte_len];
        for n in leaves {
            let [first, end] = n.bytes;
            for b in first as usize..(end as usize).min(byte_len) {
                self.byte_index[b].push(n.id);
            }
        }
    }

    /// Leaf fields overlapping byte `b`.
    pub fn fields_at_byte(&self, b: usize) -> &[u32] {
        self.byte_index.get(b).map_or(&[], Vec::as_slice)
    }

    /// The node at `path`.
    pub fn node(&self, path: &str) -> Option<&LayerNode> {
        self.nodes.iter().find(|n| n.path == path)
    }
}

/// Lower-case hex of `bytes`.
pub fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Bytes of a hex string (either case), or `None`.
pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Kind of a metadata-only inspector record (§14.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InspectorRecordType {
    /// Pipeline status tick.
    Status,
    /// Hot-edit boundary.
    Edit,
}

impl InspectorRecordType {
    /// Record `type`.
    pub const fn as_str(self) -> &'static str {
        match self {
            InspectorRecordType::Status => STATUS_RECORD_TYPE,
            InspectorRecordType::Edit => EDIT_RECORD_TYPE,
        }
    }
}

/// Field-map fit over many frames: the "does my guess hold across the recording" signal of the
/// parser-authoring loop (ADR-0011 §3.3).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FitSummary {
    /// Frames seen.
    pub frames: u64,
    /// Frames whose every present field fit.
    pub ok: u64,
    /// Frames with some fit errors.
    pub partial: u64,
    /// Frames where no field decoded.
    pub failed: u64,
    /// Frames not parsed: stored gated (metadata only), or without bytes.
    pub unparsed: u64,
    /// Fit errors counted by field path (repeat indexes removed: `items[].id`) and kind.
    #[serde(default)]
    pub errors: BTreeMap<String, BTreeMap<FitErrorKind, u64>>,
}

impl FitSummary {
    /// Counts one frame: its layer tree, or `None` when it could not be parsed.
    pub fn add(&mut self, tree: Option<&LayerTree>) {
        self.frames += 1;
        let Some(tree) = tree else {
            self.unparsed += 1;
            return;
        };
        match tree.fit {
            FitStatus::Ok => self.ok += 1,
            FitStatus::Partial => self.partial += 1,
            FitStatus::Failed => self.failed += 1,
            FitStatus::None => self.unparsed += 1,
        }
        for e in &tree.errors {
            let path = strip_indexes(&e.path);
            *self
                .errors
                .entry(path)
                .or_default()
                .entry(e.kind)
                .or_default() += 1;
        }
    }
}

/// `items[3].id` → `items[].id`.
fn strip_indexes(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut skipping = false;
    for c in path.chars() {
        match c {
            '[' => {
                skipping = true;
                out.push('[');
            }
            ']' => {
                skipping = false;
                out.push(']');
            }
            _ if skipping => {}
            _ => out.push(c),
        }
    }
    out
}

/// Opens recorded decoded streams by capture id (§14.7). The always-on capture store (T-092)
/// implements it; the inspector API (`POST /api/captures/{id}/parse`) reads through it.
pub trait CaptureSource: Send + Sync {
    /// Capture `id`'s §3 byte stream from its first byte (header frame first). `Ok(None)`: no
    /// such capture.
    fn open(&self, id: &str) -> std::io::Result<Option<Box<dyn Read + Send>>>;
}

/// Reads the frame records of a recorded decoded stream (§14.7) from any byte source: the
/// header, then `frame` records in order; `status`, `edit`, drop markers and unknown record
/// types are counted and skipped.
pub struct RecordedFrames<R> {
    r: R,
    dec: FrameDecoder,
    header: StreamHeader,
    eof: bool,
    skipped: u64,
}

impl<R: Read> RecordedFrames<R> {
    /// Reads and checks the header: a `messages` stream whose `message_schema`, if present, is
    /// [`INSPECTOR_MESSAGE_SCHEMA`].
    pub fn open(r: R) -> Result<Self, ClientError> {
        let mut this = Self {
            r,
            dec: FrameDecoder::new(HEADER_MAX_LEN),
            header: StreamHeader::new("", StreamKind::Messages, ContentClass::FAIL_CLOSED, ""),
            eof: false,
            skipped: 0,
        };
        let header = {
            let frame = this.next_raw()?.ok_or(ClientError::Truncated)?;
            StreamHeader::from_json_bytes(&frame)?
        };
        if header.kind != StreamKind::Messages {
            return Err(ClientError::Record(
                "a recorded decoded stream is a messages stream".into(),
            ));
        }
        if header
            .message_schema
            .as_deref()
            .is_some_and(|s| s != INSPECTOR_MESSAGE_SCHEMA)
        {
            return Err(ClientError::Record(format!(
                "message_schema is not {INSPECTOR_MESSAGE_SCHEMA}"
            )));
        }
        this.dec.set_max_frame_len(header.max_frame_len);
        this.header = header;
        Ok(this)
    }

    /// The stream header.
    pub fn header(&self) -> &StreamHeader {
        &self.header
    }

    /// Records skipped so far (not frame records).
    pub fn skipped(&self) -> u64 {
        self.skipped
    }

    fn next_raw(&mut self) -> Result<Option<Vec<u8>>, ClientError> {
        loop {
            if let Some(f) = self.dec.next_frame()? {
                return Ok(Some(f.to_vec()));
            }
            if self.eof {
                return if self.dec.buffered() > 0 {
                    Err(ClientError::Truncated)
                } else {
                    Ok(None)
                };
            }
            if self.dec.read_from(&mut self.r)? == 0 {
                self.eof = true;
            }
        }
    }

    /// The next frame record; `Ok(None)` at a clean end of stream.
    pub fn next_frame(&mut self) -> Result<Option<FrameRecord>, ClientError> {
        loop {
            let Some(raw) = self.next_raw()? else {
                return Ok(None);
            };
            let value: Value =
                serde_json::from_slice(&raw).map_err(|e| ClientError::Record(e.to_string()))?;
            if value.get("type").and_then(Value::as_str) == Some(FRAME_RECORD_TYPE) {
                return FrameRecord::deserialize(value)
                    .map(Some)
                    .map_err(|e| ClientError::Record(e.to_string()));
            }
            self.skipped += 1;
        }
    }
}

impl<R: Read> Iterator for RecordedFrames<R> {
    type Item = Result<FrameRecord, ClientError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_frame().transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(id: u32, parent: Option<u32>, path: &str, ty: NodeType, bits: [u32; 2]) -> LayerNode {
        LayerNode {
            id,
            parent,
            name: path.rsplit('.').next().unwrap().into(),
            path: path.into(),
            ty,
            bits,
            bytes: byte_span(bits[0], bits[1]),
            value: None,
            text: None,
            label: None,
            error: false,
        }
    }

    #[test]
    fn byte_spans() {
        assert_eq!(byte_span(0, 16), [0, 2]);
        assert_eq!(byte_span(4, 8), [0, 2]);
        assert_eq!(byte_span(12, 4), [1, 2]);
        assert_eq!(byte_span(16, 0), [2, 2]);
    }

    #[test]
    fn byte_index_links_bytes_to_leaf_fields() {
        let mut tree = LayerTree {
            nodes: vec![
                node(0, None, "hdr", NodeType::Layer, [0, 16]),
                node(1, Some(0), "hdr.kind", NodeType::Uint, [0, 4]),
                node(2, Some(0), "hdr.flags", NodeType::Bitfield, [4, 8]),
                node(3, Some(2), "hdr.flags.ack", NodeType::Flag, [4, 1]),
                node(4, Some(0), "hdr.len", NodeType::Uint, [12, 4]),
                node(5, None, "body", NodeType::Bytes, [16, 8]),
            ],
            ..Default::default()
        };
        tree.index_bytes(3);
        assert_eq!(tree.fields_at_byte(0), &[1, 3]);
        assert_eq!(tree.fields_at_byte(1), &[4]);
        assert_eq!(tree.fields_at_byte(2), &[5]);
        assert!(tree.fields_at_byte(9).is_empty());
        assert_eq!(tree.node("hdr.len").unwrap().bytes, [1, 2]);
    }

    #[test]
    fn frame_record_wire_shape_round_trips() {
        let wire = json!({
            "type": "frame", "seq": 41, "t": 1_789_300_800_123_456_789_i64,
            "content_class": "unrestricted", "gated": false, "crc_status": "valid",
            "decoder": "recipe:rds@1", "frame_model": "rds",
            "metadata": {"frame": 41, "sample_index": 123_456_789, "channel": 0,
                         "channel_hz": 101_300_000.0, "bit_len": 64, "recipe_version": 1,
                         "edit_rev": 0, "fec_corrected_bits": 0, "fit": "ok"},
            "content": {"hex": "54a80000", "layers": {
                "nodes": [{"id": 0, "name": "pi", "path": "pi", "type": "uint",
                           "bits": [0, 16], "bytes": [0, 2], "value": 21_672, "text": "0x54A8"}],
                "byte_index": [[0], [0], [], []], "fit": "ok"}}
        });
        let rec: FrameRecord = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(rec.metadata.bit_len, Some(64));
        assert_eq!(serde_json::to_value(&rec).unwrap(), wire);

        // Gated: metadata reduced to an allowlist, no content. Every metadata key is optional.
        let gated: FrameRecord = serde_json::from_value(json!({
            "type": "frame", "seq": 42, "t": 0, "content_class": "restricted-paging",
            "gated": true, "metadata": {"frame": 42}
        }))
        .unwrap();
        assert!(gated.content.is_none() && gated.metadata.bit_len.is_none());
    }

    #[test]
    fn hex_round_trip() {
        assert_eq!(to_hex(&[0x54, 0xa8, 0x00]), "54a800");
        assert_eq!(from_hex("54A800").unwrap(), vec![0x54, 0xa8, 0x00]);
        assert!(from_hex("5").is_none() && from_hex("zz").is_none());
    }
}
