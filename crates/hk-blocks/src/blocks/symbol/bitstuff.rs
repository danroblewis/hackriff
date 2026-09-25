//! `bitstuff`: HDLC zero-bit (de)stuffing (T-613, ADR-0011 §9.1; ISO/IEC 13239).
//!
//! One rule: after `stuff_after` (5) consecutive ones the transmitter inserts a zero. `destuff`
//! (receive, default) drops the zero that follows exactly `stuff_after` ones; `stuff` inserts it.
//! A run of `abort_ones` (7) or more ones is abort/idle, never data: no zero after it is dropped
//! (the run is longer than `stuff_after`), and it is counted in the `aborts` extra.
//!
//! Frame boundaries stay `sync_search`'s job. On `bits` the flag (`01111110`, six ones) passes
//! through untouched — the zero after it follows six ones, not five — so `sync_search` still
//! frames on it. On `frames` the rule restarts at each frame, and an abort run ends the frame
//! (truncated before the run; ISO 13239 discards an aborted frame's data).
//!
//! **Flag search before destuffing (AIS/AX.25).** Stuffing guarantees six ones in a row only
//! ever occur in a flag *on the stuffed line*; destuffed data can contain `01111110` (40 % of
//! random AIS position reports do), so a receiver frames first (`sync_search` on the stuffed
//! bits) and destuffs each frame here in `frames` mode. `bit_order: lsb` (frames only) then
//! reverses the destuffed body per 8 bits from its first bit — the octets of a protocol that
//! sends each octet LSB first (HDLC, ISO/IEC 13239 §4.3), in the packed form `crc` and `fields`
//! read (the `sync_search` `bit_order` convention, which cannot be used there: the stuffed line
//! is not octet-aligned). A trailing partial character is reversed within its own length.
//!
//! The only state is the current run of ones, so a stuffed zero that falls on a chunk boundary
//! is handled identically to one inside a chunk.

use hk_recipe::{BlockDescriptor, Params, PortSpec, PortType};

use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::blocks::framing::common::{P, drops_history, frames_io, frames_port, one_input};
use crate::blocks::iq::common::{bits_in, bits_out, restarts, set_meta};
use crate::buffer::PortSlice;
use crate::registry::BuildCtx;
use crate::schema::{ParamExt, descriptor as describe, hex, int, one_of, param};
use crate::status::Status;

/// The pinned `bitstuff` row.
pub(crate) fn descriptor(inputs: Vec<PortSpec>, outputs: Vec<PortSpec>) -> BlockDescriptor {
    describe(
        "bitstuff",
        "symbol",
        "HDLC zero-bit (de)stuffing; flags pass through in bits mode so sync_search still \
         frames on them.",
        inputs,
        outputs,
        vec![
            param(
                "flag",
                hex(8),
                "Flag octet; documents the framing this pairs with — flags are found by \
                 sync_search, not here.",
            )
            .default_value("0x7E"),
            param("stuff_after", int(1, 16), "Ones before a stuffed zero.").default_value(5),
            param(
                "direction",
                one_of(&["destuff", "stuff"]),
                "Remove stuffed zeros (receive) or insert them.",
            )
            .default_value("destuff"),
            param("abort_ones", int(2, 32), "Ones that mean abort/idle.").default_value(7),
            param(
                "bit_order",
                one_of(&["msb", "lsb"]),
                "frames only: lsb = the protocol sends octets LSB first (HDLC/AIS); the \
                 destuffed body is bit-reversed per 8 bits from its first bit.",
            )
            .default_value("msb"),
        ],
        true,
    )
}

/// Builds a `bitstuff`.
pub(crate) fn build(params: &Params, _ctx: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    let p = P(params);
    let stuff = match p.str("direction").unwrap_or("destuff") {
        "destuff" => false,
        "stuff" => true,
        _ => {
            return Err(BlockError::Params(
                "direction must be destuff or stuff".into(),
            ));
        }
    };
    let after = p.uint_or("stuff_after", 5)?;
    let abort = p.uint_or("abort_ones", 7)?;
    let lsb = match p.str("bit_order").unwrap_or("msb") {
        "msb" => false,
        "lsb" => true,
        _ => return Err(BlockError::Params("bit_order must be msb or lsb".into())),
    };
    if abort <= after {
        return Err(BlockError::Params(
            "abort_ones must exceed stuff_after".into(),
        ));
    }
    Ok(Box::new(Bitstuff {
        stuff,
        after,
        abort,
        lsb,
        ones: 0,
        aborts: 0,
        scratch: Vec::new(),
        status: Status::default(),
    }))
}

