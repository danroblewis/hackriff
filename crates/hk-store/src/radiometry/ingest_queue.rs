//! Non-blocking ingest into a shared [`FloorProduct`] (T-037b).
//!
//! In live mode the product sits behind a `Mutex` that `/api/history` and `/api/floor` hold for a
//! whole query (tile reads from disk included), while the history reader folds about ten frames a
//! second into it. [`FloorIngestQueue`] splits the writer from those readers: [`ingest`] only
//! *tries* the lock. While a query holds it, the frame pair is queued, bounded by the capacity
//! (the oldest pairs are dropped and counted beyond it), and folded in arrival order by the next
//! call that gets the lock. The reader thread never waits for a query, and a query waits at most
//! for one fold. [`FloorIngestQueue::drain`] folds what is left under a lock the caller already
//! holds (end of stream, before sealing).
//!
//! [`ingest`]: FloorIngestQueue::ingest

use std::collections::VecDeque;
use std::sync::{Mutex, TryLockError};

use hk_dsp::SpectrumFrame;
use hk_dsp::floor::FloorFrame;

use super::product::{FloorIngest, FloorProduct};
use crate::history::{FrameOrigin, StoreError};

/// Counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IngestQueueStats {
    /// Calls that found the product locked (the frame was queued).
    pub deferred: u64,
    /// Queued frames dropped beyond the capacity, oldest first.
    pub dropped: u64,
    /// Queued frames folded later.
    pub folded_late: u64,
}

/// What one [`FloorIngestQueue::ingest`] call did.
#[derive(Debug)]
pub struct QueuedIngest {
    /// Every fold it ran, in stream order (earlier queued frames first); empty when deferred.
    pub folded: Vec<Result<FloorIngest, StoreError>>,
    /// The product was locked: the frame was queued.
    pub deferred: bool,
    /// Queued frames this call dropped beyond the capacity.
    pub dropped: u64,
}

/// Folds frames into a `Mutex<FloorProduct>` without waiting for its readers (module docs).
#[derive(Debug)]
pub struct FloorIngestQueue {
    pending: VecDeque<(SpectrumFrame, FloorFrame, FrameOrigin)>,
    capacity: usize,
    stats: IngestQueueStats,
}

impl FloorIngestQueue {
    /// A queue keeping at most `capacity` (at least 1) deferred frame pairs.
    pub fn new(capacity: usize) -> Self {
        Self {
            pending: VecDeque::new(),
            capacity: capacity.max(1),
            stats: IngestQueueStats::default(),
        }
    }

    /// Folds `spectrum` + `floor` (and anything queued before them) if the product's lock is free
    /// now; otherwise queues the pair and returns at once.
    pub fn ingest(
        &mut self,
        product: &Mutex<FloorProduct>,
        spectrum: &SpectrumFrame,
        floor: &FloorFrame,
    ) -> QueuedIngest {
        self.ingest_from(product, spectrum, floor, FrameOrigin::default())
    }

    /// [`Self::ingest`] with the frame's source and site (T-133), kept with a deferred frame so
    /// it is recorded as of the time the frame was taken.
    pub fn ingest_from(
        &mut self,
        product: &Mutex<FloorProduct>,
        spectrum: &SpectrumFrame,
        floor: &FloorFrame,
        origin: FrameOrigin,
    ) -> QueuedIngest {
        let mut guard = match product.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(e)) => e.into_inner(),
            Err(TryLockError::WouldBlock) => {
                self.stats.deferred += 1;
                self.pending
                    .push_back((spectrum.clone(), floor.clone(), origin));
                let mut dropped = 0;
                while self.pending.len() > self.capacity {
                    self.pending.pop_front();
                    dropped += 1;
                }
                self.stats.dropped += dropped;
                return QueuedIngest {
                    folded: Vec::new(),
                    deferred: true,
                    dropped,
                };
            }
        };
        let mut folded = self.drain(&mut guard);
        folded.push(guard.ingest_from(spectrum, floor, origin));
        QueuedIngest {
            folded,
            deferred: false,
            dropped: 0,
        }
    }

    /// Folds every queued pair into `product` (whose lock the caller holds), in order.
    pub fn drain(&mut self, product: &mut FloorProduct) -> Vec<Result<FloorIngest, StoreError>> {
        let mut out = Vec::with_capacity(self.pending.len() + 1);
        for (s, f, o) in self.pending.drain(..) {
            self.stats.folded_late += 1;
            out.push(product.ingest_from(&s, &f, o));
        }
        out
    }

    /// Queued pairs.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Counters.
    pub fn stats(&self) -> IngestQueueStats {
        self.stats
    }
}
