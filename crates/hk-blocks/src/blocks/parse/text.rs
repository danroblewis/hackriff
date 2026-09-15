//! `text`: assembles segmented text carried across frames (RDS PS and RadioText) from the layer
//! trees a `fields` block attached, and emits one frame per assembled string (ADR-0011 §1.5).
//!
//! Per input frame (frames without layers, or whose check is `invalid`, are skipped):
//! 1. `key` (an integer field, e.g. PI): a change restarts assembly.
//! 2. `reset_on` (e.g. the RadioText A/B flag): a change clears the string.
//! 3. `address`: the first present path gives the segment index; `chars`: the first present
//!    path gives the segment's characters, read as 8-bit codes from the frame bytes over that
//!    node's bit range (`chars_per_segment` codes, default the node's length / 8). A change of
//!    characters per segment (RadioText 2A ↔ 2B) restarts assembly.
//! 4. `terminator`: the string ends before the first terminator character.
//!
//! **Emit.** `on-complete`: when every segment up to the end of the string has arrived since
//! the last emission (so a repeated PS cycle emits once per cycle); a segment whose characters
//! changed clears the other segments' freshness, so a string never mixes two versions of the
//! text. `on-change`: whenever a segment's characters change, missing segments as spaces. The
//! emitted frame's check is the source frame's, except that a string holding a segment taken
//! from a `corrected` frame is itself `corrected` (T-210). The output frame's bytes are the
//! character codes; its layer tree is `<name>` (layer) with `<name>.key` (the key value, a
//! zero-length node) and `<name>.text` (the string in `charset`).

use std::sync::Arc;

use hk_model::CrcStatus;
use hk_recipe::fields::eval::decode_chars;
use hk_recipe::{BlockDescriptor, Charset, Params, PortType, parse_hex};
use hk_stream::inspector::{FitStatus, LayerNode, LayerTree, NodeType, byte_span};
use serde_json::Value;

use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::buffer::{ChunkFlags, ChunkMeta, Frame, FrameBuf, FrameInfo, PortSlice, PortVec};
use crate::registry::{BlockFactory, BuildCtx};
use crate::status::Status;

/// Builds [`Text`].
pub struct TextFactory {
    descriptor: BlockDescriptor,
}

impl TextFactory {
    /// The factory (descriptor pinned in [`super::planned`]).
    pub fn new() -> Self {
        Self {
            descriptor: super::pinned("text"),
        }
    }
}

impl Default for TextFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockFactory for TextFactory {
    fn descriptor(&self) -> &BlockDescriptor {
        &self.descriptor
    }

