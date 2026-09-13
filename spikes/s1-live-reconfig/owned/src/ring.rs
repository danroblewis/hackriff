//! Single-writer, multi-reader block ring ("broadcast ring").
//!
//! The capture thread publishes fixed-size sample blocks, each wrapped in an
//! `Arc`, into a circular array of `capacity` slots. The array *is* the RAM
//! history ring (C03): a reader is nothing more than a cursor (a block
//! sequence number), so attaching a chain = creating a cursor at `head`
//! (live) or at `head - k` (pre-trigger history). No registration with the
//! writer is needed and the writer never waits on readers.
//!
//! * Writer never blocks: a reader that falls more than `capacity` blocks
//!   behind loses the oldest blocks and is told exactly how many (`Lagged`).
//! * Readers never see a torn block: they hold an `Arc` to an immutable
//!   block; the writer only ever replaces the `Arc` in a slot.
//! * Steady state allocates nothing: the block evicted from the slot is
//!   reused for the next publish when no reader still holds it.
//! * All safe Rust; per-slot `Mutex` critical sections are one `Arc` clone.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// One block of samples with its position on the source's sample clock.
#[derive(Debug, Default)]
pub struct Block<T> {
    /// Block sequence number (index into the ring's logical stream).
    pub seq: u64,
    /// Source sample index of the first sample in `data`.
    pub first_sample: u64,
    /// Number of complex samples in this block.
    pub n_samples: usize,
    /// Samples dropped by the source immediately before this block
    /// (discontinuity flag; 0 = contiguous with previous block).
    pub dropped_before: u64,
    pub data: Vec<T>,
}

pub struct Ring<T> {
    slots: Box<[Mutex<Option<Arc<Block<T>>>>]>,
    /// Sequence number of the next block to be published.
    head: AtomicU64,
    wake_lock: Mutex<()>,
    wake: Condvar,
    closed: AtomicBool,
}

impl<T: Default + Send + Sync + 'static> Ring<T> {
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            slots: (0..capacity).map(|_| Mutex::new(None)).collect(),
            head: AtomicU64::new(0),
            wake_lock: Mutex::new(()),
            wake: Condvar::new(),
            closed: AtomicBool::new(false),
        })
    }

    pub fn capacity(&self) -> u64 {
        self.slots.len() as u64
    }

    pub fn head(&self) -> u64 {
        self.head.load(Ordering::Acquire)
    }

    /// Only one writer may exist (enforced by `Writer` not being `Clone`).
    pub fn writer(self: &Arc<Self>) -> Writer<T> {
        Writer { ring: self.clone(), spare: None, allocations: 0 }
    }

    /// Reader starting at the next published block (live attach).
    pub fn reader(self: &Arc<Self>) -> Reader<T> {
        self.reader_at(self.head())
    }

    /// Reader starting `k` blocks in the past (pre-trigger attach), clamped
    /// to the oldest block still in the ring.
    pub fn reader_with_history(self: &Arc<Self>, k: u64) -> Reader<T> {
        let h = self.head();
        let oldest = h.saturating_sub(self.capacity());
        self.reader_at(h.saturating_sub(k).max(oldest))
    }

    pub fn reader_at(self: &Arc<Self>, seq: u64) -> Reader<T> {
        Reader { ring: self.clone(), cursor: seq, lost_blocks: 0 }
    }

    /// Wake all waiting readers (used on detach so a reader sees its stop flag).
    pub fn wake_all(&self) {
        let _g = self.wake_lock.lock().unwrap();
        self.wake.notify_all();
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.wake_all();
    }
}

pub struct Writer<T> {
    ring: Arc<Ring<T>>,
    spare: Option<Arc<Block<T>>>,
    /// Blocks allocated because no recyclable block was available.
    pub allocations: u64,
}

