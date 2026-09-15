//! T-088 (ADR-0011 §1.4 rule 1): a running recipe graph allocates nothing per chunk on
//! sample-rate ports in steady state, and a hot-edit swap on the pipeline thread allocates
//! nothing either (everything it needs was staged off the real-time thread).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use hk_blocks::{ChunkFlags, ChunkMeta, Input, PortInfo, PortSlice, Registry};
use hk_pipeline::recipes::graph::{Graph, stage};
use hk_pipeline::recipes::runtime::parse_recipe;
use hk_pipeline::recipes::swap::apply;
use hk_recipe::PortType;
use num_complex::Complex32;
use serde_json::json;

struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
thread_local! {
    static COUNT_HERE: Cell<bool> = const { Cell::new(false) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNT_HERE.try_with(Cell::get).unwrap_or(false) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNT_HERE.try_with(Cell::get).unwrap_or(false) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn counted<R>(f: impl FnOnce() -> R) -> (R, u64) {
    ALLOCS.store(0, Ordering::Relaxed);
    COUNT_HERE.with(|c| c.set(true));
    let r = f();
    COUNT_HERE.with(|c| c.set(false));
    (r, ALLOCS.load(Ordering::Relaxed))
}

fn chain(ids: &[&str]) -> hk_recipe::Recipe {
    let nodes: Vec<_> = ids
        .iter()
        .map(|id| json!({"id": id, "block": "identity"}))
        .collect();
    parse_recipe(json!({
        "schema": "hackriff.recipe", "schema_version": 2, "id": "alloc", "version": 1,
        "name": "alloc", "input": {"port": "iq", "sample_rate_hz": 100000.0},
        "nodes": nodes,
        "outputs": [{"id": "s", "kind": "stage", "from": ids[0]}],
        "output_policy": {"content_class": "unrestricted"}
    }))
    .unwrap()
}

#[test]
fn a_running_graph_and_a_swap_allocate_nothing_on_the_pipeline_thread() {
    const N: usize = 4096;
    let registry = Registry::builtin();
    let input = PortInfo {
        ty: PortType::Iq,
        rate_hz: 1e5,
        max_items: N,
        hold_items: 0,
    };
    let old = Arc::new(chain(&["a", "b", "c"]));
    let mut staged = stage(None, Arc::clone(&old), &registry, input).unwrap();
    let shape = staged.shape.clone();
    let mut g = Graph::empty(input);
    apply(&mut g, &mut staged).unwrap();

    let samples = vec![Complex32::new(0.5, -0.25); N];
    let mut meta = ChunkMeta::start(1e5);
    let step = |g: &mut Graph, meta: &mut ChunkMeta| {
        g.process(Input {
            meta: *meta,
            data: PortSlice::Iq(&samples),
        })
        .unwrap();
        meta.flags = ChunkFlags::NONE;
        meta.index += N as u64;
        meta.source_index += 10.0 * N as f64;
    };
    for _ in 0..4 {
        step(&mut g, &mut meta);
    }
    let ((), allocs) = counted(|| {
        for _ in 0..1000 {
            step(&mut g, &mut meta);
        }
    });
    assert_eq!(allocs, 0, "steady-state chunks allocate nothing");

    // An edit: `b` removed, `d` added. Staged off the thread; the swap itself allocates nothing.
    let new = Arc::new(chain(&["a", "c", "d"]));
    let mut staged = stage(Some((&old, &shape)), new, &registry, input).unwrap();
    let (report, allocs) = counted(|| apply(&mut g, &mut staged).unwrap());
    assert_eq!(allocs, 0, "the swap allocates nothing");
    assert_eq!((report.rebuilt, report.kept), (2, 1), "{report:?}");
    let ids: Vec<&str> = g.nodes.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(ids, ["a", "c", "d"]);
    step(&mut g, &mut meta);
    let ((), allocs) = counted(|| {
        for _ in 0..100 {
            step(&mut g, &mut meta);
        }
    });
    assert_eq!(
        allocs, 0,
        "the swapped-in graph allocates nothing per chunk"
    );
    // Dropping the retired instances happens off the pipeline thread (here: after counting).
    drop(staged);
}

/// T-111: a `messages` output's pipeline-thread side (filter a frame, `try_send` its time, channel
/// and `Arc` layer tree onto the bounded writer queue) allocates nothing, and a full queue drops
/// and counts instead of blocking or allocating. Frames that are not CRC-valid are not queued.
#[test]
fn a_messages_output_queues_frames_without_allocating() {
    use hk_blocks::{FrameInfo, Output, PortVec};
    use hk_model::{CrcStatus, Timestamp};
    use hk_pipeline::recipes::messages::MessagesSink;
    use hk_pipeline::recipes::runtime::PipelineStats;
    use hk_pipeline::recipes::taps::FrameCtx;
    use hk_stream::inspector::{FitStatus, LayerTree};

    const FRAMES: usize = 8;
    let tree = Arc::new(LayerTree {
        nodes: Vec::new(),
        byte_index: Vec::new(),
        fit: FitStatus::Ok,
        errors: Vec::new(),
    });
    let mut out = Output::for_port(&PortInfo {
        ty: PortType::Frames,
        rate_hz: 1e5,
        max_items: FRAMES,
        hold_items: 0,
    });
    let PortVec::Frames(buf) = &mut out.data else {
        unreachable!("a frames port")
    };
    for i in 0..FRAMES {
        let mut info = FrameInfo::new(i as u64, 100 * i as u64, 0);
        info.bit_len = 16;
        info.check = if i % 4 == 3 {
            CrcStatus::Invalid
        } else {
            CrcStatus::Valid
        };
        info.layers = Some(Arc::clone(&tree));
        buf.push(&[0xab, 0xcd], info);
    }
    let stats = Arc::new(PipelineStats::default());
    let (mut sink, rx) = MessagesSink::with_queue(64, Arc::clone(&stats));
    let ctx = FrameCtx {
        decoder: "recipe:alloc@1",
        frame_model: "alloc",
        emitter_id: None,
        channel_hz: 1e6,
        channels_hz: &[],
        recipe_version: 1,
        edit_rev: 0,
    };
    let t_of = |s: f64| Timestamp::from_unix_nanos(s as i64);
    assert_eq!(
        sink.publish(&out, &ctx, &t_of),
        6,
        "warm-up: 6 CRC-valid frames"
    );
    let (queued, allocs) = counted(|| sink.publish(&out, &ctx, &t_of));
    assert_eq!((queued, allocs), (6, 0), "queuing frames allocates nothing");
    assert_eq!(rx.try_iter().count(), 12);

    let (queued, allocs) = counted(|| {
        (0..11)
            .map(|_| sink.publish(&out, &ctx, &t_of))
            .sum::<u64>()
    });
    assert_eq!(allocs, 0, "a full queue drops without allocating");
    assert_eq!(queued, 64);
    assert_eq!(stats.decodes_dropped.load(Ordering::Relaxed), 2);
    assert_eq!(rx.try_iter().count(), 64);

    // T-112: nothing outlives the test. Dropping the sink closes its queue (the receiver sees the
    // disconnect, no sender left) and releases its frames' layer trees; the output buffer is the
    // last holder of the tree.
    drop(sink);
    assert!(matches!(
        rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Disconnected)
    ));
    drop(rx);
    drop(out);
    assert_eq!(
        Arc::strong_count(&tree),
        1,
        "no queued frame still holds the tree"
    );
}