    fn build(&self, params: &Params, _ctx: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
        Ok(Box::new(Text::new(Config::parse(params)?, params.clone())))
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Config {
    name: String,
    key: Option<String>,
    address: Vec<String>,
    chars: Vec<String>,
    segments: usize,
    chars_per_segment: Option<usize>,
    reset_on: Option<String>,
    terminator: Option<u8>,
    on_change: bool,
    charset: Charset,
}

fn bad(msg: &str) -> BlockError {
    BlockError::Params(msg.to_owned())
}

impl Config {
    fn parse(p: &Params) -> Result<Self, BlockError> {
        let string = |k: &str| p.get(k).and_then(Value::as_str).map(str::to_owned);
        let paths = |k: &str| -> Result<Vec<String>, BlockError> {
            let v: Option<Vec<String>> = p.get(k).and_then(Value::as_array).and_then(|a| {
                a.iter()
                    .map(|x| x.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
            });
            v.filter(|v| !v.is_empty())
                .ok_or_else(|| bad(&format!("{k} is a non-empty list of field paths")))
        };
        let int = |k: &str| p.get(k).and_then(Value::as_u64).map(|x| x as usize);
        let name = string("name").ok_or_else(|| bad("name is required"))?;
        let segments = int("segments")
            .filter(|n| (1..=256).contains(n))
            .ok_or_else(|| bad("segments is 1..=256"))?;
        let chars_per_segment = match p.get("chars_per_segment") {
            None => None,
            Some(_) => Some(
                int("chars_per_segment")
                    .filter(|n| (1..=64).contains(n))
                    .ok_or_else(|| bad("chars_per_segment is 1..=64"))?,
            ),
        };
        let terminator = match p.get("terminator") {
            None => None,
            Some(v) => Some(
                v.as_str()
                    .and_then(parse_hex)
                    .and_then(|x| u8::try_from(x).ok())
                    .ok_or_else(|| bad("terminator is an 8-bit hex value"))?,
            ),
        };
        let on_change = match string("emit").as_deref() {
            None | Some("on-complete") => false,
            Some("on-change") => true,
            Some(_) => return Err(bad("emit is on-complete or on-change")),
        };
        let charset = match string("charset").as_deref() {
            None | Some("ascii") => Charset::Ascii,
            Some("latin1") => Charset::Latin1,
            Some("rds") => Charset::Rds,
            Some(_) => return Err(bad("charset is ascii, latin1 or rds")),
        };
        Ok(Self {
            name,
            key: string("key"),
            address: paths("address")?,
            chars: paths("chars")?,
            segments,
            chars_per_segment,
            reset_on: string("reset_on"),
            terminator,
            on_change,
            charset,
        })
    }
}

/// The block.
pub struct Text {
    cfg: Config,
    params: Params,
    key: Option<Value>,
    reset_value: Option<Value>,
    /// Characters per segment of the string being assembled (0: none yet).
    cps: usize,
    chars: Vec<u8>,
    /// Segment arrived since the last emission.
    fresh: Vec<bool>,
    /// Segment ever arrived since the last restart (for `on-change`).
    seen: Vec<bool>,
    /// Segment's current characters came from a frame that was not `corrected` (T-210: a string
    /// holding any corrected segment is emitted `corrected`, never `valid`).
    clean: Vec<bool>,
    emitted: u64,
    codes: Vec<u8>,
    status: Status,
}

impl Text {
    fn new(cfg: Config, params: Params) -> Self {
        let segments = cfg.segments;
        Self {
            cfg,
            params,
            key: None,
            reset_value: None,
            cps: 0,
            chars: Vec::new(),
            fresh: vec![false; segments],
            seen: vec![false; segments],
            clean: vec![true; segments],
            emitted: 0,
            codes: Vec::with_capacity(64),
            status: Status::default(),
        }
    }

    fn restart(&mut self) {
        self.chars.fill(b' ');
        self.fresh.fill(false);
        self.seen.fill(false);
        self.clean.fill(true);
    }

    /// Characters in the assembled string (up to the terminator, if one has arrived).
    fn string_len(&self) -> usize {
        let full = self.cfg.segments * self.cps;
        match self.cfg.terminator {
            Some(t) => (0..full)
                .find(|&i| self.seen[i / self.cps] && self.chars[i] == t)
                .unwrap_or(full),
            None => full,
        }
    }

    fn take(&mut self, f: Frame<'_>, out: &mut FrameBuf) {
        if f.info.check == CrcStatus::Invalid {
            return;
        }
        let Some(tree) = f.info.layers.as_deref() else {
            return;
        };
        let value = |path: &Option<String>| {
            path.as_deref()
                .and_then(|p| tree.node(p))
                .filter(|n| !n.error)
                .and_then(|n| n.value.clone())
        };
        if let Some(k) = value(&self.cfg.key) {
            if self.key.as_ref() != Some(&k) {
                self.restart();
                self.key = Some(k);
            }
        }
        if let Some(r) = value(&self.cfg.reset_on) {
            if self.reset_value.as_ref().is_some_and(|old| *old != r) {
                self.restart();
            }
            self.reset_value = Some(r);
        }
        let first = |paths: &[String]| paths.iter().find_map(|p| tree.node(p).filter(|n| !n.error));
        let Some(segment) = first(&self.cfg.address)
            .and_then(|n| n.value.as_ref())
            .and_then(Value::as_u64)
            .map(|s| s as usize)
        else {
            return;
        };
        let Some(chars) = first(&self.cfg.chars) else {
            return;
        };
        let cps = self
            .cfg
            .chars_per_segment
            .unwrap_or(chars.bits[1] as usize / 8);
        if cps == 0 || segment >= self.cfg.segments {
            return;
        }
        let start = chars.bits[0] as usize;
        if start + cps * 8 > f.info.bit_len as usize {
            return;
        }
        if cps != self.cps {
            self.cps = cps;
            self.chars.clear();
            self.chars.resize(self.cfg.segments * cps, b' ');
            self.restart();
        }
        self.codes.clear();
        for i in 0..cps {
            let bit = start + i * 8;
            let (byte, shift) = (bit / 8, bit % 8);
            let hi = u16::from(f.bytes[byte]) << 8;
            let lo = u16::from(f.bytes.get(byte + 1).copied().unwrap_or(0));
            self.codes.push(((hi | lo) << shift >> 8) as u8);
        }
        let slot = &mut self.chars[segment * cps..(segment + 1) * cps];
        let changed = !self.seen[segment] || slot != self.codes.as_slice();
        slot.copy_from_slice(&self.codes);
        // A segment that changed invalidates the other segments' freshness: `on-complete` then
        // waits for them to arrive again, so a string never mixes two versions of the text
        // (a scrolling RDS PS changing mid-cycle).
        if changed && self.seen[segment] {
            self.fresh.fill(false);
        }
        self.fresh[segment] = true;
        self.seen[segment] = true;
        self.clean[segment] = f.info.check != CrcStatus::Corrected;
        let len = self.string_len();
        let last_segment = if len == 0 { 0 } else { (len - 1) / cps };
        if self.cfg.on_change {
            if changed {
                self.emit(f.info, len, out);
            }
        } else if self.fresh[..=last_segment.min(self.cfg.segments - 1)]
            .iter()
            .all(|&x| x)
        {
            self.emit(f.info, len, out);
            self.fresh.fill(false);
        }
    }

    fn emit(&mut self, src: &FrameInfo, len: usize, out: &mut FrameBuf) {
        // The string's provenance: `corrected` as soon as one of its segments came from a
        // corrected frame, so it is never counted as CRC-valid evidence downstream.
        let last = if len == 0 {
            0
        } else {
            (len - 1) / self.cps.max(1)
        };
        let check = if (0..=last.min(self.cfg.segments - 1)).any(|s| self.seen[s] && !self.clean[s])
        {
            CrcStatus::Corrected
        } else {
            src.check
        };
        let bytes = &self.chars[..len];
        let bit_len = (len * 8) as u32;
        let text = decode_chars(self.cfg.charset, bytes);
        let mut nodes = vec![LayerNode {
            id: 0,
            parent: None,
            name: self.cfg.name.clone(),
            path: self.cfg.name.clone(),
            ty: NodeType::Layer,
            bits: [0, bit_len],
            bytes: byte_span(0, bit_len),
            value: None,
            text: None,
            label: None,
            error: false,
        }];
        if let Some(k) = &self.key {
            nodes.push(LayerNode {
                id: 1,
                parent: Some(0),
                name: "key".into(),
                path: format!("{}.key", self.cfg.name),
                ty: NodeType::Uint,
                bits: [0, 0],
                bytes: [0, 0],
                value: Some(k.clone()),
                text: Some(
                    k.as_u64()
                        .map_or_else(|| k.to_string(), |v| format!("0x{v:X}")),
                ),
                label: None,
                error: false,
            });
        }
        nodes.push(LayerNode {
            id: nodes.len() as u32,
            parent: Some(0),
            name: "text".into(),
            path: format!("{}.text", self.cfg.name),
            ty: NodeType::Ascii,
            bits: [0, bit_len],
            bytes: byte_span(0, bit_len),
            value: Some(Value::String(text.clone())),
            text: Some(text),
            label: None,
            error: false,
        });
        let mut tree = LayerTree {
            nodes,
            byte_index: Vec::new(),
            fit: FitStatus::Ok,
            errors: Vec::new(),
        };
        tree.index_bytes(len);
        let info = FrameInfo {
            index: self.emitted,
            source_index: src.source_index,
            channel: src.channel,
            bit_len,
            check,
            corrected_bits: 0,
            layers: Some(Arc::new(tree)),
        };
        out.push(bytes, info);
        self.emitted += 1;
    }
}

impl Block for Text {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let [input] = inputs else {
            return Err(BlockError::Ports("text has exactly one input".into()));
        };
        if input.ty != PortType::Frames {
            return Err(BlockError::PortType {
                port: 0,
                expected: PortType::Frames,
                got: input.ty,
            });
        }
        Ok(vec![PortInfo {
            hold_items: self.cfg.segments,
            ..*input
        }])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        let PortSlice::Frames(frames) = input.data else {
            return Err(BlockError::PortType {
                port: 0,
                expected: PortType::Frames,
                got: input.data.port_type(),
            });
        };
        if input.meta.flags.contains(ChunkFlags::DISCONTINUITY)
            || input.meta.flags.contains(ChunkFlags::RESET)
            || input.meta.flags.contains(ChunkFlags::CHANNEL_CHANGE)
        {
            self.reset();
        }
        let out = io.output(0)?;
        out.meta = ChunkMeta {
            index: out.meta.index,
            ..input.meta
        };
        let PortVec::Frames(buf) = &mut out.data else {
            return Err(BlockError::PortType {
                port: 0,
                expected: PortType::Frames,
                got: out.data.port_type(),
            });
        };
        let before = buf.len();
        for f in frames.iter() {
            self.take(f, buf);
        }
        self.status.items_in += frames.len() as u64;
        self.status.items_out += (buf.len() - before) as u64;
        if self.cps > 0 {
            let seen = self.seen.iter().filter(|&&x| x).count();
            self.status.quality = Some(seen as f32 / self.cfg.segments as f32);
        }
        self.status.extra.set("strings", self.emitted as f64);
        Ok(())
    }

    fn reset(&mut self) {
        self.key = None;
        self.reset_value = None;
        self.restart();
    }

    fn update_params(
        &mut self,
        params: &Params,
        _ctx: &BuildCtx<'_>,
    ) -> Result<ParamUpdate, BlockError> {
        let cfg = Config::parse(params)?;
        let mut cold_old = self.params.clone();
        let mut cold_new = params.clone();
        cold_old.remove("emit");
        cold_new.remove("emit");
        if cold_old != cold_new {
            return Ok(ParamUpdate::Rebuild);
        }
        self.cfg.on_change = cfg.on_change;
        self.params = params.clone();
        Ok(ParamUpdate::Applied)
    }

    fn status(&self) -> Status {
        self.status
    }
}
