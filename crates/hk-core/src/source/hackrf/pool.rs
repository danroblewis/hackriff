//! Transfer hand-over from the libusb callback thread to the capture thread, with no allocation
//! and no blocking lock after construction.
//!
//! - A fixed pool of transfer-sized byte buffers and two single-producer/single-consumer index
//!   queues: `free` (filled by the consumer, emptied by the callback) and `filled` (the reverse).
//!   Every buffer index sits in exactly one queue or is held by exactly one side, which is what
//!   makes the unsynchronised buffer access sound.
//! - The callback copies each transfer into a free buffer and stamps it with a sequence number,
//!   its byte offset in the stream (dropped transfers included, so the sample counter keeps
//!   counting) and its arrival time. With no free buffer the transfer is dropped and counted.
//! - The consumer takes the oldest filled buffer ([`TransferPool::take`]); dropping the
//!   [`Taken`] guard recycles it. [`TransferPool::wait`] parks the consumer until the next
//!   transfer (the callback unparks it with a `try_lock`, so it never blocks on the waiter slot).

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError, TryLockError};
use std::thread::Thread;
use std::time::Duration;

/// Counters shared by the callback, the stream and observers.
#[derive(Debug, Default)]
pub struct TransferCounters {
    /// Transfers delivered by the driver (callbacks).
    pub transfers: AtomicU64,
    /// Transfers dropped because no free buffer was available (the capture thread fell behind).
    pub dropped_transfers: AtomicU64,
    /// Samples in dropped transfers.
    pub dropped_samples: AtomicU64,
    /// Transfers longer than a pool buffer (their tail is dropped and counted as a gap).
    pub truncated_transfers: AtomicU64,
    /// Deepest `filled` queue seen.
    pub max_queue_depth: AtomicU64,
}

/// A bounded single-producer/single-consumer queue of buffer indices.
struct IndexQueue {
    slots: Box<[AtomicU32]>,
    head: AtomicUsize,
    tail: AtomicUsize,
}

impl IndexQueue {
    fn new(capacity: usize) -> Self {
        Self {
            slots: (0..capacity + 1).map(|_| AtomicU32::new(0)).collect(),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    /// Producer side. `false` when full.
    fn push(&self, value: u32) -> bool {
        let tail = self.tail.load(Ordering::SeqCst);
        let next = (tail + 1) % self.slots.len();
        if next == self.head.load(Ordering::SeqCst) {
            return false;
        }
        self.slots[tail].store(value, Ordering::SeqCst);
        self.tail.store(next, Ordering::SeqCst);
        true
    }

    /// Consumer side.
    fn pop(&self) -> Option<u32> {
        let head = self.head.load(Ordering::SeqCst);
        if head == self.tail.load(Ordering::SeqCst) {
            return None;
        }
        let value = self.slots[head].load(Ordering::SeqCst);
        self.head
            .store((head + 1) % self.slots.len(), Ordering::SeqCst);
        Some(value)
    }

    fn len(&self) -> usize {
        let n = self.slots.len();
        (self.tail.load(Ordering::SeqCst) + n - self.head.load(Ordering::SeqCst)) % n
    }
}

struct Slot {
    bytes: UnsafeCell<Box<[u8]>>,
    seq: AtomicU64,
    offset: AtomicU64,
    len: AtomicUsize,
    arrival_ns: AtomicI64,
}

/// Where a filled buffer came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransferMeta {
    /// Callback sequence number (0-based, dropped transfers included).
    pub seq: u64,
    /// Byte offset of the first byte in the stream (dropped transfers included).
    pub byte_offset: u64,
    /// Valid bytes.
    pub len: usize,
    /// Host time when the transfer arrived, Unix ns.
    pub arrival_ns: i64,
}

/// The buffer pool (see the [module docs](self)).
pub struct TransferPool {
    slots: Box<[Slot]>,
    free: IndexQueue,
    filled: IndexQueue,
    next_seq: AtomicU64,
    next_offset: AtomicU64,
    last_arrival_ns: AtomicI64,
    counters: std::sync::Arc<TransferCounters>,
    waiter: Mutex<Option<Thread>>,
}

// SAFETY: a slot's `bytes` are only touched by the side that holds its index (popped from `free`
// by the single callback thread until pushed to `filled`; popped from `filled` by the single
// consumer until pushed back to `free`). Everything else is atomics or a mutex.
unsafe impl Sync for TransferPool {}
// SAFETY: as above; the pool owns its buffers.
unsafe impl Send for TransferPool {}

impl TransferPool {
    /// `buffers` buffers of `buffer_bytes` each (both at least 1).
    pub fn new(
        buffers: usize,
        buffer_bytes: usize,
        counters: std::sync::Arc<TransferCounters>,
    ) -> Self {
        let buffers = buffers.clamp(1, u32::MAX as usize);
        let pool = Self {
            slots: (0..buffers)
                .map(|_| Slot {
                    bytes: UnsafeCell::new(vec![0u8; buffer_bytes.max(2)].into_boxed_slice()),
                    seq: AtomicU64::new(0),
                    offset: AtomicU64::new(0),
                    len: AtomicUsize::new(0),
                    arrival_ns: AtomicI64::new(0),
                })
                .collect(),
            free: IndexQueue::new(buffers),
            filled: IndexQueue::new(buffers),
            next_seq: AtomicU64::new(0),
            next_offset: AtomicU64::new(0),
            last_arrival_ns: AtomicI64::new(0),
            counters,
            waiter: Mutex::new(None),
        };
        for i in 0..buffers {
            let pushed = pool.free.push(i as u32);
            debug_assert!(pushed);
        }
        pool
    }

