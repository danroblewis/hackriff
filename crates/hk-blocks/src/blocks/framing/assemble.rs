//! `assemble`: codewords → messages (POCSAG). Each input frame (a batch) splits into
//! `word_bits` words; a start word opens a message (header bits, then its slot index), later
//! words append payload bits, and an idle word, the next start word, `max_words`, a
//! non-contiguous frame or `DISCONTINUITY`/`END` closes it.
//!
//! **Check status.** A frame carries one check status (ADR-0011 §1.1), so a message's check is
//! the worst of the frames its words came from and `corrected_bits` the sum of those frames'
//! corrected bits (each frame counted once). Per-word status would need a `FrameInfo`
//! extension.

use hk_model::CrcStatus;
use hk_recipe::{Params, PortType};

use super::common::{
    FRAME_BITS_PER_ITEM, P, bit, drops_history, extend_bits, frames_io, frames_port, one_input,
    push_value, read_bits, update_hot,
};
use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::buffer::{ChunkFlags, FrameBuf, FrameInfo};
use crate::registry::BuildCtx;
use crate::status::Status;

#[derive(Clone, Copy)]
struct Range {
    offset: usize,
    bits: usize,
}

/// Builds an `assemble`.
pub(crate) fn build(params: &Params, _ctx: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    let p = P(params);
    let perr = |m: &str| BlockError::Params(m.into());
    let word_bits = p.req_uint("word_bits")? as usize;
    let range = |key: &str| -> Result<Option<Range>, BlockError> {
        let Some(o) = p.obj(key) else { return Ok(None) };
        let r = Range {
            offset: o.req_uint("offset_bits")? as usize,
            bits: o.req_uint("bits")? as usize,
        };
        if r.offset + r.bits > word_bits {
            return Err(BlockError::Params(format!("{key} ends past word_bits")));
        }
        Ok(Some(r))
    };
    let header = range("header")?.ok_or_else(|| perr("header is required"))?;
    let payload = range("payload")?.ok_or_else(|| perr("payload is required"))?;
    let start = p.obj("start").ok_or_else(|| perr("start is required"))?;
    let start_bit = start.req_uint("bit")? as usize;
    if start_bit >= word_bits {
        return Err(perr("start.bit past word_bits"));
    }
    let slot = match p.obj("slot") {
        None => None,
        Some(s) => Some((
            s.req_uint("words_per_slot")?.max(1) as usize,
            s.req_uint("bits")? as usize,
        )),
    };
    let mask = if word_bits >= 64 {
        u64::MAX
    } else {
        (1u64 << word_bits) - 1
    };
    let idle = p
        .list("idle_words")
        .iter()
        .map(|w| {
            w.as_str()
                .and_then(hk_recipe::parse_hex)
                .filter(|v| v & !mask == 0)
                .ok_or_else(|| perr("idle word wider than word_bits"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let max_words = p.uint_or("max_words", 256)?.max(1) as usize;
    let cap = header.bits + slot.map_or(0, |s| s.1) + max_words * payload.bits;
    Ok(Box::new(Assemble {
        params: params.clone(),
        word_bits,
        start_bit,
        start_value: start.req_uint("value")? as u8,
        idle,
        header,
        slot,
        payload,
        max_words,
        span_frames: p.bool_or("span_frames", true),
        msg: Vec::with_capacity(cap),
        open: None,
        prev_source: None,
        min_gap: None,
        status: Status::default(),
        out_index: 0,
        messages: 0,
        words: 0,
        idle_words: 0,
        orphans: 0,
    }))
}

#[derive(Clone, Copy)]
struct Open {
    source: u64,
    channel: u16,
    words: usize,
    check: CrcStatus,
    corrected: u32,
    /// Index of the last input frame whose status was folded in.
    last_frame: u64,
}

/// The block.
pub struct Assemble {
    params: Params,
    word_bits: usize,
    start_bit: usize,
    start_value: u8,
    idle: Vec<u64>,
    header: Range,
    slot: Option<(usize, usize)>,
    payload: Range,
    max_words: usize,
    span_frames: bool,
    msg: Vec<u8>,
    open: Option<Open>,
    prev_source: Option<u64>,
    min_gap: Option<u64>,
    status: Status,
    out_index: u64,
    messages: u64,
    words: u64,
    idle_words: u64,
    orphans: u64,
}

fn worse(a: CrcStatus, b: CrcStatus) -> CrcStatus {
    let rank = |s| match s {
        CrcStatus::Invalid => 3,
        CrcStatus::Unknown => 2,
        CrcStatus::NoCrc => 1,
        CrcStatus::Valid => 0,
    };
    if rank(b) > rank(a) { b } else { a }
}

impl Assemble {
    fn close(&mut self, out: &mut FrameBuf) {
        let Some(open) = self.open.take() else { return };
        let mut info = FrameInfo::new(self.out_index, open.source, open.channel);
        info.check = open.check;
        info.corrected_bits = open.corrected;
        out.push_bits(&self.msg, info);
        self.out_index += 1;
        self.messages += 1;
        self.msg.clear();
    }

    fn fold(open: &mut Open, info: &FrameInfo) {
        if open.last_frame != info.index {
            open.last_frame = info.index;
            open.check = worse(open.check, info.check);
            open.corrected += info.corrected_bits;
        }
    }

    /// Whether `source` continues the previous frame: no larger gap than 1.5 × the smallest
    /// inter-frame gap seen since the last discontinuity.
    fn contiguous(&mut self, source: u64) -> bool {
        let Some(prev) = self.prev_source.replace(source) else {
            return false;
        };
        let gap = source.saturating_sub(prev);
        let min = self.min_gap.map_or(gap, |m| m.min(gap));
        self.min_gap = Some(min);
        gap.saturating_mul(2) <= min.saturating_mul(3)
    }

    fn frame(&mut self, frames: &FrameBuf, i: usize, out: &mut FrameBuf) {
        let Some(f) = frames.get(i) else { return };
        let info = f.info;
        if !self.contiguous(info.source_index) {
            self.close(out);
        }
        let n_words = info.bit_len as usize / self.word_bits;
        for w in 0..n_words {
            let base = w * self.word_bits;
            let word = read_bits(f.bytes, base, self.word_bits);
            self.words += 1;
            if self.idle.contains(&word) {
                self.idle_words += 1;
                self.close(out);
                continue;
            }
            if bit(f.bytes, base + self.start_bit) == self.start_value {
                self.close(out);
                self.msg.clear();
                extend_bits(
                    &mut self.msg,
                    f.bytes,
                    base + self.header.offset,
                    self.header.bits,
                );
                if let Some((per, bits)) = self.slot {
                    push_value(&mut self.msg, (w / per) as u64, bits);
                }
                self.open = Some(Open {
                    source: info.source_index,
                    channel: info.channel,
                    words: 1,
                    check: info.check,
                    corrected: info.corrected_bits,
                    last_frame: info.index,
                });
            } else if let Some(open) = &mut self.open {
                Self::fold(open, info);
                extend_bits(
                    &mut self.msg,
                    f.bytes,
                    base + self.payload.offset,
                    self.payload.bits,
                );
                open.words += 1;
            } else {
                self.orphans += 1;
                continue;
            }
            if self.open.is_some_and(|o| o.words >= self.max_words) {
                self.close(out);
            }
        }
        if !self.span_frames {
            self.close(out);
        }
    }
}

impl Block for Assemble {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = one_input("assemble", inputs, &[PortType::Frames])?;
        // A message needs its own start word (idle-closed, max_words 1), plus the one carried
        // open from the previous chunk: at most the chunk's words + 1.
        let words = input
            .max_items
            .saturating_mul(FRAME_BITS_PER_ITEM / self.word_bits);
        Ok(vec![frames_port(input, words.saturating_add(1))])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let (meta, frames, buf) = frames_io(io)?;
        let before = buf.len();
        if drops_history(meta.flags) {
            // A message never spans a discontinuity.
            self.reset();
        }
        for i in 0..frames.len() {
            self.frame(frames, i, buf);
        }
        if meta.flags.contains(ChunkFlags::END) {
            self.close(buf);
        }
        self.status.items_in += frames.len() as u64;
        self.status.items_out += (buf.len() - before) as u64;
        let s = &mut self.status.extra;
        s.set("messages", self.messages as f64);
        s.set("words", self.words as f64);
        s.set("idle_words", self.idle_words as f64);
        s.set("orphan_words", self.orphans as f64);
        Ok(())
    }

    fn reset(&mut self) {
        self.open = None;
        self.msg.clear();
        self.prev_source = None;
        self.min_gap = None;
    }

    fn update_params(
        &mut self,
        params: &Params,
        _ctx: &BuildCtx<'_>,
    ) -> Result<ParamUpdate, BlockError> {
        update_hot(&mut self.params, params, &[], |_| {})
    }

    fn status(&self) -> Status {
        self.status
    }
}
