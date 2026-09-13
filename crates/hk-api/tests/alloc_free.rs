//! The producer side of a binary stream allocates nothing per record in steady state
//! (docs/stream-contract.md §7), including the drop path and drop markers.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use hk_api::stream::{
    BinaryRecord, Publisher, PublisherConfig, RecordFlags, StreamHeader, StreamKind,
};
use hk_model::{ContentClass, Timestamp};

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

/// Blocks writes while the gate is closed, so its ring fills and drops happen.
struct Valve(Arc<(Mutex<bool>, Condvar)>);

impl Write for Valve {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let (m, cv) = &*self.0;
        let mut open = m.lock().unwrap();
        while !*open {
            open = cv.wait(open).unwrap();
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn set_valve(v: &Arc<(Mutex<bool>, Condvar)>, open: bool) {
    *v.0.lock().unwrap() = open;
    v.1.notify_all();
}

#[test]
fn binary_publish_allocates_nothing_in_steady_state() {
    let mut header = StreamHeader::new("iq", StreamKind::Iq, ContentClass::Unrestricted, "t");
    header.datatype = Some("ci8".into());
    header.sample_rate_hz = Some(2e6);
    header.max_frame_len = 4096;
    let config = PublisherConfig {
        queue_bytes: 64 * 1024,
        disconnect_after_drops: u64::MAX,
        disconnect_after: Duration::from_secs(3600),
    };
    let mut publisher = Publisher::new(header, config).unwrap();
    let handle = publisher.handle();
    handle
        .subscribe("sink", Box::new(std::io::sink()), Box::new(|_| {}))
        .unwrap();
    let valve = Arc::new((Mutex::new(false), Condvar::new()));
    let slow = handle
        .subscribe(
            "valve",
            Box::new(Valve(Arc::clone(&valve))),
            Box::new(|_| {}),
        )
        .unwrap();

    let payload = [0x5au8; 1024];
    let mut seq = 0u64;
    let mut publish_n = |publisher: &mut Publisher, n: u64| {
        for _ in 0..n {
            publisher
                .publish_binary(BinaryRecord {
                    t: Timestamp::from_unix_nanos(seq as i64),
                    sample_index: seq,
                    flags: RecordFlags::empty(),
                    payload: &payload,
                })
                .unwrap();
            seq += 1;
        }
    };

    // Warm-up: fill, drop, recover (marker), so every code path has run once.
    publish_n(&mut publisher, 2_000);
    set_valve(&valve, true);
    while handle.stats(slow).unwrap().queued_bytes > 0 {
        std::thread::sleep(Duration::from_millis(1));
    }
    publish_n(&mut publisher, 10);
    set_valve(&valve, false);
    std::thread::sleep(Duration::from_millis(20));
    let warm = handle.stats(slow).unwrap();

    COUNT_HERE.with(|c| c.set(true));
    publish_n(&mut publisher, 20_000);
    COUNT_HERE.with(|c| c.set(false));

    let after = handle.stats(slow).unwrap();
    assert!(
        after.records_dropped > warm.records_dropped,
        "the measured window exercised the drop path"
    );
    assert_eq!(
        ALLOCS.load(Ordering::Relaxed),
        0,
        "allocations on the producer thread"
    );

    set_valve(&valve, true);
    drop(publisher);
    assert!(handle.wait_closed(Duration::from_secs(5)));
}