/// The block.
pub struct Bitstuff {
    stuff: bool,
    after: u32,
    abort: u32,
    /// `bit_order: lsb` (frames mode): reverse the output per 8 bits.
    lsb: bool,
    /// Consecutive ones just seen.
    ones: u32,
    aborts: u64,
    scratch: Vec<u8>,
    status: Status,
}

impl Bitstuff {
    /// One bit through the rule; pushes 0, 1 or 2 bits. Returns `true` on the bit that
    /// completes an abort run (destuff only).
    fn bit(&mut self, b: u8, out: &mut Vec<u8>) -> bool {
        if b == 1 {
            self.ones = self.ones.saturating_add(1);
            out.push(1);
            if self.stuff && self.ones == self.after {
                out.push(0);
                self.ones = 0;
            }
            !self.stuff && self.ones == self.abort
        } else {
            if self.stuff || self.ones != self.after {
                out.push(0);
            }
            self.ones = 0;
            false
        }
    }

    fn process_bits(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        let x = bits_in(&input)?;
        let m = input.meta;
        if restarts(m.flags) {
            self.ones = 0;
        }
        let out = io.output(0)?;
        set_meta(out, &m, m.source_index, m.source_per_item);
        let y = bits_out(out)?;
        let before = y.len();
        for &b in x {
            if self.bit(b & 1, y) {
                self.aborts += 1;
            }
        }
        self.status.items_in += x.len() as u64;
        self.status.items_out += (y.len() - before) as u64;
        self.status.extra.set("aborts", self.aborts as f64);
        Ok(())
    }

    fn process_frames(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let (meta, frames, buf) = frames_io(io)?;
        if drops_history(meta.flags) {
            self.ones = 0;
        }
        let before = buf.len();
        let mut bits = std::mem::take(&mut self.scratch);
        for f in frames.iter() {
            self.ones = 0;
            bits.clear();
            let mut aborted = false;
            for i in 0..f.info.bit_len as usize {
                let b = (f.bytes[i / 8] >> (7 - i % 8)) & 1;
                if self.bit(b, &mut bits) {
                    aborted = true;
                    break;
                }
            }
            if aborted {
                self.aborts += 1;
                bits.truncate(bits.len() - self.abort as usize);
            }
            if self.lsb {
                for chr in bits.chunks_mut(8) {
                    chr.reverse();
                }
            }
            buf.push_bits(&bits, f.info.clone());
        }
        self.scratch = bits;
        self.status.items_in += frames.len() as u64;
        self.status.items_out += (buf.len() - before) as u64;
        self.status.extra.set("aborts", self.aborts as f64);
        Ok(())
    }
}

