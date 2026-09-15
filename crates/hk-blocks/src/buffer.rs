//! Port buffers and chunk metadata (ADR-0011 §1.1).

use std::sync::Arc;

use hk_model::CrcStatus;
use hk_recipe::PortType;
use hk_stream::inspector::LayerTree;
use num_complex::Complex32;

/// Chunk flags, set on the chunk whose first item they apply to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ChunkFlags(pub u8);

impl ChunkFlags {
    /// Nothing special.
    pub const NONE: Self = Self(0);
    /// Stream start, a gap, lost ring samples or a retune before this chunk: drop history and
    /// re-acquire (clock, sync, frame assembly). Propagated to the block's outputs.
    pub const DISCONTINUITY: Self = Self(1 << 0);
    /// A hot edit rebuilt this block or something upstream at this chunk boundary: state was
    /// reset. Propagated.
    pub const RESET: Self = Self(1 << 1);
    /// The input moved to a different channel (a `follow_hops` instance was re-assigned).
    pub const CHANNEL_CHANGE: Self = Self(1 << 2);
    /// Last chunk of the stream: flush partial frames. Propagated.
    pub const END: Self = Self(1 << 3);

    /// Whether every flag in `other` is set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether any flag is set.
    pub const fn any(self) -> bool {
        self.0 != 0
    }
}

impl std::ops::BitOr for ChunkFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for ChunkFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Metadata of one chunk on one port.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChunkMeta {
    /// Element index of the chunk's first item on this port (a running count from 0).
    pub index: u64,
    /// Source sample index (ring stream counter, fractional after resampling) of the first
    /// item's centre.
    pub source_index: f64,
    /// Source samples per item (decimation), so item `k` is at `source_index + k ·
    /// source_per_item`. Frames carry their own `source_index` instead.
    pub source_per_item: f64,
    /// Item rate on this port, Hz (sample, symbol, bit or nominal frame rate).
    pub rate_hz: f64,
    /// Channel index (0 unless `follow_hops`).
    pub channel: u16,
    /// Flags.
    pub flags: ChunkFlags,
}

impl ChunkMeta {
    /// The first chunk of a stream at `rate_hz` (flags: `DISCONTINUITY`).
    pub fn start(rate_hz: f64) -> Self {
        Self {
            index: 0,
            source_index: 0.0,
            source_per_item: 1.0,
            rate_hz,
            channel: 0,
            flags: ChunkFlags::DISCONTINUITY,
        }
    }

    /// Source sample index of item `offset` in this chunk.
    pub fn source_index_of(&self, offset: usize) -> f64 {
        self.source_index + offset as f64 * self.source_per_item
    }
}

/// Per-frame information on a `frames` port.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameInfo {
    /// Frame counter on this port.
    pub index: u64,
    /// Source sample index of the first bit.
    pub source_index: u64,
    /// Channel index.
    pub channel: u16,
    /// Length in bits; bytes are `ceil(bit_len / 8)`, packed MSB-first, last byte zero-padded.
    pub bit_len: u32,
    /// Check status (`unknown` until a check block ran; `no-crc` if the recipe has none).
    pub check: CrcStatus,
    /// Bits corrected by FEC.
    pub corrected_bits: u32,
    /// Layer tree, once a `fields` block ran. Shared (`Arc`), so frames → frames pass-through
    /// blocks copy a pointer, not the tree.
    pub layers: Option<Arc<LayerTree>>,
}

impl FrameInfo {
    /// A new frame's info (check `unknown`, nothing corrected, no layers, length set by push).
    pub fn new(index: u64, source_index: u64, channel: u16) -> Self {
        Self {
            index,
            source_index,
            channel,
            bit_len: 0,
            check: CrcStatus::Unknown,
            corrected_bits: 0,
            layers: None,
        }
    }
}

/// A borrowed frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frame<'a> {
    /// Packed bytes.
    pub bytes: &'a [u8],
    /// Info.
    pub info: &'a FrameInfo,
}

