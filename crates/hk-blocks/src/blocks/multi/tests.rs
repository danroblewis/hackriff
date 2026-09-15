//! `follow_hops` unit tests: ordering within the window, deduplication across channels (best
//! copy kept), same-channel repeats kept, flushing on flags, hot `dedupe_s`.

use hk_model::CrcStatus;
use hk_recipe::PortType;
use serde_json::json;

use crate::Registry;
use crate::block::{Block, Io, ParamUpdate, PortInfo};
use crate::buffer::{
    ChunkFlags, ChunkMeta, FrameBuf, FrameInfo, Input, Output, PortSlice, PortVec,
};
use crate::registry::BuildCtx;

/// Source rate 1000 samples/s: `order_window_s` 0.2 = 200 samples, `dedupe_s` 0.5 = 500.
const RATE: f64 = 1000.0;

fn build(params: serde_json::Value) -> Box<dyn Block> {
    let ctx = BuildCtx {
        field_maps: &Default::default(),
        input_types: &[PortType::Frames],
    };
    Registry::builtin()
        .build("follow_hops", params.as_object().unwrap(), &ctx)
        .unwrap()
}

struct Harness {
    block: Box<dyn Block>,
    out: Output,
}

/// `(bytes, source_index, channel, check, corrected_bits)`.
type F = (&'static [u8], u64, u16, CrcStatus, u32);

impl Harness {
    fn new(params: serde_json::Value) -> Self {
        let mut block = build(params);
        let info = PortInfo {
            ty: PortType::Frames,
            rate_hz: 10.0,
            max_items: 64,
            hold_items: 0,
        };
        let ports = block.init(&[info]).unwrap();
        Self {
            block,
            out: Output::for_port(&ports[0]),
        }
    }

    /// One chunk with watermark `wm`; returns `(first byte, source_index, channel)` out.
    fn chunk(&mut self, frames: &[F], wm: f64, flags: ChunkFlags) -> Vec<(u8, u64, u16)> {
        let mut buf = FrameBuf::with_capacity(8, 64);
        for (bytes, s, ch, check, fixed) in frames {
            let mut info = FrameInfo::new(0, *s, *ch);
            info.bit_len = bytes.len() as u32 * 8;
            info.check = *check;
            info.corrected_bits = *fixed;
            buf.push(bytes, info);
        }
        let meta = ChunkMeta {
            source_index: wm,
            source_per_item: RATE / 10.0,
            flags,
            ..ChunkMeta::start(10.0)
        };
        self.out.begin_chunk();
        let inputs = [Input {
            meta,
            data: PortSlice::Frames(&buf),
        }];
        self.block
            .process(&mut Io::new(&inputs, std::slice::from_mut(&mut self.out)))
            .unwrap();
        assert_eq!(self.out.meta.flags, flags, "flags propagate");
        let PortVec::Frames(o) = &self.out.data else {
            panic!("frames out")
        };
        o.iter()
            .map(|f| (f.bytes[0], f.info.source_index, f.info.channel))
            .collect()
    }
}

const V: CrcStatus = CrcStatus::Valid;
const N: ChunkFlags = ChunkFlags::NONE;

#[test]
fn frames_leave_in_source_time_order_after_the_window() {
    let mut h = Harness::new(json!({"order_window_s": 0.2, "dedupe_s": 0.5}));
    // Channel 1's frame at 1100 arrives before channel 0's earlier frame at 1050.
    assert!(h.chunk(&[(b"b", 1100, 1, V, 0)], 1150.0, N).is_empty());
    assert!(h.chunk(&[(b"a", 1050, 0, V, 0)], 1200.0, N).is_empty());
    // Watermark 1250: 1050 + 200 <= 1250 leaves, 1100 waits.
    assert_eq!(h.chunk(&[], 1250.0, N), vec![(b'a', 1050, 0)]);
    assert_eq!(h.chunk(&[], 1300.0, N), vec![(b'b', 1100, 1)]);
    // A frame older than one that already left is emitted at once and counted late.
    assert_eq!(
        h.chunk(&[(b"c", 1000, 2, V, 0)], 1300.0, N),
        vec![(b'c', 1000, 2)]
    );
    let s = h.block.status();
    assert_eq!(s.items_out, 3);
    assert_eq!(s.extra.iter().find(|(k, _)| *k == "late").unwrap().1, 1.0);
}

#[test]
fn a_duplicate_on_another_channel_keeps_the_best_copy_and_is_counted() {
    let mut h = Harness::new(json!({"order_window_s": 0.2, "dedupe_s": 0.5}));
    // Same bytes on channels 0 (2 corrected bits) and 1 (clean) 30 samples apart: one leaves,
    // the clean copy from channel 1.
    h.chunk(&[(b"x", 2000, 0, V, 2)], 2000.0, N);
    h.chunk(&[(b"x", 2030, 1, V, 0)], 2030.0, N);
    // An invalid copy on channel 2 loses to both.
    h.chunk(&[(b"x", 2010, 2, CrcStatus::Invalid, 0)], 2040.0, N);
    assert_eq!(h.chunk(&[], 2300.0, N), vec![(b'x', 2030, 1)]);
    // A late copy after the kept one left is dropped too (history), within dedupe_s.
    assert!(h.chunk(&[(b"x", 2400, 3, V, 0)], 2400.0, N).is_empty());
    assert!(h.chunk(&[], 3000.0, N).is_empty());
    // Beyond dedupe_s the same bytes are a new transmission.
    h.chunk(&[(b"x", 3100, 3, V, 0)], 3100.0, N);
    assert_eq!(h.chunk(&[], 3400.0, N), vec![(b'x', 3100, 3)]);
    let s = h.block.status();
    assert_eq!(s.extra.iter().find(|(k, _)| *k == "dups").unwrap().1, 3.0);
}

#[test]
fn repeats_on_the_same_channel_are_kept() {
    let mut h = Harness::new(json!({"order_window_s": 0.2}));
    h.chunk(&[(b"r", 100, 0, V, 0), (b"r", 150, 0, V, 0)], 150.0, N);
    assert_eq!(
        h.chunk(&[], 1000.0, N),
        vec![(b'r', 100, 0), (b'r', 150, 0)]
    );
}

#[test]
fn end_and_discontinuity_release_everything_channel_change_does_not() {
    let mut h = Harness::new(json!({}));
    h.chunk(&[(b"p", 500, 0, V, 0)], 500.0, N);
    assert!(h.chunk(&[], 510.0, ChunkFlags::CHANNEL_CHANGE).is_empty());
    assert_eq!(h.chunk(&[], 520.0, ChunkFlags::END), vec![(b'p', 500, 0)]);
    h.chunk(&[(b"q", 600, 1, V, 0)], 600.0, N);
    // The discontinuity releases what waited before this chunk's frames join the buffer.
    assert_eq!(
        h.chunk(&[(b"z", 50, 0, V, 0)], 50.0, ChunkFlags::DISCONTINUITY),
        vec![(b'q', 600, 1)]
    );
    // History was cleared: `q` on another channel is not a duplicate any more.
    h.chunk(&[(b"q", 60, 2, V, 0)], 60.0, N);
    assert_eq!(h.chunk(&[], 5000.0, N), vec![(b'z', 50, 0), (b'q', 60, 2)]);
}

#[test]
fn dedupe_is_hot_and_the_order_window_rebuilds() {
    let mut b = build(json!({}));
    let ctx = BuildCtx {
        field_maps: &Default::default(),
        input_types: &[PortType::Frames],
    };
    let p = json!({"dedupe_s": 2.0});
    assert_eq!(
        b.update_params(p.as_object().unwrap(), &ctx).unwrap(),
        ParamUpdate::Applied
    );
    let p = json!({"dedupe_s": 2.0, "order_window_s": 1.0});
    assert_eq!(
        b.update_params(p.as_object().unwrap(), &ctx).unwrap(),
        ParamUpdate::Rebuild
    );
}

#[test]
fn the_reorder_buffer_is_bounded() {
    let mut h = Harness::new(json!({"order_window_s": 5.0, "dedupe_s": 0.0}));
    let mut out = 0;
    for k in 0..(super::follow_hops::MAX_PENDING as u64 + 10) {
        let bytes: &'static [u8] =
            Box::leak(vec![(k % 250) as u8, (k >> 8) as u8].into_boxed_slice());
        out += h.chunk(&[(bytes, k, 0, V, 0)], k as f64, N).len();
    }
    assert_eq!(out, 10, "the oldest leave early once the buffer is full");
}