impl Block for Bitstuff {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = one_input("bitstuff", inputs, &[PortType::Bits, PortType::Frames])?;
        if self.lsb && input.ty != PortType::Frames {
            return Err(BlockError::Params(
                "bit_order: lsb needs a frames input (a bits stream has no octet boundary)".into(),
            ));
        }
        Ok(vec![match input.ty {
            PortType::Frames => frames_port(input, input.max_items),
            // Stuffing can add one bit per `after` ones.
            _ => PortInfo {
                hold_items: 0,
                max_items: input.max_items + input.max_items / self.after.max(1) as usize + 1,
                ..input
            },
        }])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        match io.input(0)?.data {
            PortSlice::Frames(_) => self.process_frames(io),
            _ => self.process_bits(io),
        }
    }

    fn reset(&mut self) {
        self.ones = 0;
    }

    fn update_params(
        &mut self,
        _params: &Params,
        _: &BuildCtx<'_>,
    ) -> Result<ParamUpdate, BlockError> {
        Ok(ParamUpdate::Rebuild)
    }

    fn status(&self) -> Status {
        self.status
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::blocks::framing::common::testutil::{Owned, build as mk, run_frames};
    use crate::buffer::{ChunkFlags, ChunkMeta, Input, Output, PortVec};

    fn run(params: Value, bits: &[u8], chunk: usize) -> Vec<u8> {
        let mut b = mk("bitstuff", params, PortType::Bits);
        let info = b
            .init(&[PortInfo {
                ty: PortType::Bits,
                rate_hz: 9600.0,
                max_items: chunk,
                hold_items: 0,
            }])
            .unwrap();
        let mut outputs = vec![Output::for_port(&info[0])];
        let mut all = Vec::new();
        for (k, c) in bits.chunks(chunk).enumerate() {
            outputs[0].begin_chunk();
            let meta = ChunkMeta {
                index: (k * chunk) as u64,
                flags: if k == 0 {
                    ChunkFlags::DISCONTINUITY
                } else {
                    ChunkFlags::NONE
                },
                ..ChunkMeta::start(9600.0)
            };
            let inputs = [Input {
                meta,
                data: PortSlice::Bits(c),
            }];
            b.process(&mut Io::new(&inputs, &mut outputs)).unwrap();
            let PortVec::Bits(v) = &outputs[0].data else {
                panic!()
            };
            all.extend_from_slice(v);
        }
        all
    }

    fn bits(s: &str) -> Vec<u8> {
        s.bytes().filter(|c| *c != b' ').map(|c| c - b'0').collect()
    }

    fn stuffed(data: &[u8]) -> Vec<u8> {
        run(json!({"direction": "stuff"}), data, data.len().max(1))
    }

    #[test]
    fn destuff_drops_zero_after_five_ones_only() {
        assert_eq!(run(json!({}), &bits("1111101"), 64), bits("111111"));
        // A flag keeps its zeros (six ones); so does an abort run.
        assert_eq!(run(json!({}), &bits("01111110"), 64), bits("01111110"));
        assert_eq!(run(json!({}), &bits("11111110"), 64), bits("11111110"));
    }

    #[test]
    fn round_trip_and_chunking_invariance_including_boundary_zero() {
        let mut data: Vec<u8> = (0..400u32)
            .map(|k| u8::from((k.wrapping_mul(2654435761) >> 13) & 3 != 0))
            .collect();
        data.extend(bits("11111 11111 11111 0 11111"));
        let s = stuffed(&data);
        assert!(s.len() > data.len());
        for chunk in [1, 2, 3, 5, 6, 7, 64, s.len()] {
            assert_eq!(run(json!({}), &s, chunk), data, "chunk {chunk}");
        }
        // The stuffed zero exactly on a chunk boundary (5 ones | stuffed 0 | 1).
        let s = stuffed(&bits("111111"));
        assert_eq!(s, bits("1111101"));
        assert_eq!(run(json!({}), &s, 5), bits("111111"));
        assert_eq!(run(json!({}), &s, 6), bits("111111"));
    }

    /// AIS-style frame between flags: payload `00 11111 11111 1 0` is stuffed on air.
    #[test]
    fn known_ais_style_frame_destuffs_to_payload() {
        let air = bits("01111110 00 11111 0 11111 0 1 0 01111110");
        let out = run(json!({}), &air, 7);
        assert_eq!(out, bits("01111110 00 11111 11111 1 0 01111110"));
    }

    #[test]
    fn frames_mode_destuffs_per_frame_and_truncates_abort() {
        let mut b = mk("bitstuff", json!({}), PortType::Frames);
        let f = |s: &str| Owned::from_bits(&bits(s), 0, 0);
        let out = run_frames(&mut *b, &[f("1111101 0"), f("100 1111111 0")], 1, false);
        assert_eq!(out[0].bits, bits("111111 0"));
        assert_eq!(out[0].info.bit_len, 7);
        assert_eq!(out[1].bits, bits("100"));
    }

    #[test]
    fn frames_mode_lsb_reverses_destuffed_octets() {
        // Air order: octet 0x21 LSB first (1000 0100), then 0xFF LSB first (five ones, a
        // stuffed zero, three ones), then a 3-bit tail.
        let mut b = mk("bitstuff", json!({ "bit_order": "lsb" }), PortType::Frames);
        let f = Owned::from_bits(&bits("10000100 11111 0 111 001"), 0, 0);
        let out = run_frames(&mut *b, &[f], 1, false);
        assert_eq!(out[0].bits, bits("00100001 11111111 100"));
        assert_eq!(out[0].info.bit_len, 19);
    }

    #[test]
    fn lsb_on_a_bits_stream_is_refused() {
        let mut b = mk("bitstuff", json!({ "bit_order": "lsb" }), PortType::Bits);
        let err = b.init(&[PortInfo {
            ty: PortType::Bits,
            rate_hz: 9600.0,
            max_items: 64,
            hold_items: 0,
        }]);
        assert!(err.is_err());
    }
}
