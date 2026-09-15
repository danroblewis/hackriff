//! `follow_hops`: the merge node of a follow-hops recipe (ADR-0011 §2.5, T-093).
//!
//! The runtime runs everything upstream of this node once per channel and hands it every
//! channel's frames (each tagged with `FrameInfo::channel`) as one frames input per chunk. The
//! block turns them into one stream:
//!
//! - **Ordered by frame end.** A frame is complete only at its last bit, so the merge orders
//!   frames by when they **ended** in source time, not when they started: a multi-batch POCSAG
//!   message that started seconds ago leaves after the short page on another channel that ended
//!   before it, and neither is held longer than `order_window_s` (the ADR-0011 latency bound).
//!   A frame's end is its arrival watermark: the input chunk's `meta.source_index` (the runtime
//!   sets it to the ring sample every channel has been processed up to), or its own
//!   `source_index` if later. Frames that ended within the same input chunk are ordered by start
//!   (`source_index`, then channel), so the end resolution is one input chunk. Frames wait in a
//!   bounded reorder buffer sorted by `(end, source_index, channel)` and leave once the
//!   watermark (the larger of `meta.source_index` and the newest frame start seen) has advanced
//!   `order_window_s` past their end. A frame that arrives with an end earlier than one that
//!   already left (the input's watermark went backwards) is emitted at once and counted `late`;
//!   a long frame is never late for being long.
//! - **Deduplicated.** A frame with the same `bit_len` and bytes as one from *another* channel
//!   within `dedupe_s` (source time) is a duplicate: the same transmission decoded on an adjacent
//!   channel or an overlapping window. While both wait in the buffer the better copy is kept
//!   (check `valid` > `no-crc` > `unknown` > `invalid`, then fewer corrected bits, then the
//!   first); once a copy has left, later copies are dropped (matched on a 64-bit FNV-1a hash of
//!   `bit_len` and bytes). Repeats on the *same* channel are separate transmissions and are kept.
//!   Duplicates are counted in status (`dups`).
//! - **Bounded.** At most [`MAX_PENDING`] frames wait; beyond it the oldest leaves early
//!   (`overflow`). The dedupe history keeps the last [`HISTORY`] frames that left.
//!
//! Flags: `DISCONTINUITY`/`RESET` release every waiting frame (they were decoded before the
//! break) and clear the dedupe history; `END` releases everything; `CHANNEL_CHANGE` (a channel
//! added or removed) keeps both. All flags propagate. Allocation: the buffers are sized at
//! `init`; a waiting frame's bytes reuse a pooled vector (a frame longer than
//! [`POOLED_BYTES`] grows its vector once, frame-rate allocation per ADR-0011 §1.4).

use std::collections::VecDeque;

use hk_model::CrcStatus;
use hk_recipe::{BlockDescriptor, Params, PortType};

use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::buffer::{ChunkFlags, ChunkMeta, FrameInfo, PortSlice, PortVec};
use crate::registry::{BlockFactory, BuildCtx};
use crate::status::Status;

/// Most frames waiting in the reorder buffer.
pub const MAX_PENDING: usize = 256;
/// Frames that left, remembered for deduplication.
pub const HISTORY: usize = 256;
/// Bytes each pooled frame vector is pre-sized to.
pub const POOLED_BYTES: usize = 256;

const DEFAULT_DEDUPE_S: f64 = 0.5;
const DEFAULT_ORDER_WINDOW_S: f64 = 0.2;

fn float_param(p: &Params, key: &str, default: f64) -> f64 {
    p.get(key)
        .and_then(serde_json::Value::as_f64)
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(default)
}

/// Builds [`FollowHops`].
pub struct FollowHopsFactory {
    descriptor: BlockDescriptor,
}

impl FollowHopsFactory {
    /// The factory over the pinned descriptor.
    pub fn new(descriptor: BlockDescriptor) -> Self {
        Self { descriptor }
    }
}

impl BlockFactory for FollowHopsFactory {
    fn descriptor(&self) -> &BlockDescriptor {
        &self.descriptor
    }

    fn build(&self, params: &Params, _ctx: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
        Ok(Box::new(FollowHops::new(
            float_param(params, "dedupe_s", DEFAULT_DEDUPE_S),
            float_param(params, "order_window_s", DEFAULT_ORDER_WINDOW_S),
        )))
    }
}