impl<T: Default + Send + Sync + 'static> Writer<T> {
    /// Fill and publish the next block. `fill` gets exclusive access to a
    /// (possibly recycled) block; `seq` is set by the ring.
    pub fn publish(&mut self, fill: impl FnOnce(&mut Block<T>)) -> u64 {
        let ring = &*self.ring;
        let seq = ring.head.load(Ordering::Acquire);
        let mut blk = match self.spare.take() {
            Some(b) => b,
            None => {
                self.allocations += 1;
                Arc::new(Block::default())
            }
        };
        {
            let b = Arc::get_mut(&mut blk).expect("spare block is uniquely owned");
            fill(b);
            b.seq = seq;
        }
        let slot = &ring.slots[(seq % ring.capacity()) as usize];
        let evicted = slot.lock().unwrap().replace(blk);
        ring.head.store(seq + 1, Ordering::Release);
        {
            let _g = ring.wake_lock.lock().unwrap();
            ring.wake.notify_all();
        }
        // Recycle the evicted block if no reader still holds it.
        if let Some(mut old) = evicted {
            if Arc::get_mut(&mut old).is_some() {
                self.spare = Some(old);
            }
        }
        seq
    }
}

pub enum Next<T> {
    Block(Arc<Block<T>>),
    /// Reader fell behind; this many blocks were lost (cursor advanced).
    Lagged(u64),
    /// Timed out or stop requested.
    Idle,
    Closed,
}

pub struct Reader<T> {
    ring: Arc<Ring<T>>,
    cursor: u64,
    pub lost_blocks: u64,
}

impl<T: Default + Send + Sync + 'static> Reader<T> {
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Next block, waiting up to `timeout`; returns `Idle` early if `stop` is set.
    pub fn next(&mut self, timeout: Duration, stop: &AtomicBool) -> Next<T> {
        loop {
            let head = self.ring.head.load(Ordering::Acquire);
            if self.cursor < head {
                let oldest = head.saturating_sub(self.ring.capacity());
                if self.cursor < oldest {
                    let lost = oldest - self.cursor;
                    self.cursor = oldest;
                    self.lost_blocks += lost;
                    return Next::Lagged(lost);
                }
                let slot = &self.ring.slots[(self.cursor % self.ring.capacity()) as usize];
                let got = slot.lock().unwrap().clone();
                match got {
                    Some(b) if b.seq == self.cursor => {
                        self.cursor += 1;
                        return Next::Block(b);
                    }
                    // Overwritten between reading head and locking the slot:
                    // loop, and the lag check above accounts for it.
                    _ => continue,
                }
            }
            if stop.load(Ordering::Acquire) {
                return Next::Idle;
            }
            if self.ring.closed.load(Ordering::Acquire) {
                return Next::Closed;
            }
            let g = self.ring.wake_lock.lock().unwrap();
            if self.ring.head.load(Ordering::Acquire) > self.cursor || stop.load(Ordering::Acquire) {
                continue;
            }
            let (_g, res) = self.ring.wake.wait_timeout(g, timeout).unwrap();
            if res.timed_out() {
                return Next::Idle;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readers_are_independent_and_lag_is_counted() {
        let ring = Ring::<u8>::new(4);
        let mut w = ring.writer();
        let stop = AtomicBool::new(false);
        let mut fast = ring.reader();
        let mut slow = ring.reader();
        for i in 0..6u64 {
            w.publish(|b| b.first_sample = i);
            match fast.next(Duration::from_millis(1), &stop) {
                Next::Block(b) => assert_eq!(b.first_sample, i),
                _ => panic!("fast reader should get every block"),
            }
        }
        // slow reader is 6 behind with capacity 4: loses 2
        assert!(matches!(slow.next(Duration::from_millis(1), &stop), Next::Lagged(2)));
        match slow.next(Duration::from_millis(1), &stop) {
            Next::Block(b) => assert_eq!(b.first_sample, 2),
            _ => panic!(),
        }
        let mut hist = ring.reader_with_history(2);
        match hist.next(Duration::from_millis(1), &stop) {
            Next::Block(b) => assert_eq!(b.first_sample, 4),
            _ => panic!(),
        }
    }
}
