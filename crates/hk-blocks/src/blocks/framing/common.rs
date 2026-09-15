//! Helpers shared by the framing and FEC blocks (T-087): parameter readers, a function
//! factory, the frames → frames plumbing, a windowed error-rate meter and bit access.

use hk_model::CrcStatus;
use hk_recipe::{BlockDescriptor, Params, PortType, parse_hex};
use serde_json::Value;

use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::buffer::{ChunkFlags, ChunkMeta, FrameBuf, PortSlice, PortVec};
use crate::registry::{BlockFactory, BuildCtx};

/// Reads schema-validated params (defaults applied by the caller).
#[derive(Clone, Copy)]
pub(crate) struct P<'a>(pub &'a Params);

impl<'a> P<'a> {
    pub fn get(&self, key: &str) -> Option<&'a Value> {
        self.0.get(key)
    }
    pub fn int(&self, key: &str) -> Option<i64> {
        self.get(key).and_then(Value::as_i64)
    }
    pub fn int_or(&self, key: &str, d: i64) -> i64 {
        self.int(key).unwrap_or(d)
    }
    pub fn uint(&self, key: &str) -> Result<Option<u32>, BlockError> {
        self.int(key)
            .map(|v| {
                u32::try_from(v).map_err(|_| BlockError::Params(format!("{key} out of range")))
            })
            .transpose()
    }
    pub fn req_uint(&self, key: &str) -> Result<u32, BlockError> {
        self.uint(key)?
            .ok_or_else(|| BlockError::Params(format!("{key} is required")))
    }
    pub fn uint_or(&self, key: &str, d: u32) -> Result<u32, BlockError> {
        Ok(self.uint(key)?.unwrap_or(d))
    }
    pub fn bool_or(&self, key: &str, d: bool) -> bool {
        self.get(key).and_then(Value::as_bool).unwrap_or(d)
    }
    pub fn str(&self, key: &str) -> Option<&'a str> {
        self.get(key).and_then(Value::as_str)
    }
    pub fn hex(&self, key: &str) -> Option<u64> {
        self.str(key).and_then(parse_hex)
    }
    pub fn hex_or(&self, key: &str, d: u64) -> u64 {
        self.hex(key).unwrap_or(d)
    }
    pub fn obj(&self, key: &str) -> Option<P<'a>> {
        self.get(key).and_then(Value::as_object).map(P)
    }
    pub fn list(&self, key: &str) -> &'a [Value] {
        self.get(key)
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice)
    }
}

/// Whether every key that differs between `old` and `new` is in `hot`.
pub(crate) fn only_hot_changed(old: &Params, new: &Params, hot: &[&str]) -> bool {
    old.keys()
        .chain(new.keys())
        .all(|k| old.get(k) == new.get(k) || hot.contains(&k.as_str()))
}

pub(crate) type BuildFn = fn(&Params, &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError>;

/// A factory over a pinned descriptor and a build function.
pub(crate) struct FnFactory {
    descriptor: BlockDescriptor,
    build: BuildFn,
}

impl FnFactory {
    /// The factory for `name` from `pinned` (the group's `planned()`).
    pub fn new(pinned: &[BlockDescriptor], name: &str, build: BuildFn) -> Self {
        let descriptor = pinned
            .iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("{name} has no pinned descriptor"))
            .clone();
        Self { descriptor, build }
    }
}

impl BlockFactory for FnFactory {
    fn descriptor(&self) -> &BlockDescriptor {
        &self.descriptor
    }
    fn build(&self, params: &Params, ctx: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
        (self.build)(params, ctx)
    }
}

/// Checks `init` has one input of an accepted type.
pub(crate) fn one_input(
    name: &str,
    inputs: &[PortInfo],
    accepted: &[PortType],
) -> Result<PortInfo, BlockError> {
    let [input] = inputs else {
        return Err(BlockError::Ports(format!("{name} has exactly one input")));
    };
    if !accepted.contains(&input.ty) {
        return Err(BlockError::PortType {
            port: 0,
            expected: accepted[0],
            got: input.ty,
        });
    }
    Ok(*input)
}