struct Pending {
    /// Source index the frame ended by (its arrival watermark): the ordering key.
    end: u64,
    hash: u64,
    bytes: Vec<u8>,
    info: FrameInfo,
}

#[derive(Clone, Copy)]
struct Left {
    hash: u64,
    bit_len: u32,
    source_index: u64,
    channel: u16,
}

/// The merge block.
pub struct FollowHops {
    dedupe_s: f64,
    order_window_s: f64,
    pending: VecDeque<Pending>,
    pool: Vec<Vec<u8>>,
    history: VecDeque<Left>,
    /// Source samples per second (from the input chunk meta).
    source_rate: f64,
    watermark: f64,
    /// Latest end of a frame that left.
    left_upto: Option<u64>,
    index: u64,
    dups: u64,
    late: u64,
    overflow: u64,
    status: Status,
}

impl FollowHops {
    /// A merge with a duplicate window of `dedupe_s` and a reorder window of `order_window_s`.
    pub fn new(dedupe_s: f64, order_window_s: f64) -> Self {
        Self {
            dedupe_s,
            order_window_s,
            pending: VecDeque::new(),
            pool: Vec::new(),
            history: VecDeque::new(),
            source_rate: 0.0,
            watermark: f64::NEG_INFINITY,
            left_upto: None,
            index: 0,
            dups: 0,
            late: 0,
            overflow: 0,
            status: Status::default(),
        }
    }

    /// Duplicates dropped so far.
    pub fn dups(&self) -> u64 {
        self.dups
    }

    fn samples(&self, seconds: f64) -> f64 {
        if self.source_rate > 0.0 {
            seconds * self.source_rate
        } else {
            0.0
        }
    }

    fn ingest(
        &mut self,
        bytes: &[u8],
        info: &FrameInfo,
        end: u64,
        out: &mut crate::buffer::FrameBuf,
    ) {
        let hash = fnv(info.bit_len, bytes);
        let dd = self.samples(self.dedupe_s);
        let near = |a: u64, b: u64| (a as f64 - b as f64).abs() <= dd;
        if let Some(i) = self.pending.iter().position(|p| {
            p.hash == hash
                && p.info.bit_len == info.bit_len
                && p.info.channel != info.channel
                && near(p.info.source_index, info.source_index)
                && p.bytes == bytes
        }) {
            self.dups += 1;
            if better(info, &self.pending[i].info)
                && let Some(mut p) = self.pending.remove(i)
            {
                p.info = info.clone();
                self.insert(p);
            }
            return;
        }
        if self.history.iter().any(|l| {
            l.hash == hash
                && l.bit_len == info.bit_len
                && l.channel != info.channel
                && near(l.source_index, info.source_index)
        }) {
            self.dups += 1;
            return;
        }
        if self.pending.len() >= MAX_PENDING {
            self.overflow += 1;
            self.release_front(out);
        }
        let mut buf = self.pool.pop().unwrap_or_default();
        buf.clear();
        buf.extend_from_slice(bytes);
        let p = Pending {
            end,
            hash,
            bytes: buf,
            info: info.clone(),
        };
        if self.left_upto.is_some_and(|u| end < u) {
            // It ended before a frame that already left: emit now rather than out of order later.
            self.late += 1;
            self.emit(p, out);
        } else {
            self.insert(p);
        }
    }

    fn insert(&mut self, p: Pending) {
        let key = (p.end, p.info.source_index, p.info.channel);
        let at = self
            .pending
            .partition_point(|q| (q.end, q.info.source_index, q.info.channel) <= key);
        self.pending.insert(at, p);
    }

    fn release_front(&mut self, out: &mut crate::buffer::FrameBuf) {
        if let Some(p) = self.pending.pop_front() {
            self.emit(p, out);
        }
    }

    fn emit(&mut self, mut p: Pending, out: &mut crate::buffer::FrameBuf) {
        p.info.index = self.index;
        self.index += 1;
        out.push(&p.bytes, p.info.clone());
        if self.history.len() >= HISTORY {
            self.history.pop_front();
        }
        self.history.push_back(Left {
            hash: p.hash,
            bit_len: p.info.bit_len,
            source_index: p.info.source_index,
            channel: p.info.channel,
        });
        self.left_upto = Some(self.left_upto.map_or(p.end, |u| u.max(p.end)));
        self.pool.push(p.bytes);
    }

    fn release_all(&mut self, out: &mut crate::buffer::FrameBuf) {
        while !self.pending.is_empty() {
            self.release_front(out);
        }
    }
}

