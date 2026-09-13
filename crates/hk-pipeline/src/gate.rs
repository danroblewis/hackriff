//! Lossless flow control for offline replay.
//!
//! The ring never waits for a reader (ADR-0001): a live source cannot be paused, so a slow reader
//! is lapped and its loss is counted. An **unpaced replay** can wait, and `hk replay` must be
//! deterministic and lossless however slowly a debug build runs. So in lossless mode every reader
//! (always-on readers, runtime chains, recorders) registers a [`GateCursor`] holding the oldest
//! stream index it still needs, and the capture thread holds each block back until the block
//! would stay within the ring's retained span behind the slowest cursor. With the gate disabled
//! (paced replay, live sources) the capture thread never consults it.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// A reader's claim: the oldest stream index it still needs (`u64::MAX` = released).
#[derive(Debug)]
pub struct GateCursor(Arc<AtomicU64>);

impl GateCursor {
    /// Records the oldest index still needed.
    pub fn set(&self, sample: u64) {
        self.0.store(sample, Ordering::SeqCst);
    }

    /// The claimed index.
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

impl Drop for GateCursor {
    fn drop(&mut self) {
        self.0.store(u64::MAX, Ordering::SeqCst);
    }
}

/// Backpressure between the capture thread and the ring's readers (see the module docs).
#[derive(Debug)]
pub struct FlowGate {
    enabled: bool,
    /// Samples a block may extend past the slowest cursor: half the ring, so a chain can still
    /// attach with up to half a ring of pre-trigger history behind the slowest reader.
    slack: u64,
    cursors: Mutex<Vec<Arc<AtomicU64>>>,
    waits: AtomicU64,
}

impl FlowGate {
    /// A gate for a ring of `capacity` samples; `enabled = false` makes every call a no-op.
    pub fn new(enabled: bool, capacity: usize) -> Self {
        Self {
            enabled,
            slack: (capacity as u64 / 2).max(1),
            cursors: Mutex::new(Vec::new()),
            waits: AtomicU64::new(0),
        }
    }

    /// Lossless mode.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Registers a cursor needing samples from `from` on.
    pub fn register(&self, from: u64) -> GateCursor {
        let cell = Arc::new(AtomicU64::new(from));
        if self.enabled {
            let mut c = self.cursors.lock().unwrap_or_else(PoisonError::into_inner);
            c.retain(|x| x.load(Ordering::SeqCst) != u64::MAX);
            c.push(Arc::clone(&cell));
        }
        GateCursor(cell)
    }

    /// The oldest index any registered cursor needs.
    pub fn min_needed(&self) -> u64 {
        let c = self.cursors.lock().unwrap_or_else(PoisonError::into_inner);
        c.iter()
            .map(|x| x.load(Ordering::SeqCst))
            .min()
            .unwrap_or(u64::MAX)
    }

    /// Capture side: waits until a block ending at `end` keeps every cursor's needed samples in
    /// the ring, or `stop` is set. Returns at once when disabled.
    pub fn wait_for_room(&self, end: u64, stop: &AtomicBool) {
        if !self.enabled {
            return;
        }
        let mut waited = false;
        loop {
            let min = self.min_needed();
            if min == u64::MAX || end <= min.saturating_add(self.slack) {
                break;
            }
            if stop.load(Ordering::SeqCst) {
                break;
            }
            if !waited {
                waited = true;
                self.waits.fetch_add(1, Ordering::Relaxed);
            }
            std::thread::sleep(Duration::from_micros(200));
        }
    }

    /// Blocks that had to wait.
    pub fn waits(&self) -> u64 {
        self.waits.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_gate_never_waits_and_released_cursors_do_not_hold() {
        let stop = AtomicBool::new(false);
        let off = FlowGate::new(false, 100);
        let _c = off.register(0);
        off.wait_for_room(10_000, &stop);
        let on = FlowGate::new(true, 100);
        let c = on.register(0);
        assert_eq!(on.min_needed(), 0);
        drop(c);
        on.wait_for_room(10_000, &stop);
        assert_eq!(on.waits(), 0);
    }

    #[test]
    fn gate_holds_the_writer_until_the_reader_advances() {
        let gate = Arc::new(FlowGate::new(true, 100));
        let cursor = gate.register(0);
        let stop = Arc::new(AtomicBool::new(false));
        let g = Arc::clone(&gate);
        let s = Arc::clone(&stop);
        let writer = std::thread::spawn(move || g.wait_for_room(120, &s));
        std::thread::sleep(Duration::from_millis(20));
        assert!(!writer.is_finished());
        cursor.set(80);
        writer.join().unwrap();
        assert_eq!(gate.waits(), 1);
    }
}