/// Frames → frames plumbing: the input chunk's metadata and frames and the output buffer, with
/// the output metadata copied from the input (flags propagated).
pub(crate) fn frames_io<'a, 'b>(
    io: &'b mut Io<'a>,
) -> Result<(ChunkMeta, &'a FrameBuf, &'b mut FrameBuf), BlockError> {
    let input = io.input(0)?;
    let PortSlice::Frames(frames) = input.data else {
        return Err(BlockError::PortType {
            port: 0,
            expected: PortType::Frames,
            got: input.data.port_type(),
        });
    };
    let out = io.output(0)?;
    out.meta = ChunkMeta {
        index: out.meta.index,
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
    Ok((input.meta, frames, buf))
}

/// Whether a chunk's flags drop history.
pub(crate) fn drops_history(flags: ChunkFlags) -> bool {
    flags.0 & HISTORY_FLAGS.0 != 0
}

/// Flags that drop history.
pub(crate) const HISTORY_FLAGS: ChunkFlags =
    ChunkFlags(ChunkFlags::DISCONTINUITY.0 | ChunkFlags::RESET.0 | ChunkFlags::CHANNEL_CHANGE.0);

/// Where the current bits chunk sits in source time.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Clock {
    /// Running bit index of the chunk's first bit.
    pub first_bit: u64,
    pub source_index: f64,
    pub per_bit: f64,
}

impl Clock {
    /// Source index of running bit `b` (extrapolated for bits of earlier chunks).
    pub fn source(&self, b: u64) -> u64 {
        source_u64(self.source_index + (b as f64 - self.first_bit as f64) * self.per_bit)
    }
}

/// The covered bit range of a frame: from `start_bit` to `end_trim_bits` before its end.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Span {
    pub start: usize,
    pub trim: usize,
}

impl Span {
    /// From the `span` param (absent: the whole frame).
    pub fn from_params(p: P<'_>) -> Result<Self, BlockError> {
        Ok(match p.obj("span") {
            None => Self::default(),
            Some(s) => Self {
                start: s.uint_or("start_bit", 0)? as usize,
                trim: s.uint_or("end_trim_bits", 0)? as usize,
            },
        })
    }
}

/// Bit `i` of MSB-first packed bytes.
#[inline]
pub(crate) fn bit(bytes: &[u8], i: usize) -> u8 {
    (bytes[i / 8] >> (7 - i % 8)) & 1
}

/// `n ≤ 64` bits from `start`, MSB first.
#[inline]
pub(crate) fn read_bits(bytes: &[u8], start: usize, n: usize) -> u64 {
    (start..start + n).fold(0, |acc, i| (acc << 1) | u64::from(bit(bytes, i)))
}

/// Appends `n` bits of packed `bytes` from `start` to unpacked `dst`.
pub(crate) fn extend_bits(dst: &mut Vec<u8>, bytes: &[u8], start: usize, n: usize) {
    dst.extend((start..start + n).map(|i| bit(bytes, i)));
}

/// Appends the low `n` bits of `v`, MSB first, to unpacked `dst`.
pub(crate) fn push_value(dst: &mut Vec<u8>, v: u64, n: usize) {
    dst.extend((0..n).rev().map(|k| ((v >> k) & 1) as u8));
}

/// A check result folded into a frame's existing status: a failure anywhere is a failure; a
/// pass upgrades `unknown`/`no-crc`.
pub(crate) fn combine(existing: CrcStatus, passed: bool) -> CrcStatus {
    match (existing, passed) {
        (CrcStatus::Invalid, _) | (_, false) => CrcStatus::Invalid,
        (CrcStatus::Corrected, true) => CrcStatus::Corrected,
        _ => CrcStatus::Valid,
    }
}

/// Source index (integer) from a fractional one.
pub(crate) fn source_u64(x: f64) -> u64 {
    if x.is_finite() && x > 0.0 {
        x.round() as u64
    } else {
        0
    }
}

/// Failure ratio over the last `window` (≤ 1024) events; `Copy`, never allocates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RateMeter {
    bits: [u64; 16],
    window: u16,
    pos: u16,
    count: u16,
    bad: u16,
}