    /// Callbacks seen so far: the sequence number the next transfer will get.
    pub fn next_seq(&self) -> u64 {
        self.next_seq.load(Ordering::SeqCst)
    }

    /// Arrival time of the newest transfer, Unix ns (0 before the first).
    pub fn last_arrival_ns(&self) -> i64 {
        self.last_arrival_ns.load(Ordering::SeqCst)
    }

    /// Buffers waiting for the consumer.
    pub fn queued(&self) -> usize {
        self.filled.len()
    }

    /// **Callback side** (one producer thread): hands one transfer over, or drops and counts it.
    /// No allocation, no blocking.
    pub fn on_transfer(&self, data: &[u8], arrival_ns: i64) {
        let c = &self.counters;
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let offset = self
            .next_offset
            .fetch_add(data.len() as u64, Ordering::SeqCst);
        self.last_arrival_ns.store(arrival_ns, Ordering::SeqCst);
        c.transfers.fetch_add(1, Ordering::Relaxed);
        let Some(i) = self.free.pop() else {
            c.dropped_transfers.fetch_add(1, Ordering::Relaxed);
            c.dropped_samples
                .fetch_add(data.len() as u64 / 2, Ordering::Relaxed);
            return;
        };
        let slot = &self.slots[i as usize];
        // SAFETY: index `i` was just popped from `free`, so this thread holds it exclusively.
        let buf = unsafe { &mut *slot.bytes.get() };
        let mut n = data.len().min(buf.len());
        n &= !1;
        if n < data.len() {
            c.truncated_transfers.fetch_add(1, Ordering::Relaxed);
        }
        buf[..n].copy_from_slice(&data[..n]);
        slot.seq.store(seq, Ordering::SeqCst);
        slot.offset.store(offset, Ordering::SeqCst);
        slot.len.store(n, Ordering::SeqCst);
        slot.arrival_ns.store(arrival_ns, Ordering::SeqCst);
        let pushed = self.filled.push(i);
        debug_assert!(pushed, "filled has room for every buffer");
        c.max_queue_depth
            .fetch_max(self.filled.len() as u64, Ordering::Relaxed);
        if let Ok(waiter) = self.waiter.try_lock() {
            if let Some(t) = waiter.as_ref() {
                t.unpark();
            }
        }
    }

    /// **Consumer side** (one thread): the oldest filled buffer, recycled when the guard drops.
    pub fn take(&self) -> Option<Taken<'_>> {
        let index = self.filled.pop()?;
        let slot = &self.slots[index as usize];
        Some(Taken {
            pool: self,
            index,
            meta: TransferMeta {
                seq: slot.seq.load(Ordering::SeqCst),
                byte_offset: slot.offset.load(Ordering::SeqCst),
                len: slot.len.load(Ordering::SeqCst),
                arrival_ns: slot.arrival_ns.load(Ordering::SeqCst),
            },
        })
    }

