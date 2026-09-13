//! Lossless flow control for offline replay.
//!
//! The ring never waits for a reader (ADR-0001): a live source cannot be paused, so a slow reader
//! is lapped and its loss is counted. An **unpaced replay** can wait, and `hk replay` must be
//! deterministic and lossless however slowly a debug build runs. So in lossless mode every reader
//! (always-on readers, runtime chains, recorders) registers a [`GateCursor`] holding the oldest
//! stream index it still needs, and the capture thread holds each block back until the block
//! would stay within the ring's retained span behind the slowest cursor. With the gate disabled
//! (paced replay, live sources) the capture thread never consults it. `Pipeline::start` refuses
//! lossless mode for a source that cannot be paused ([`hk_core::Source::pausable`]).
//!
//! **Stream indices do not start at 0.** A recording's first sample carries its
//! `core:global_index` (e.g. 423 000 000), so a cursor registered at 0 before the first block,
//! or a claim below history the ring has already dropped, must not hold the writer: such claims
//! are clamped up to the ring's oldest retained sample (the block's own start on an empty ring)
//! in [`FlowGate::wait_for_block`]. Likewise a cursor that has read everything written needs
//! nothing the next block could overwrite, so a source gap (an index jump) or a block longer than
//! the slack never holds it.

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

    /// Whether a block `[start, end)` may be written now, given the slowest claim `min`, the
    /// ring's oldest retained sample and the end of the newest committed block.
    fn admits(
        &self,
        min: u64,
        start: u64,
        end: u64,
        ring_oldest: Option<u64>,
        ring_next: Option<u64>,
    ) -> bool {
        if min == u64::MAX {
            return true;
        }
        // An empty ring: nothing written can be overwritten, whatever the block's size or index.
        let Some(next) = ring_next else {
            return true;
        };
        // History already gone (or before the stream's first index) cannot be protected.
        let need = min.max(ring_oldest.unwrap_or(start));
        // Every reader has consumed everything written: nothing retained is still needed.
        if need >= next {
            return true;
        }
        end <= need.saturating_add(self.slack)
    }

    /// Capture side: waits until a block `[start, end)` keeps every cursor's needed samples in
    /// the ring, or `stop` is set. `ring_oldest` / `ring_next` are the ring's oldest retained
    /// sample and the end of its newest block (`None` on an empty ring). Returns at once when
    /// disabled.
    pub fn wait_for_block(
        &self,
        start: u64,
        end: u64,
        ring_oldest: Option<u64>,
        ring_next: Option<u64>,
        stop: &AtomicBool,
    ) {
        if !self.enabled {
            return;
        }
        let mut waited = false;
        loop {
            if self.admits(self.min_needed(), start, end, ring_oldest, ring_next) {
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

    /// Capture side, index-only form: waits until a block ending at `end` lies within the slack
    /// of the slowest cursor. Kept for callers that do not know the ring's state; it does not
    /// clamp claims below the stream start, so prefer [`Self::wait_for_block`].
    pub fn wait_for_room(&self, end: u64, stop: &AtomicBool) {
        // No clamp (oldest 0) and never "caught up" (next = MAX): `end <= min + slack`.
        self.wait_for_block(end, end, Some(0), Some(u64::MAX), stop);
    }

    /// Blocks that had to wait.
    pub fn waits(&self) -> u64 {
        self.waits.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    /// Runs a possibly blocking gate call on its own thread; a call still blocked after 10 s
    /// fails the test (it sets `stop` so the thread can exit) instead of hanging it.
    fn bounded(f: impl FnOnce(&AtomicBool) + Send + 'static) {
        let stop = Arc::new(AtomicBool::new(false));
        let s = Arc::clone(&stop);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            f(&s);
            let _ = tx.send(());
        });
        if rx.recv_timeout(Duration::from_secs(10)).is_err() {
            stop.store(true, Ordering::SeqCst);
            panic!("the gate blocked for 10 s");
        }
    }

    #[test]
    fn disabled_gate_never_waits_and_released_cursors_do_not_hold() {
        let off = Arc::new(FlowGate::new(false, 100));
        let _c = off.register(0);
        let g = Arc::clone(&off);
        bounded(move |stop| g.wait_for_room(10_000, stop));
        let on = Arc::new(FlowGate::new(true, 100));
        let c = on.register(0);
        assert_eq!(on.min_needed(), 0);
        drop(c);
        let g = Arc::clone(&on);
        bounded(move |stop| g.wait_for_room(10_000, stop));
        assert_eq!(on.waits(), 0);
    }

    #[test]
    fn gate_holds_the_writer_until_the_reader_advances() {
        let gate = Arc::new(FlowGate::new(true, 100));
        let cursor = gate.register(0);
        let stop = Arc::new(AtomicBool::new(false));
        let g = Arc::clone(&gate);
        let s = Arc::clone(&stop);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            g.wait_for_room(120, &s);
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(20)).is_err(),
            "the writer must wait for the reader"
        );
        cursor.set(80);
        let released = rx.recv_timeout(Duration::from_secs(10)).is_ok();
        stop.store(true, Ordering::SeqCst);
        assert!(
            released,
            "the writer stayed blocked after the reader advanced"
        );
        assert_eq!(gate.waits(), 1);
    }

    #[test]
    fn claims_below_the_stream_start_do_not_hold_a_large_global_index() {
        let big = 423_000_000u64;
        let gate = FlowGate::new(true, 100);
        let _reader = gate.register(0);
        // Empty ring: the first block starts far beyond 0 + slack.
        assert!(gate.admits(gate.min_needed(), big, big + 10, None, None));
        // The unread ring holds [big, big + 40): the writer may run to oldest + slack, no further.
        assert!(gate.admits(0, big + 40, big + 50, Some(big), Some(big + 40)));
        assert!(!gate.admits(0, big + 50, big + 60, Some(big), Some(big + 50)));
        // Once the reader has advanced, the writer may continue.
        assert!(gate.admits(big + 30, big + 50, big + 60, Some(big), Some(big + 50)));
    }

    #[test]
    fn caught_up_readers_pass_gaps_and_oversize_blocks() {
        let gate = FlowGate::new(true, 100);
        // An index jump of 10 000 after the reader consumed everything written.
        assert!(gate.admits(1_000, 11_000, 11_010, Some(950), Some(1_000)));
        // A lagging reader still holds it.
        assert!(!gate.admits(990, 11_000, 11_010, Some(950), Some(1_000)));
        // A block longer than the slack.
        assert!(gate.admits(1_000, 1_000, 1_080, Some(950), Some(1_000)));
    }

    #[test]
    fn wait_for_block_returns_for_a_huge_first_index() {
        let gate = Arc::new(FlowGate::new(true, 100));
        let a = gate.register(0);
        let b = gate.register(0);
        let start = 5_000_000_000u64;
        // First block on an empty ring (64 samples, more than the 50-sample slack).
        let g = Arc::clone(&gate);
        bounded(move |stop| g.wait_for_block(start, start + 64, None, None, stop));
        assert_eq!(gate.waits(), 0);
        // The ring now holds [start, start + 64) unread; the readers' claims of 0 clamp to
        // `start`, so the next block is held only by the slack (half a ring of pre-trigger
        // reserve), never by the claims' distance from 0.
        let (s0, s1) = (start + 64, start + 90);
        assert!(!gate.admits(gate.min_needed(), s0, s1, Some(start), Some(s0)));
        a.set(start + 40);
        // The slowest reader still claims 0 (clamped to `start`): held.
        assert!(!gate.admits(gate.min_needed(), s0, s1, Some(start), Some(s0)));
        b.set(start + 40);
        assert!(gate.admits(gate.min_needed(), s0, s1, Some(start), Some(s0)));
        b.set(s0);
        a.set(s0);
        let g = Arc::clone(&gate);
        bounded(move |stop| g.wait_for_block(s0, s1, Some(start), Some(s0), stop));
        drop((a, b));
    }
}