impl RateMeter {
    pub fn new(window: usize) -> Self {
        Self {
            bits: [0; 16],
            window: window.clamp(1, 1024) as u16,
            pos: 0,
            count: 0,
            bad: 0,
        }
    }

    pub fn push(&mut self, bad: bool) {
        let (w, b) = (usize::from(self.pos) / 64, 1u64 << (self.pos % 64));
        if self.count == self.window {
            if self.bits[w] & b != 0 {
                self.bad -= 1;
            }
        } else {
            self.count += 1;
        }
        if bad {
            self.bits[w] |= b;
            self.bad += 1;
        } else {
            self.bits[w] &= !b;
        }
        self.pos = (self.pos + 1) % self.window;
    }

    /// Failures in the window.
    pub fn bad(&self) -> u16 {
        self.bad
    }

    /// Failure ratio, `None` before any event.
    pub fn rate(&self) -> Option<f32> {
        (self.count > 0).then(|| f32::from(self.bad) / f32::from(self.count))
    }

    pub fn clear(&mut self) {
        *self = Self::new(usize::from(self.window));
    }
}

/// Bits per frame a frames chunk is sized for: `PortVec::with_capacity` pre-sizes a frames
/// port's arena at 256 bytes per item, so a chunk of `max_items` frames carries at most
/// `max_items × FRAME_BITS_PER_ITEM` bits without reallocating. Blocks that split frames
/// bound their output count from it.
pub(crate) const FRAME_BITS_PER_ITEM: usize = 256 * 8;

/// Frame output port info for a frames → frames block.
pub(crate) fn frames_port(input: PortInfo, max_items: usize) -> PortInfo {
    PortInfo {
        ty: PortType::Frames,
        max_items: max_items.max(1),
        hold_items: 0,
        ..input
    }
}

