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