/// Whether copy `a` is better than `b`.
fn better(a: &FrameInfo, b: &FrameInfo) -> bool {
    let rank = |c: CrcStatus| match c {
        CrcStatus::Valid => 3,
        CrcStatus::NoCrc => 2,
        CrcStatus::Unknown => 1,
        CrcStatus::Invalid => 0,
    };
    (rank(a.check), std::cmp::Reverse(a.corrected_bits))
        > (rank(b.check), std::cmp::Reverse(b.corrected_bits))
}

/// 64-bit FNV-1a over `bit_len` (LE) and the bytes.
fn fnv(bit_len: u32, bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bit_len.to_le_bytes().iter().chain(bytes) {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

impl Block for FollowHops {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let [input] = inputs else {
            return Err(BlockError::Ports(
                "follow_hops has exactly one input".into(),
            ));
        };
        if input.ty != PortType::Frames {
            return Err(BlockError::PortType {
                port: 0,
                expected: PortType::Frames,
                got: input.ty,
            });
        }
        self.pending = VecDeque::with_capacity(MAX_PENDING + 1);
        self.history = VecDeque::with_capacity(HISTORY + 1);
        self.pool = (0..MAX_PENDING + 1)
            .map(|_| Vec::with_capacity(POOLED_BYTES))
            .collect();
        self.pool.reserve(MAX_PENDING + 1);
        let hold = (self.order_window_s * input.rate_hz.max(0.0)).ceil();
        Ok(vec![PortInfo {
            ty: PortType::Frames,
            rate_hz: input.rate_hz,
            max_items: input.max_items + MAX_PENDING,
            hold_items: if hold.is_finite() { hold as usize } else { 0 },
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
        let meta = input.meta;
        let rate = meta.rate_hz * meta.source_per_item;
        if rate.is_finite() && rate > 0.0 {
            self.source_rate = rate;
        }
        let out = io.output(0)?;
        let first_index = self.index;
        let PortVec::Frames(buf) = &mut out.data else {
            return Err(BlockError::Ports(
                "follow_hops output must be frames".into(),
            ));
        };
        if meta.flags.contains(ChunkFlags::DISCONTINUITY) || meta.flags.contains(ChunkFlags::RESET)
        {
            self.release_all(buf);
            self.history.clear();
            self.left_upto = None;
            self.watermark = f64::NEG_INFINITY;
        }
        // Every frame of this chunk ended by the ring sample all channels were processed up to.
        let arrived = if meta.source_index.is_finite() && meta.source_index > 0.0 {
            meta.source_index as u64
        } else {
            0
        };
        for f in frames.iter() {
            self.ingest(f.bytes, f.info, arrived.max(f.info.source_index), buf);
            self.watermark = self.watermark.max(f.info.source_index as f64);
        }
        if meta.source_index.is_finite() {
            self.watermark = self.watermark.max(meta.source_index);
        }
        if meta.flags.contains(ChunkFlags::END) {
            self.release_all(buf);
        } else {
            let win = self.samples(self.order_window_s);
            while self
                .pending
                .front()
                .is_some_and(|p| p.end as f64 + win <= self.watermark)
            {
                self.release_front(buf);
            }
        }
        out.meta = ChunkMeta {
            index: first_index,
            ..meta
        };
        self.status.items_in += frames.len() as u64;
        self.status.items_out = self.index;
        Ok(())
    }

    fn reset(&mut self) {
        while let Some(p) = self.pending.pop_front() {
            self.pool.push(p.bytes);
        }
        self.history.clear();
        self.left_upto = None;
        self.watermark = f64::NEG_INFINITY;
    }

    fn update_params(
        &mut self,
        params: &Params,
        _ctx: &BuildCtx<'_>,
    ) -> Result<ParamUpdate, BlockError> {
        let window = float_param(params, "order_window_s", DEFAULT_ORDER_WINDOW_S);
        if window != self.order_window_s {
            return Ok(ParamUpdate::Rebuild);
        }
        self.dedupe_s = float_param(params, "dedupe_s", DEFAULT_DEDUPE_S);
        Ok(ParamUpdate::Applied)
    }

    fn status(&self) -> Status {
        let mut s = self.status;
        s.extra.set("dups", self.dups as f64);
        s.extra.set("late", self.late as f64);
        s.extra.set("overflow", self.overflow as f64);
        s.extra.set("pending", self.pending.len() as f64);
        s
    }
}