/// `update_params` for a block whose hot keys are applied by `apply`.
pub(crate) fn update_hot(
    current: &mut Params,
    new: &Params,
    hot: &[&str],
    apply: impl FnOnce(P<'_>),
) -> Result<ParamUpdate, BlockError> {
    if !only_hot_changed(current, new, hot) {
        return Ok(ParamUpdate::Rebuild);
    }
    *current = new.clone();
    apply(P(new));
    Ok(ParamUpdate::Applied)
}

#[cfg(test)]
pub(crate) mod testutil {
    //! Drives blocks the way the runtime does: build through the registry (schema-validated),
    //! `init`, `begin_chunk` + `process` per chunk.

    use std::collections::BTreeMap;

    use hk_recipe::{Params, PortType};
    use serde_json::Value;

    use crate::block::{Block, Io, PortInfo};
    use crate::buffer::{
        ChunkFlags, ChunkMeta, FrameBuf, FrameInfo, Input, Output, PortSlice, PortVec,
    };
    use crate::registry::{BuildCtx, Registry};

    pub fn build(name: &str, params: Value, input: PortType) -> Box<dyn Block> {
        try_build(name, params, input).unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    /// `build`, returning the build error.
    pub fn try_build(name: &str, params: Value, input: PortType) -> Result<Box<dyn Block>, String> {
        let maps = BTreeMap::new();
        let ctx = BuildCtx {
            field_maps: &maps,
            input_types: &[input],
        };
        let p: Params = params.as_object().cloned().unwrap_or_default();
        Registry::builtin()
            .build(name, &p, &ctx)
            .map_err(|e| e.to_string())
    }

    /// An owned frame.
    #[derive(Clone, Debug, PartialEq)]
    pub struct Owned {
        pub bits: Vec<u8>,
        pub info: FrameInfo,
    }

    impl Owned {
        pub fn from_bits(bits: &[u8], index: u64, source: u64) -> Self {
            let mut info = FrameInfo::new(index, source, 0);
            info.bit_len = bits.len() as u32;
            Self {
                bits: bits.to_vec(),
                info,
            }
        }
    }

    fn collect(out: &Output, into: &mut Vec<Owned>) {
        let PortVec::Frames(buf) = &out.data else {
            panic!("frames output expected")
        };
        for f in buf.iter() {
            let bits = (0..f.info.bit_len).map(|i| f.bit(i).unwrap()).collect();
            into.push(Owned {
                bits,
                info: f.info.clone(),
            });
        }
    }

    fn port(ty: PortType, max_items: usize) -> PortInfo {
        PortInfo {
            ty,
            rate_hz: 1200.0,
            max_items,
            hold_items: 0,
        }
    }

    /// Runs `bits` through `block` in chunks of `chunk` (the last chunk flagged `END` when
    /// `end`); returns the frames and the flags seen on the output.
    pub fn run_bits(block: &mut dyn Block, bits: &[u8], chunk: usize, end: bool) -> Vec<Owned> {
        let info = block.init(&[port(PortType::Bits, chunk)]).unwrap();
        let mut outputs = vec![Output::for_port(&info[0])];
        let mut frames = Vec::new();
        let n = bits.chunks(chunk).count();
        for (k, c) in bits.chunks(chunk).enumerate() {
            outputs[0].begin_chunk();
            let mut meta = ChunkMeta {
                index: (k * chunk) as u64,
                source_index: (k * chunk * 10) as f64,
                source_per_item: 10.0,
                flags: if k == 0 {
                    ChunkFlags::DISCONTINUITY
                } else {
                    ChunkFlags::NONE
                },
                ..ChunkMeta::start(1200.0)
            };
            if end && k + 1 == n {
                meta.flags |= ChunkFlags::END;
            }
            let inputs = [Input {
                meta,
                data: PortSlice::Bits(c),
            }];
            block.process(&mut Io::new(&inputs, &mut outputs)).unwrap();
            assert_eq!(outputs[0].meta.flags, meta.flags, "flags propagate");
            collect(&outputs[0], &mut frames);
        }
        frames
    }

    /// Runs frames through `block`, `per_chunk` frames per chunk.
    pub fn run_frames(
        block: &mut dyn Block,
        input: &[Owned],
        per_chunk: usize,
        end: bool,
    ) -> Vec<Owned> {
        run_frames_period(block, input, per_chunk, end, 1.0)
    }

    /// [`run_frames`] with the chunks' frame period (`source_per_item`) set: blocks that check
    /// frame contiguity (`crc`'s synced correction) need it to match the frames' source indexes.
    pub fn run_frames_period(
        block: &mut dyn Block,
        input: &[Owned],
        per_chunk: usize,
        end: bool,
        period: f64,
    ) -> Vec<Owned> {
        let info = block.init(&[port(PortType::Frames, per_chunk)]).unwrap();
        let mut outputs = vec![Output::for_port(&info[0])];
        let mut frames = Vec::new();
        let n = input.chunks(per_chunk).count();
        for (k, c) in input.chunks(per_chunk).enumerate() {
            let mut buf = FrameBuf::with_capacity(c.len(), 64);
            for f in c {
                buf.push_bits(&f.bits, f.info.clone());
            }
            outputs[0].begin_chunk();
            let mut meta = ChunkMeta {
                source_per_item: period,
                flags: if k == 0 {
                    ChunkFlags::DISCONTINUITY
                } else {
                    ChunkFlags::NONE
                },
                ..ChunkMeta::start(2.0)
            };
            if end && k + 1 == n {
                meta.flags |= ChunkFlags::END;
            }
            let inputs = [Input {
                meta,
                data: PortSlice::Frames(&buf),
            }];
            block.process(&mut Io::new(&inputs, &mut outputs)).unwrap();
            collect(&outputs[0], &mut frames);
        }
        frames
    }

    /// Unpacked bits of `v`'s low `n` bits, MSB first.
    pub fn bits_of(v: u64, n: usize) -> Vec<u8> {
        (0..n).rev().map(|k| ((v >> k) & 1) as u8).collect()
    }

    /// Unpacked bits of bytes, MSB first.
    pub fn bytes_bits(bytes: &[u8]) -> Vec<u8> {
        bytes
            .iter()
            .flat_map(|&b| bits_of(u64::from(b), 8))
            .collect()
    }

    /// Deterministic pseudo-random bits.
    pub fn noise(n: usize, seed: u64) -> Vec<u8> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s >> 33) as u8 & 1
            })
            .collect()
    }
}