impl Frame<'_> {
    /// Bit `i` (0 = MSB of byte 0), or `None` past `bit_len`.
    pub fn bit(&self, i: u32) -> Option<u8> {
        (i < self.info.bit_len).then(|| (self.bytes[(i / 8) as usize] >> (7 - i % 8)) & 1)
    }
}

/// A chunk of frames: one byte arena plus per-frame info. Clearing keeps capacity, so frame
/// assembly reuses memory; only layer trees allocate (frame rate, ADR-0011 §1.4).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FrameBuf {
    bytes: Vec<u8>,
    starts: Vec<u32>,
    frames: Vec<FrameInfo>,
}

impl FrameBuf {
    /// Pre-sized for `frames` frames and `bytes` bytes.
    pub fn with_capacity(frames: usize, bytes: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(bytes),
            starts: Vec::with_capacity(frames),
            frames: Vec::with_capacity(frames),
        }
    }

    /// Removes every frame, keeping capacity.
    pub fn clear(&mut self) {
        self.bytes.clear();
        self.starts.clear();
        self.frames.clear();
    }

    /// Number of frames.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// No frames.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Appends packed `bytes` (`info.bit_len` must fit: `bytes.len() == ceil(bit_len / 8)`).
    pub fn push(&mut self, bytes: &[u8], info: FrameInfo) {
        debug_assert_eq!(bytes.len(), info.bit_len.div_ceil(8) as usize);
        self.starts.push(self.bytes.len() as u32);
        self.bytes.extend_from_slice(bytes);
        self.frames.push(info);
    }

    /// Appends a frame from unpacked bits (one byte per bit), packing MSB-first; sets
    /// `info.bit_len`.
    pub fn push_bits(&mut self, bits: &[u8], mut info: FrameInfo) {
        info.bit_len = bits.len() as u32;
        self.starts.push(self.bytes.len() as u32);
        for chunk in bits.chunks(8) {
            let byte = chunk
                .iter()
                .enumerate()
                .fold(0u8, |acc, (k, b)| acc | ((b & 1) << (7 - k)));
            self.bytes.push(byte);
        }
        self.frames.push(info);
    }

    /// Frame `i`.
    pub fn get(&self, i: usize) -> Option<Frame<'_>> {
        let info = self.frames.get(i)?;
        let start = self.starts[i] as usize;
        let len = info.bit_len.div_ceil(8) as usize;
        Some(Frame {
            bytes: &self.bytes[start..start + len],
            info,
        })
    }

    /// Mutable info of frame `i` (check blocks set `check`, `fields` sets `layers`).
    pub fn info_mut(&mut self, i: usize) -> Option<&mut FrameInfo> {
        self.frames.get_mut(i)
    }

    /// Frames in order.
    pub fn iter(&self) -> impl Iterator<Item = Frame<'_>> {
        (0..self.len()).filter_map(|i| self.get(i))
    }
}