    /// **Consumer side**: parks until a transfer arrives or `timeout` passes.
    pub fn wait(&self, timeout: Duration) {
        {
            let mut waiter = match self.waiter.try_lock() {
                Ok(w) => w,
                Err(TryLockError::Poisoned(p)) => p.into_inner(),
                Err(TryLockError::WouldBlock) => {
                    self.waiter.lock().unwrap_or_else(PoisonError::into_inner)
                }
            };
            let me = std::thread::current();
            if waiter.as_ref().is_none_or(|t| t.id() != me.id()) {
                *waiter = Some(me);
            }
        }
        if self.filled.len() == 0 {
            std::thread::park_timeout(timeout);
        }
    }
}

/// A filled buffer held by the consumer; recycled on drop.
pub struct Taken<'a> {
    pool: &'a TransferPool,
    index: u32,
    /// Where the buffer came from.
    pub meta: TransferMeta,
}

impl Taken<'_> {
    /// The transfer's valid bytes (interleaved signed 8-bit I/Q).
    pub fn bytes(&self) -> &[u8] {
        let slot = &self.pool.slots[self.index as usize];
        // SAFETY: the consumer holds this index (popped from `filled`, not yet back in `free`),
        // so the callback cannot be writing it.
        let buf = unsafe { &*slot.bytes.get() };
        &buf[..self.meta.len]
    }
}

impl Drop for Taken<'_> {
    fn drop(&mut self) {
        let pushed = self.pool.free.push(self.index);
        debug_assert!(pushed, "free has room for every buffer");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn transfers_flow_in_order_and_overflow_is_counted() {
        let counters = Arc::new(TransferCounters::default());
        let pool = TransferPool::new(2, 8, Arc::clone(&counters));
        for k in 0..3u8 {
            pool.on_transfer(&[k; 8], i64::from(k));
        }
        assert_eq!(counters.dropped_transfers.load(Ordering::Relaxed), 1);
        assert_eq!(counters.dropped_samples.load(Ordering::Relaxed), 4);
        let a = pool.take().unwrap();
        assert_eq!(
            (a.meta.seq, a.meta.byte_offset, a.bytes()),
            (0, 0, &[0u8; 8][..])
        );
        drop(a);
        let b = pool.take().unwrap();
        assert_eq!((b.meta.seq, b.meta.byte_offset), (1, 8));
        drop(b);
        assert!(pool.take().is_none());
        // The dropped transfer still advanced the stream offset.
        pool.on_transfer(&[9; 10], 9);
        let c = pool.take().unwrap();
        assert_eq!((c.meta.seq, c.meta.byte_offset, c.meta.len), (3, 24, 8));
        assert_eq!(counters.truncated_transfers.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_producer_thread_and_a_consumer_thread_lose_nothing_the_pool_accepted() {
        let counters = Arc::new(TransferCounters::default());
        let pool = Arc::new(TransferPool::new(8, 64, Arc::clone(&counters)));
        let producer = {
            let pool = Arc::clone(&pool);
            std::thread::spawn(move || {
                for k in 0..20_000u32 {
                    let mut data = [0u8; 64];
                    data[..4].copy_from_slice(&k.to_le_bytes());
                    pool.on_transfer(&data, i64::from(k));
                }
            })
        };
        let mut seen = 0u64;
        let mut last = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            match pool.take() {
                Some(t) => {
                    let k = u32::from_le_bytes(t.bytes()[..4].try_into().unwrap());
                    assert_eq!(u64::from(k), t.meta.seq);
                    assert_eq!(t.meta.byte_offset, 64 * t.meta.seq);
                    assert!(last.is_none_or(|l| k > l), "in order");
                    last = Some(k);
                    seen += 1;
                }
                None if producer.is_finished() && pool.queued() == 0 => break,
                None => pool.wait(Duration::from_millis(1)),
            }
            assert!(std::time::Instant::now() < deadline, "watchdog");
        }
        producer.join().unwrap();
        let dropped = counters.dropped_transfers.load(Ordering::Relaxed);
        assert_eq!(seen + dropped, 20_000);
    }
}
