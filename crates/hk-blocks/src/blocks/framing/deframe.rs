//! `deframe`: frames without a sync word. From `bits`: consecutive frames cut from the stream
//! (after skipping `offset_bits` following a discontinuity). From `frames`: each input frame
//! split into consecutive sub-frames from `offset_bits`. Lengths follow the shared frame-length
//! rules (fixed `frame_bits`, or `length_from`/`terminator` with `frame_bits` as the maximum).

use hk_recipe::{Params, PortType};

use super::common::{
    Clock, FRAME_BITS_PER_ITEM, P, bit, drops_history, frames_io, one_input, update_hot,
};
use super::length::{FrameLength, LengthState};
use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::buffer::{ChunkFlags, ChunkMeta, FrameInfo, PortSlice, PortVec};
use crate::registry::BuildCtx;
use crate::status::Status;

/// Builds a `deframe`.
pub(crate) fn build(params: &Params, _ctx: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    let p = P(params);
    let max = p.req_uint("frame_bits")?;
    let length = FrameLength::from_params(params, max)?;
    let offset_bits = p.uint_or("offset_bits", 0)? as usize;
    Ok(Box::new(Deframe {
        params: params.clone(),
        body: Vec::with_capacity(max as usize),
        length,
        offset_bits,
        st: LengthState::default(),
        skip_left: offset_bits,
        first_bit: 0,
        bit_index: 0,
        frame_index: 0,
        ty: PortType::Bits,
        rate_hz: 0.0,
        frames: 0,
        bits_dropped: 0,
        status: Status::default(),
    }))
}

/// The block.
pub struct Deframe {
    params: Params,
    length: FrameLength,
    offset_bits: usize,
    body: Vec<u8>,
    st: LengthState,
    skip_left: usize,
    first_bit: u64,
    bit_index: u64,
    frame_index: u64,
    ty: PortType,
    rate_hz: f64,
    frames: u64,
    bits_dropped: u64,
    status: Status,
}

impl Deframe {
    fn process_bits(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        let PortSlice::Bits(bits) = input.data else {
            return Err(BlockError::PortType {
                port: 0,
                expected: PortType::Bits,
                got: input.data.port_type(),
            });
        };
        let out = io.output(0)?;
        out.meta = ChunkMeta {
            index: out.meta.index,
            source_per_item: input.meta.source_per_item * f64::from(self.length.max_bits()),
            rate_hz: self.rate_hz,
            ..input.meta
        };
        let got = out.data.port_type();
        let PortVec::Frames(buf) = &mut out.data else {
            return Err(BlockError::PortType {
                port: 0,
                expected: PortType::Frames,
                got,
            });
        };
        if drops_history(input.meta.flags) {
            self.reset();
        }
        let clock = Clock {
            first_bit: self.bit_index,
            source_index: input.meta.source_index,
            per_bit: input.meta.source_per_item,
        };
        let channel = input.meta.channel;
        let before = buf.len();
        for (k, &b) in bits.iter().enumerate() {
            if self.skip_left > 0 {
                self.skip_left -= 1;
                continue;
            }
            if self.body.is_empty() {
                self.first_bit = self.bit_index + k as u64;
            }
            self.body.push(b & 1);
            if self.length.after_bit(&mut self.st, &self.body) {
                buf.push_bits(
                    &self.body,
                    FrameInfo::new(self.frame_index, clock.source(self.first_bit), channel),
                );
                self.frame_index += 1;
                self.body.clear();
                self.st = LengthState::default();
            }
        }
        if input.meta.flags.contains(ChunkFlags::END) && !self.body.is_empty() {
            buf.push_bits(
                &self.body,
                FrameInfo::new(self.frame_index, clock.source(self.first_bit), channel),
            );
            self.frame_index += 1;
            self.body.clear();
            self.st = LengthState::default();
        }
        self.bit_index += bits.len() as u64;
        self.status.items_in += bits.len() as u64;
        let n = (buf.len() - before) as u64;
        self.frames += n;
        self.status.items_out += n;
        Ok(())
    }

    fn process_frames(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let (_, frames, buf) = frames_io(io)?;
        let before = buf.len();
        for f in frames.iter() {
            let len = f.info.bit_len as usize;
            let mut pos = self.offset_bits.min(len);
            let mut first = true;
            while pos < len {
                self.body.clear();
                self.st = LengthState::default();
                let mut ended = false;
                while pos < len {
                    self.body.push(bit(f.bytes, pos));
                    pos += 1;
                    if self.length.after_bit(&mut self.st, &self.body) {
                        ended = true;
                        break;
                    }
                }
                if !ended {
                    self.bits_dropped += self.body.len() as u64;
                    break;
                }
                let mut info =
                    FrameInfo::new(self.frame_index, f.info.source_index, f.info.channel);
                info.check = f.info.check;
                info.corrected_bits = if first { f.info.corrected_bits } else { 0 };
                first = false;
                buf.push_bits(&self.body, info);
                self.frame_index += 1;
            }
            self.body.clear();
        }
        self.status.items_in += frames.len() as u64;
        let n = (buf.len() - before) as u64;
        self.frames += n;
        self.status.items_out += n;
        Ok(())
    }
}

impl Block for Deframe {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = one_input("deframe", inputs, &[PortType::Bits, PortType::Frames])?;
        self.ty = input.ty;
        let min = self.length.min_bits() as usize;
        Ok(vec![match input.ty {
            PortType::Bits => {
                self.rate_hz = input.rate_hz / f64::from(self.length.max_bits());
                PortInfo {
                    ty: PortType::Frames,
                    rate_hz: self.rate_hz,
                    max_items: input.max_items / min + 2,
                    hold_items: self.length.max_bits() as usize,
                }
            }
            _ => PortInfo {
                ty: PortType::Frames,
                rate_hz: input.rate_hz,
                // Each sub-frame takes at least min_bits of the chunk's bits.
                max_items: input
                    .max_items
                    .saturating_mul(FRAME_BITS_PER_ITEM / min)
                    .max(1),
                hold_items: 0,
            },
        }])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let r = match self.ty {
            PortType::Bits => self.process_bits(io),
            _ => self.process_frames(io),
        };
        self.status.extra.set("frames", self.frames as f64);
        self.status
            .extra
            .set("bits_dropped", self.bits_dropped as f64);
        r
    }

    fn reset(&mut self) {
        self.body.clear();
        self.st = LengthState::default();
        self.skip_left = self.offset_bits;
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