/// An input chunk: borrowed items of one port type.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PortSlice<'a> {
    /// Complex baseband.
    Iq(&'a [Complex32]),
    /// Real waveform.
    Real(&'a [f32]),
    /// Soft symbols.
    Soft(&'a [f32]),
    /// Hard bits.
    Bits(&'a [u8]),
    /// Frames.
    Frames(&'a FrameBuf),
}

impl PortSlice<'_> {
    /// Port type.
    pub fn port_type(&self) -> PortType {
        match self {
            PortSlice::Iq(_) => PortType::Iq,
            PortSlice::Real(_) => PortType::Real,
            PortSlice::Soft(_) => PortType::Soft,
            PortSlice::Bits(_) => PortType::Bits,
            PortSlice::Frames(_) => PortType::Frames,
        }
    }

    /// Items (samples, symbols, bits or frames).
    pub fn len(&self) -> usize {
        match self {
            PortSlice::Iq(x) => x.len(),
            PortSlice::Real(x) | PortSlice::Soft(x) => x.len(),
            PortSlice::Bits(x) => x.len(),
            PortSlice::Frames(x) => x.len(),
        }
    }

    /// No items.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// An owned output buffer of one port type.
#[derive(Clone, Debug, PartialEq)]
pub enum PortVec {
    /// Complex baseband.
    Iq(Vec<Complex32>),
    /// Real waveform.
    Real(Vec<f32>),
    /// Soft symbols.
    Soft(Vec<f32>),
    /// Hard bits.
    Bits(Vec<u8>),
    /// Frames.
    Frames(FrameBuf),
}

impl PortVec {
    /// A buffer of `ty` pre-sized for `items` (frames: `items` frames of up to 256 bytes).
    pub fn with_capacity(ty: PortType, items: usize) -> Self {
        match ty {
            PortType::Iq => PortVec::Iq(Vec::with_capacity(items)),
            PortType::Real => PortVec::Real(Vec::with_capacity(items)),
            PortType::Soft => PortVec::Soft(Vec::with_capacity(items)),
            PortType::Bits => PortVec::Bits(Vec::with_capacity(items)),
            PortType::Frames => PortVec::Frames(FrameBuf::with_capacity(items, items * 256)),
        }
    }

    /// Port type.
    pub fn port_type(&self) -> PortType {
        self.as_slice().port_type()
    }

    /// Items.
    pub fn len(&self) -> usize {
        self.as_slice().len()
    }

    /// No items.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Removes every item, keeping capacity.
    pub fn clear(&mut self) {
        match self {
            PortVec::Iq(x) => x.clear(),
            PortVec::Real(x) | PortVec::Soft(x) => x.clear(),
            PortVec::Bits(x) => x.clear(),
            PortVec::Frames(x) => x.clear(),
        }
    }

    /// Borrowed view, e.g. as the next block's input.
    pub fn as_slice(&self) -> PortSlice<'_> {
        match self {
            PortVec::Iq(x) => PortSlice::Iq(x),
            PortVec::Real(x) => PortSlice::Real(x),
            PortVec::Soft(x) => PortSlice::Soft(x),
            PortVec::Bits(x) => PortSlice::Bits(x),
            PortVec::Frames(x) => PortSlice::Frames(x),
        }
    }
}

/// An input: chunk metadata and items.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Input<'a> {
    /// Metadata.
    pub meta: ChunkMeta,
    /// Items.
    pub data: PortSlice<'a>,
}

/// An output port's buffer for the current chunk.
#[derive(Clone, Debug, PartialEq)]
pub struct Output {
    /// Metadata the block sets (see [`Output::begin_chunk`]).
    pub meta: ChunkMeta,
    /// Items the block appends.
    pub data: PortVec,
}

impl Output {
    /// A buffer sized for a port described by `info`.
    pub fn for_port(info: &crate::block::PortInfo) -> Self {
        Self {
            meta: ChunkMeta::start(info.rate_hz),
            data: PortVec::with_capacity(info.ty, info.max_items),
        }
    }

    /// Called by the runtime before each `process`: advances `meta.index` past the previous
    /// chunk, clears the items (capacity kept) and the flags. The block then sets
    /// `source_index`, `source_per_item`, `channel` and any flags, and appends items.
    pub fn begin_chunk(&mut self) {
        self.meta.index += self.data.len() as u64;
        self.meta.flags = ChunkFlags::NONE;
        self.data.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_pack_msb_first_and_reuse_capacity() {
        let mut buf = FrameBuf::with_capacity(4, 64);
        buf.push_bits(&[1, 0, 1, 0, 0, 1, 0, 1, 1, 1], FrameInfo::new(0, 100, 0));
        buf.push(
            &[0xff],
            FrameInfo {
                bit_len: 8,
                ..FrameInfo::new(1, 200, 0)
            },
        );
        let f = buf.get(0).unwrap();
        assert_eq!(f.bytes, &[0xa5, 0xc0]);
        assert_eq!(f.info.bit_len, 10);
        assert_eq!((f.bit(8), f.bit(9), f.bit(10)), (Some(1), Some(1), None));
        assert_eq!(buf.get(1).unwrap().bytes, &[0xff]);
        let cap = buf.bytes.capacity();
        buf.clear();
        assert!(buf.is_empty() && buf.bytes.capacity() == cap);
    }
}
