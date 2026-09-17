//! RAM ring buffer (C03, ADR-0001 always-on core, ADR-0006 pre-trigger): one writer, any number
//! of independent readers attached and detached at runtime, and pre-trigger extraction by
//! sample index.
//!
//! # Design
//!
//! - **Sample-indexed.** Position `i` in the ring *is* stream sample index `i` (the source's
//!   monotonic counter), stored at physical slot `i mod capacity`. Capacities are used exactly as
//!   configured (no power-of-two rounding). A source gap is an index jump: nothing is written for
//!   missing samples and the block metadata records the gap, so a pre-trigger window
//!   `[t − pre, t + post)` maps straight onto slots and a gap is never spliced.
//! - **Block metadata ring.** A second, smaller ring holds one record per block: first sample,
//!   length, host time, discontinuity flags, the source gap before the block, the cumulative gap
//!   count since the stream start, and the block's provenance (key and table slot). A reader
//!   reports metadata changes exactly at block boundaries.
//! - **Concurrency: a seqlock by sample index.** For each block the writer
//!   1. announces the record write (`meta_write_end = block + 1`), then publishes provenance and
//!      stores the record;
//!   2. announces the sample write (`sample_write_end = first + n`), then stores the samples;
//!   3. commits (`sample_commit_end`, then `meta_head`).
//!
//!   A reader copies `[a, b)` only if `b <= sample_commit_end` (loaded *before* the copy), then
//!   re-loads `sample_write_end`: the copy is valid iff `a >= sample_write_end − capacity`, since
//!   any store that could have overwritten a copied slot was announced first. A record for block
//!   `k` is read only if `k < meta_head` (loaded first) and is valid iff
//!   `k >= meta_write_end − block_capacity` afterwards. The writer never waits for a reader, and
//!   readers need no registration: attach = create a cursor, detach = drop it.
//!
//!   Two counters loaded one after the other are never a snapshot. The seqlock checks above are
//!   safe anyway because each counter is used only in its conservative direction (commit bound
//!   loaded before the copy, invalidation bound after). A decision that needs both at once, as
//!   when `locate` bounds its search by `meta_head` and `meta_write_end`, takes a consistent pair
//!   from `Shared::meta_counters` (load both, reload both, accept only if unchanged).
//!
//!   Sample cells are atomics (`AtomicU64` holds the two `f32` bit patterns of a `Complex32`), so
//!   a racing copy is merely stale, never undefined behaviour, and no `unsafe` is needed.
//!
//! # Why every shared operation is `SeqCst`
//!
//! The argument above needs *sequential consistency*: a reader that observes a slot store must
//! also observe every store the writer made before it. Rust promises that only for
//! `Ordering::SeqCst`. The first version stored samples and records with `Relaxed`, which on
//! Apple Silicon compiles to plain `ldr`/`str` whose effects other cores may observe out of
//! program order (the CPU does not provide total store order natively), while `SeqCst` compiles
//! to ordered instructions. Readers then validated fresh slot contents against a stale
//! `write_end` and accepted overwritten samples and records of unwritten blocks, in release
//! builds only. With every operation `SeqCst` the proof rests on the language guarantee alone;
//! the cost is within noise (`examples/ring_throughput.rs`).
//!
//! This was verified empirically, not just argued: on the dev Mac (M3 Ultra, rustc 1.93.1 /
//! LLVM 21) a two-variable probe using the weaker orderings saw about 5 million linearisability
//! violations and the same probe with `SeqCst` saw none, matching the stale reads the ring
//! stress test caught. `tests/ring_atomic_ordering.rs` therefore fails the build on any
//! non-`SeqCst` ordering in `src/ring/` (exemption: an `// ordering-exempt: <reason>` comment
//! on the same line). Re-verify on the Jetson (aarch64 Linux) before relaxing anything.
//!
//! # Loss accounting
//!
//! Every stream index between a reader's start and its position is accounted exactly once as
//! read, lost (overwritten before it was read: [`ReadOutcome::Overrun`]) or a source gap
//! (reported as a chunk's `dropped_before` or an overrun's `gap_samples`). The cursor moves only
//! after a successful copy, so a failed copy never hides loss.
//!
//! # Provenance
//!
//! Provenance handles live in a fixed table (`2 × block_capacity + 8` slots, allocated and
//! initialised up front). On a provenance change the writer takes a slot that no retained block
//! references, using `try_lock` only: a slot a reader happens to hold is skipped, so the writer
//! never parks and never waits behind a lower-priority reader. A slot is reused only after the
//! record announce has invalidated every block that referenced it, so a reader that finds a
//! different key in a slot knows its record is stale. Readers cache the handle they last used.
//!
//! # Waking readers
//!
//! The writer notifies a condvar only if a reader is parked, and uses `try_lock`, so it cannot
//! block. The rare missed wake-up is bounded by the reader's 2 ms wait slice.
//!
//! Sample types implement [`RingSample`]: `Complex32` (8 bytes/sample; 160 MB/s at 20 Msps) and
//! `Complex<i8>` (2 bytes/sample; 40 MB/s, the C03 recommendation for long pre-trigger rings on
//! the 8 GB Jetson).

pub mod extract;

use std::collections::VecDeque;
use std::sync::atomic::{
    AtomicBool, AtomicI64, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering,
};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::{Duration, Instant};

use hk_model::{SampleTime, Timestamp};
use num_complex::{Complex, Complex32};

use crate::block::{BlockHeader, Discontinuity, ProvenanceHandle, SampleBlock};
use crate::rt::MemoryLock;

pub use extract::{
    CaptureError, CaptureSegment, CaptureStatus, CapturedWindow, PreTriggerCapture, TriggerRead,
    TriggerStream, TriggerWindow,
};

/// The ordering for every operation on shared ring state (see the module docs).
const SEQ: Ordering = Ordering::SeqCst;
/// Longest single condvar wait; bounds the latency of a wake-up missed by the writer's `try_lock`.
const WAIT_SLICE: Duration = Duration::from_millis(2);

/// A sample type storable in the ring: each sample lives in one atomic cell.
pub trait RingSample: Copy + Default + Send + Sync + 'static {
    /// The atomic cell type.
    type Cell: Send + Sync;
    /// A zeroed cell.
    fn new_cell() -> Self::Cell;
    /// Reads a sample (sequentially consistent).
    fn load(cell: &Self::Cell) -> Self;
    /// Writes a sample (sequentially consistent).
    fn store(cell: &Self::Cell, value: Self);
}

impl RingSample for Complex32 {
    type Cell = AtomicU64;

    fn new_cell() -> AtomicU64 {
        AtomicU64::new(0)
    }

    #[inline]
    fn load(cell: &AtomicU64) -> Self {
        let bits = cell.load(SEQ);
        Complex32::new(
            f32::from_bits((bits >> 32) as u32),
            f32::from_bits(bits as u32),
        )
    }

    #[inline]
    fn store(cell: &AtomicU64, value: Self) {
        cell.store(
            (u64::from(value.re.to_bits()) << 32) | u64::from(value.im.to_bits()),
            SEQ,
        );
    }
}

impl RingSample for Complex<i8> {
    type Cell = AtomicU16;

    fn new_cell() -> AtomicU16 {
        AtomicU16::new(0)
    }

    #[inline]
    fn load(cell: &AtomicU16) -> Self {
        let bits = cell.load(SEQ);
        Complex::new((bits >> 8) as u8 as i8, bits as u8 as i8)
    }

    #[inline]
    fn store(cell: &AtomicU16, value: Self) {
        cell.store(
            (u16::from(value.re as u8) << 8) | u16::from(value.im as u8),
            SEQ,
        );
    }
}

/// Ring sizes, used exactly (a zero is treated as one).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingConfig {
    /// Samples retained.
    pub sample_capacity: usize,
    /// Block metadata records retained. Should cover `sample_capacity / smallest block length`,
    /// or history is limited by metadata instead of samples.
    pub block_capacity: usize,
}

impl RingConfig {
    /// A ring holding `seconds` of history at `sample_rate_hz`, sized for blocks of at least
    /// `min_block_len` samples.
    pub fn for_duration(sample_rate_hz: f64, seconds: f64, min_block_len: usize) -> Self {
        let sample_capacity = (sample_rate_hz * seconds).ceil().max(1.0) as usize;
        Self {
            sample_capacity,
            block_capacity: sample_capacity.div_ceil(min_block_len.max(1)),
        }
    }

    /// Bytes of sample storage.
    pub fn sample_bytes<T: RingSample>(&self) -> usize {
        self.sample_capacity
            .max(1)
            .saturating_mul(size_of::<T::Cell>())
    }
}

/// Errors pushing into the ring. A rejected push leaves the ring unchanged.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RingError {
    /// A block must hold at least one sample.
    #[error("empty block")]
    EmptyBlock,
    /// The block is longer than the ring.
    #[error("block of {len} samples exceeds ring capacity {capacity}")]
    BlockTooLarge {
        /// Block length.
        len: usize,
        /// Ring sample capacity.
        capacity: usize,
    },
    /// The block starts before the end of the previous one.
    #[error("block starts at sample {got}, before the previous block's end {expected_at_least}")]
    NonMonotonic {
        /// End of the previous block.
        expected_at_least: u64,
        /// Start of the rejected block.
        got: u64,
    },
    /// `first_sample + len` does not fit in the 64-bit sample counter.
    #[error("block of {len} samples at sample {first} overflows the sample counter")]
    IndexOverflow {
        /// Start of the rejected block.
        first: u64,
        /// Block length.
        len: usize,
    },
}

struct MetaSlot {
    first_sample: AtomicU64,
    len: AtomicU64,
    host_time_ns: AtomicI64,
    dropped_before: AtomicU64,
    gaps_total: AtomicU64,
    flags: AtomicU32,
    provenance_key: AtomicU64,
    provenance_slot: AtomicU64,
}

#[derive(Clone, Copy, Debug)]
struct BlockMeta {
    first_sample: u64,
    len: u64,
    host_time: Timestamp,
    flags: Discontinuity,
    /// Source-gap indices in the stream before this block's first sample (since stream start).
    gaps_total: u64,
    provenance_key: u64,
    provenance_slot: usize,
}

impl BlockMeta {
    fn end(&self) -> u64 {
        // `push` rejects blocks whose end overflows.
        self.first_sample + self.len
    }
}

enum CopyOutcome {
    Copied,
    NotCommitted,
    Overwritten,
}

type ProvenanceSlot = Mutex<Option<(u64, ProvenanceHandle)>>;

struct Shared<T: RingSample> {
    cells: Box<[T::Cell]>,
    meta: Box<[MetaSlot]>,
    /// End (exclusive) of the newest sample write, announced *before* the samples are stored.
    sample_write_end: AtomicU64,
    /// End (exclusive) of the newest committed block.
    sample_commit_end: AtomicU64,
    /// Blocks whose record write has been announced (published *before* the record is stored).
    meta_write_end: AtomicU64,
    /// Blocks fully committed (samples and metadata).
    meta_head: AtomicU64,
    /// First sample of block 0; meaningful once `meta_head > 0`.
    stream_start: AtomicU64,
    /// `(key, handle)` per slot; see the module docs.
    provenance: Box<[ProvenanceSlot]>,
    closed: AtomicBool,
    waiters: AtomicUsize,
    wake_lock: Mutex<()>,
    wake: Condvar,
}

/// Test-only injection point between the reader's counter loads: a test can advance the writer
/// at exactly this instant, deterministically reproducing a torn counter read.
#[cfg(test)]
mod race_hook {
    use std::cell::Cell;

    type Hook = Box<dyn FnOnce()>;

    thread_local! {
        static HOOK: Cell<Option<Hook>> = const { Cell::new(None) };
    }

    /// Runs `f` once, on this thread, at the next race point.
    pub(super) fn set(f: impl FnOnce() + 'static) {
        HOOK.with(|h| h.set(Some(Box::new(f))));
    }

    pub(super) fn fire() {
        if let Some(f) = HOOK.with(Cell::take) {
            f();
        }
    }
}

#[cfg(test)]
fn counter_race_point() {
    race_hook::fire();
}

#[cfg(not(test))]
#[inline(always)]
fn counter_race_point() {}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Provenance table size for a metadata ring of `block_capacity` records: at most
/// `block_capacity` keys are referenced by retained blocks, and the slack keeps free slots
/// available even when readers briefly hold some of them.
fn provenance_table_len(block_capacity: usize) -> usize {
    block_capacity.saturating_mul(2).saturating_add(8)
}

impl<T: RingSample> Shared<T> {
    fn sample_capacity(&self) -> u64 {
        self.cells.len() as u64
    }

    fn meta_capacity(&self) -> u64 {
        self.meta.len() as u64
    }

    /// Samples below this index may have been overwritten.
    fn oldest_valid_sample(&self) -> u64 {
        self.sample_write_end
            .load(SEQ)
            .saturating_sub(self.sample_capacity())
    }

    /// A committed block's metadata; `None` if the block is not committed yet or its record has
    /// been (or is being) overwritten.
    fn read_meta(&self, block: u64) -> Option<BlockMeta> {
        if block >= self.meta_head.load(SEQ) {
            return None;
        }
        let slot = &self.meta[(block % self.meta_capacity()) as usize];
        let meta = BlockMeta {
            first_sample: slot.first_sample.load(SEQ),
            len: slot.len.load(SEQ),
            host_time: Timestamp::from_unix_nanos(slot.host_time_ns.load(SEQ)),
            flags: Discontinuity::from_bits_truncate(slot.flags.load(SEQ) as u8),
            gaps_total: slot.gaps_total.load(SEQ),
            provenance_key: slot.provenance_key.load(SEQ),
            provenance_slot: slot.provenance_slot.load(SEQ) as usize,
        };
        let oldest = self
            .meta_write_end
            .load(SEQ)
            .saturating_sub(self.meta_capacity());
        (block >= oldest).then_some(meta)
    }

    /// Copies samples `[first, first + out.len())`.
    fn copy_out(&self, first: u64, out: &mut [T]) -> CopyOutcome {
        let n = out.len();
        let Some(end) = first.checked_add(n as u64) else {
            return CopyOutcome::NotCommitted;
        };
        if end > self.sample_commit_end.load(SEQ) {
            return CopyOutcome::NotCommitted;
        }
        let phys = (first % self.sample_capacity()) as usize;
        let run = n.min(self.cells.len() - phys);
        let (head, tail) = out.split_at_mut(run);
        for (dst, cell) in head.iter_mut().zip(&self.cells[phys..phys + run]) {
            *dst = T::load(cell);
        }
        for (dst, cell) in tail.iter_mut().zip(&self.cells[..n - run]) {
            *dst = T::load(cell);
        }
        if first >= self.oldest_valid_sample() {
            CopyOutcome::Copied
        } else {
            CopyOutcome::Overwritten
        }
    }

    fn copy_in(&self, first: u64, samples: &[T]) {
        let n = samples.len();
        let phys = (first % self.sample_capacity()) as usize;
        let run = n.min(self.cells.len() - phys);
        let (head, tail) = samples.split_at(run);
        for (src, cell) in head.iter().zip(&self.cells[phys..phys + run]) {
            T::store(cell, *src);
        }
        for (src, cell) in tail.iter().zip(&self.cells[..n - run]) {
            T::store(cell, *src);
        }
    }

    /// The handle for `key`, if its table slot still holds it.
    fn provenance(&self, key: u64, slot: usize) -> Option<ProvenanceHandle> {
        let entry = lock(self.provenance.get(slot)?);
        match &*entry {
            Some((k, handle)) if *k == key => Some(handle.clone()),
            _ => None,
        }
    }

    /// `(meta_head, meta_write_end)` as they were at one instant.
    ///
    /// Both counters only grow. Loads in the order head, write_end, head, write_end that agree
    /// pairwise mean `meta_head` held its value from its first load to its second and
    /// `meta_write_end` from its first load to its second; the second head load lies inside both
    /// spans, so both values held then. Hence `head <= write_end <= head + 1`.
    fn meta_counters(&self) -> (u64, u64) {
        loop {
            let head = self.meta_head.load(SEQ);
            counter_race_point();
            let write_end = self.meta_write_end.load(SEQ);
            if self.meta_head.load(SEQ) == head && self.meta_write_end.load(SEQ) == write_end {
                return (head, write_end);
            }
            std::hint::spin_loop();
        }
    }

    /// The smallest committed, retained block whose end is after `sample` (with its metadata),
    /// or the head block index and `None` if `sample` is at or beyond the newest block's end.
    ///
    /// `None` is only returned after block `head − 1` was validated to end at or before
    /// `sample`: callers treat it as "nothing lost".
    fn locate(&self, sample: u64) -> (u64, Option<BlockMeta>) {
        'retry: loop {
            // A torn pair (the writer lapping the metadata ring between the two loads) once made
            // the search range empty, returning `None` for a sample inside retained blocks.
            let (head, write_end) = self.meta_counters();
            if head == 0 {
                return (0, None);
            }
            let lo = write_end.saturating_sub(self.meta_capacity());
            if lo >= head {
                // No committed record is retained (only possible with `block_capacity` 1, while
                // the writer replaces the sole record): where `sample` lies is unknown.
                std::hint::spin_loop();
                continue 'retry;
            }
            let (mut left, mut right) = (lo, head);
            while left < right {
                let mid = left + (right - left) / 2;
                let Some(m) = self.read_meta(mid) else {
                    continue 'retry;
                };
                if m.end() > sample {
                    right = mid;
                } else {
                    left = mid + 1;
                }
            }
            if left == head {
                return (head, None);
            }
            match self.read_meta(left) {
                Some(m) => return (left, Some(m)),
                None => continue 'retry,
            }
        }
    }

    fn notify_all_blocking(&self) {
        let _guard = lock(&self.wake_lock);
        self.wake.notify_all();
    }
}

/// Creates a ring and returns its only writer and a handle for attaching readers.
pub fn ring_buffer<T: RingSample>(config: RingConfig) -> (RingWriter<T>, RingHandle<T>) {
    let sample_capacity = config.sample_capacity.max(1);
    let block_capacity = config.block_capacity.max(1);
    let table_len = provenance_table_len(block_capacity);
    let provenance: Box<[ProvenanceSlot]> = (0..table_len).map(|_| Mutex::new(None)).collect();
    // Some platforms allocate an OS mutex lazily on first use; do it now, not on the writer's
    // first provenance change.
    for slot in provenance.iter() {
        drop(lock(slot));
    }
    let shared = Arc::new(Shared {
        cells: (0..sample_capacity).map(|_| T::new_cell()).collect(),
        meta: (0..block_capacity)
            .map(|_| MetaSlot {
                first_sample: AtomicU64::new(0),
                len: AtomicU64::new(0),
                host_time_ns: AtomicI64::new(0),
                dropped_before: AtomicU64::new(0),
                gaps_total: AtomicU64::new(0),
                flags: AtomicU32::new(0),
                provenance_key: AtomicU64::new(0),
                provenance_slot: AtomicU64::new(0),
            })
            .collect(),
        sample_write_end: AtomicU64::new(0),
        sample_commit_end: AtomicU64::new(0),
        meta_write_end: AtomicU64::new(0),
        meta_head: AtomicU64::new(0),
        stream_start: AtomicU64::new(0),
        provenance,
        closed: AtomicBool::new(false),
        waiters: AtomicUsize::new(0),
        wake_lock: Mutex::new(()),
        wake: Condvar::new(),
    });
    drop(lock(&shared.wake_lock));
    (
        RingWriter {
            shared: Arc::clone(&shared),
            next_block: 0,
            stream_end: None,
            gaps_total: 0,
            next_provenance_key: 0,
            current_provenance: None,
            live_provenance: VecDeque::with_capacity(block_capacity + 2),
            slot_in_use: vec![false; table_len].into_boxed_slice(),
            slot_cursor: 0,
        },
        RingHandle { shared },
    )
}

/// The ring's single writer. Dropping it closes the ring: readers drain, then see
/// [`ReadOutcome::Closed`].
pub struct RingWriter<T: RingSample> {
    shared: Arc<Shared<T>>,
    next_block: u64,
    stream_end: Option<u64>,
    gaps_total: u64,
    next_provenance_key: u64,
    /// `(key, table slot, handle)` of the newest block.
    current_provenance: Option<(u64, usize, ProvenanceHandle)>,
    /// `(key, first block using it, table slot)`, oldest first. At most `block_capacity + 1`.
    live_provenance: VecDeque<(u64, u64, usize)>,
    /// Writer-side mirror: table slots holding a key some retained block may reference.
    slot_in_use: Box<[bool]>,
    slot_cursor: usize,
}

impl<T: RingSample> RingWriter<T> {
    /// Appends a block. Never blocks on readers and allocates nothing.
    ///
    /// `header.first_sample()` must not precede the previous block's end. A later start is a
    /// gap: the stored record gets [`Discontinuity::GAP`] and `dropped_before` from the counter
    /// jump.
    pub fn push(&mut self, header: &BlockHeader, samples: &[T]) -> Result<(), RingError> {
        let n = samples.len();
        if n == 0 {
            return Err(RingError::EmptyBlock);
        }
        if n as u64 > self.shared.sample_capacity() {
            return Err(RingError::BlockTooLarge {
                len: n,
                capacity: self.shared.cells.len(),
            });
        }
        let first = header.first_sample();
        let end = first
            .checked_add(n as u64)
            .ok_or(RingError::IndexOverflow { first, len: n })?;
        let mut flags = header.discontinuity;
        let (dropped_before, gap) = match self.stream_end {
            Some(prev_end) if first < prev_end => {
                return Err(RingError::NonMonotonic {
                    expected_at_least: prev_end,
                    got: first,
                });
            }
            Some(prev_end) => (first - prev_end, first - prev_end),
            // Samples missing before the stream started are not part of the ring's stream.
            None => (header.dropped_before, 0),
        };
        if dropped_before > 0 {
            flags |= Discontinuity::GAP;
        }
        // Gap indices never exceed `first`, so this cannot overflow.
        let gaps_total = self.gaps_total + gap;

        let block = self.next_block;
        // 1. Announce the record write. From here on, every block older than
        //    `block + 1 − block_capacity` is invalid for every reader.
        self.shared.meta_write_end.store(block + 1, SEQ);
        let (provenance_key, provenance_slot) = self.provenance_for(&header.provenance, block);
        let shared = &*self.shared;
        if block == 0 {
            shared.stream_start.store(first, SEQ);
        }
        let slot = &shared.meta[(block % shared.meta_capacity()) as usize];
        slot.first_sample.store(first, SEQ);
        slot.len.store(n as u64, SEQ);
        slot.host_time_ns
            .store(header.time.host_time.as_unix_nanos(), SEQ);
        slot.dropped_before.store(dropped_before, SEQ);
        slot.gaps_total.store(gaps_total, SEQ);
        slot.flags.store(u32::from(flags.bits()), SEQ);
        slot.provenance_key.store(provenance_key, SEQ);
        slot.provenance_slot.store(provenance_slot as u64, SEQ);

        // 2. Announce the sample write, then store.
        shared.sample_write_end.store(end, SEQ);
        shared.copy_in(first, samples);

        // 3. Commit: samples first, so a reader that sees the new head sees their end too.
        shared.sample_commit_end.store(end, SEQ);
        shared.meta_head.store(block + 1, SEQ);
        self.next_block = block + 1;
        self.stream_end = Some(end);
        self.gaps_total = gaps_total;

        if shared.waiters.load(SEQ) > 0 {
            if let Ok(_guard) = shared.wake_lock.try_lock() {
                shared.wake.notify_all();
            }
        }
        Ok(())
    }

    /// Stream sample index one past the newest block, if any.
    pub fn stream_end(&self) -> Option<u64> {
        self.stream_end
    }

    /// A handle for attaching readers.
    pub fn handle(&self) -> RingHandle<T> {
        RingHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    /// The provenance key and table slot for `block`. Must run after the record announce for
    /// `block`, so any slot it reuses is referenced only by invalid blocks.
    fn provenance_for(&mut self, handle: &ProvenanceHandle, block: u64) -> (u64, usize) {
        if let Some((key, slot, current)) = &self.current_provenance {
            if current == handle {
                return (*key, *slot);
            }
        }
        let oldest_valid_block = (block + 1).saturating_sub(self.shared.meta_capacity());
        // The front key's last block is the block before the next key's first block.
        while self.live_provenance.len() >= 2 && self.live_provenance[1].1 <= oldest_valid_block {
            if let Some((_, _, slot)) = self.live_provenance.pop_front() {
                self.slot_in_use[slot] = false;
            }
        }
        let key = self.next_provenance_key;
        self.next_provenance_key += 1;
        let slot = self.claim_slot(key, handle);
        self.live_provenance.push_back((key, block, slot));
        self.current_provenance = Some((key, slot, handle.clone()));
        (key, slot)
    }

    /// Stores `(key, handle)` in a free table slot, skipping (never waiting on) slots a reader
    /// holds. At most `block_capacity + 1` of the `2 × block_capacity + 8` slots are in use, so a
    /// free, unheld slot exists unless more readers than that are inside a lookup at once.
    fn claim_slot(&mut self, key: u64, handle: &ProvenanceHandle) -> usize {
        let table = &self.shared.provenance;
        loop {
            for _ in 0..table.len() {
                let slot = self.slot_cursor;
                self.slot_cursor = (self.slot_cursor + 1) % table.len();
                if self.slot_in_use[slot] {
                    continue;
                }
                let mut entry = match table[slot].try_lock() {
                    Ok(entry) => entry,
                    Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                    Err(TryLockError::WouldBlock) => continue,
                };
                let evicted = entry.replace((key, handle.clone()));
                drop(entry);
                drop(evicted);
                self.slot_in_use[slot] = true;
                return slot;
            }
            std::hint::spin_loop();
        }
    }
}

impl RingWriter<Complex32> {
    /// Appends an owned [`SampleBlock`].
    pub fn push_block(&mut self, block: &SampleBlock) -> Result<(), RingError> {
        self.push(&block.header, &block.samples)
    }
}

impl<T: RingSample> Drop for RingWriter<T> {
    fn drop(&mut self) {
        self.shared.closed.store(true, SEQ);
        self.shared.notify_all_blocking();
    }
}

/// A cloneable handle to a ring, for attaching readers and pre-trigger captures.
pub struct RingHandle<T: RingSample> {
    shared: Arc<Shared<T>>,
}

impl<T: RingSample> Clone for RingHandle<T> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T: RingSample> RingHandle<T> {
    /// Samples retained.
    pub fn sample_capacity(&self) -> usize {
        self.shared.cells.len()
    }

    /// Block metadata records retained.
    pub fn block_capacity(&self) -> usize {
        self.shared.meta.len()
    }

    /// Blocks committed so far.
    pub fn blocks_written(&self) -> u64 {
        self.shared.meta_head.load(SEQ)
    }

    /// First sample index of the stream, once a block has been committed.
    pub fn stream_start(&self) -> Option<u64> {
        (self.shared.meta_head.load(SEQ) > 0).then(|| self.shared.stream_start.load(SEQ))
    }

    /// Stream sample index one past the newest committed block, if any.
    pub fn next_sample(&self) -> Option<u64> {
        loop {
            let head = self.shared.meta_head.load(SEQ);
            if head == 0 {
                return None;
            }
            if let Some(m) = self.shared.read_meta(head - 1) {
                return Some(m.end());
            }
        }
    }

    /// The oldest stream sample index still readable, if any.
    pub fn oldest_sample(&self) -> Option<u64> {
        let oldest = self.shared.oldest_valid_sample();
        let (_, meta) = self.shared.locate(oldest);
        meta.map(|m| m.first_sample.max(oldest))
    }

    /// The writer has been dropped.
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(SEQ)
    }

    /// Best-effort: pins the sample storage in RAM so capture never waits on paging. Failure
    /// (usually a memlock limit) leaves the ring fully usable.
    pub fn lock_memory(&self) -> MemoryLock {
        crate::rt::lock_memory(&self.shared.cells)
    }

    /// A reader starting at the live edge: it sees the next block written (on an empty ring, the
    /// first block of the stream, wherever its sample counter starts).
    pub fn reader(&self) -> RingReader<T> {
        let shared = &*self.shared;
        let cursor = loop {
            let head = shared.meta_head.load(SEQ);
            if head == 0 {
                break Cursor::Unresolved { from: 0 };
            }
            if let Some(m) = shared.read_meta(head - 1) {
                break Cursor::At {
                    sample: m.end(),
                    block: head,
                    gaps: m.gaps_total,
                };
            }
        };
        RingReader::new(Arc::clone(&self.shared), cursor)
    }

    /// A reader starting at stream sample `sample` (e.g. a pre-trigger position), or at the
    /// stream start if that is later. A sample in the future is waited for. If that history has
    /// already been overwritten, the first read reports [`ReadOutcome::Overrun`].
    pub fn reader_at(&self, sample: u64) -> RingReader<T> {
        RingReader::new(
            Arc::clone(&self.shared),
            Cursor::Unresolved { from: sample },
        )
    }

    /// Starts an allocating pre-trigger capture of `window`; see [`PreTriggerCapture`].
    pub fn pre_trigger(&self, window: TriggerWindow) -> Result<PreTriggerCapture<T>, CaptureError> {
        PreTriggerCapture::new(self, window)
    }

    /// Streams `window` chunk by chunk without allocating it; see [`TriggerStream`].
    pub fn trigger_stream(&self, window: TriggerWindow) -> TriggerStream<T> {
        TriggerStream::new(self, window)
    }
}

/// What a lapped reader does after an overrun.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ResyncPolicy {
    /// Jump to the start of the newest block (least further loss; real-time consumers).
    #[default]
    Latest,
    /// Jump to the oldest sample still retained (most history; captures and recorders).
    Oldest,
}

/// A contiguous run of samples copied out of the ring.
#[derive(Clone, Debug)]
pub struct ReadChunk {
    /// Time of the first sample; `time.sample_index` is its stream index.
    pub time: SampleTime,
    /// Samples copied into the caller's buffer (from index 0).
    pub len: usize,
    /// The chunk starts at a block boundary.
    pub block_start: bool,
    /// The block's discontinuity flags (only when `block_start`).
    pub discontinuity: Discontinuity,
    /// Source-gap indices between this reader's previous position and the chunk (only when
    /// `block_start`). For a contiguous reader this is the block's recorded gap; after an overrun
    /// it excludes any part already counted in the overrun's `gap_samples`.
    pub dropped_before: u64,
    /// Provenance in force for every sample of the chunk.
    pub provenance: ProvenanceHandle,
}

impl ReadChunk {
    /// Stream index of the first sample.
    pub fn first_sample(&self) -> u64 {
        self.time.sample_index
    }

    /// Stream index one past the last sample.
    pub fn end_sample(&self) -> u64 {
        self.time.sample_index + self.len as u64
    }
}

/// Result of a read.
#[derive(Clone, Debug)]
pub enum ReadOutcome {
    /// Samples were copied.
    Data(ReadChunk),
    /// The reader was lapped (or its start was no longer retained). Stream indices from the
    /// previous position to `resume_at` were skipped: `lost_samples + gap_samples` of them.
    Overrun {
        /// Samples the source produced that were overwritten before this reader copied them.
        /// If the start of a [`RingHandle::reader_at`] was already gone when the reader was
        /// positioned, block boundaries there are unknown and source gaps in that span are
        /// counted here too.
        lost_samples: u64,
        /// Source-gap indices (never produced) inside the skipped span.
        gap_samples: u64,
        /// Stream index the next read starts from.
        resume_at: u64,
    },
    /// No new data yet.
    Empty,
    /// The writer is gone and everything has been read.
    Closed,
}

#[derive(Clone, Copy, Debug)]
enum Cursor {
    /// Not yet positioned: starts at `from`, or at the stream start if later, once a block
    /// reaching it is committed.
    Unresolved { from: u64 },
    /// Next sample `sample`; `block` is the first block that may hold it; `gaps` counts source-gap
    /// indices in the stream before `sample`.
    At { sample: u64, block: u64, gaps: u64 },
}

struct ReaderState {
    cursor: Cursor,
    policy: ResyncPolicy,
    provenance_cache: Option<(u64, ProvenanceHandle)>,
    samples_read: u64,
    lost_samples: u64,
    gap_samples: u64,
    overruns: u64,
    /// `meta_head` when the last read found nothing; waits park until it moves.
    seen_head: u64,
}

/// An independent cursor over the ring. Attach with [`RingHandle::reader`] or
/// [`RingHandle::reader_at`]; detach by dropping.
pub struct RingReader<T: RingSample> {
    shared: Arc<Shared<T>>,
    state: ReaderState,
}

impl<T: RingSample> RingReader<T> {
    fn new(shared: Arc<Shared<T>>, cursor: Cursor) -> Self {
        Self {
            shared,
            state: ReaderState {
                cursor,
                policy: ResyncPolicy::default(),
                provenance_cache: None,
                samples_read: 0,
                lost_samples: 0,
                gap_samples: 0,
                overruns: 0,
                seen_head: 0,
            },
        }
    }

    /// Sets the resync policy.
    pub fn with_resync_policy(mut self, policy: ResyncPolicy) -> Self {
        self.state.policy = policy;
        self
    }

    /// Stream index of the next sample this reader will account for.
    pub fn position(&self) -> u64 {
        match self.state.cursor {
            Cursor::Unresolved { from } => from,
            Cursor::At { sample, .. } => sample,
        }
    }

    /// Samples returned so far.
    pub fn samples_read(&self) -> u64 {
        self.state.samples_read
    }

    /// Samples lost to overruns (overwritten before they were read).
    pub fn lost_samples(&self) -> u64 {
        self.state.lost_samples
    }

    /// Source-gap indices passed so far (chunks' `dropped_before` plus overruns' `gap_samples`).
    pub fn gap_samples(&self) -> u64 {
        self.state.gap_samples
    }

    /// Number of overruns reported.
    pub fn overruns(&self) -> u64 {
        self.state.overruns
    }

    /// Copies the next available samples into `out` without waiting. A chunk never spans a
    /// block boundary.
    pub fn read(&mut self, out: &mut [T]) -> ReadOutcome {
        self.state.read(&self.shared, out, u64::MAX)
    }

    /// Like [`RingReader::read`], waiting up to `timeout` for data. An empty `out` returns at once.
    pub fn read_timeout(&mut self, out: &mut [T], timeout: Duration) -> ReadOutcome {
        if out.is_empty() {
            return self.read(out);
        }
        let deadline = Instant::now() + timeout;
        loop {
            match self.read(out) {
                ReadOutcome::Empty => {}
                other => return other,
            }
            if !self.wait_for_data(deadline) {
                return ReadOutcome::Empty;
            }
        }
    }

    /// Reads samples with stream index below `limit`.
    pub(crate) fn read_until(&mut self, out: &mut [T], limit: u64) -> ReadOutcome {
        self.state.read(&self.shared, out, limit)
    }

    /// Parks until the ring may have new data or `deadline` passes. `false` once past deadline.
    pub(crate) fn wait_for_data(&self, deadline: Instant) -> bool {
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let shared = &*self.shared;
        shared.waiters.fetch_add(1, SEQ);
        {
            let guard = lock(&shared.wake_lock);
            if shared.meta_head.load(SEQ) <= self.state.seen_head && !shared.closed.load(SEQ) {
                let slice = (deadline - now).min(WAIT_SLICE);
                drop(
                    shared
                        .wake
                        .wait_timeout(guard, slice)
                        .unwrap_or_else(PoisonError::into_inner),
                );
            }
        }
        shared.waiters.fetch_sub(1, SEQ);
        true
    }
}

impl ReaderState {
    fn read<T: RingSample>(
        &mut self,
        shared: &Shared<T>,
        out: &mut [T],
        limit: u64,
    ) -> ReadOutcome {
        loop {
            let step = match self.cursor {
                Cursor::Unresolved { from } => self.resolve(shared, from),
                Cursor::At {
                    sample,
                    block,
                    gaps,
                } => self.step(shared, out, limit, sample, block, gaps),
            };
            if let Some(outcome) = step {
                return outcome;
            }
        }
    }

    /// `Empty`, or `Closed` if the writer was gone before `head` was loaded.
    fn nothing(&mut self, closed: bool, head: u64) -> Option<ReadOutcome> {
        self.seen_head = head;
        Some(if closed {
            ReadOutcome::Closed
        } else {
            ReadOutcome::Empty
        })
    }

    /// Positions an unresolved cursor. `None` = retry the read.
    fn resolve<T: RingSample>(&mut self, shared: &Shared<T>, from: u64) -> Option<ReadOutcome> {
        let closed = shared.closed.load(SEQ);
        let head = shared.meta_head.load(SEQ);
        if head == 0 {
            return self.nothing(closed, 0);
        }
        let stream_start = shared.stream_start.load(SEQ);
        if from <= stream_start {
            // Nothing precedes the stream start, so its gap count is 0 even if block 0 is gone:
            // any loss from here is split exactly.
            self.cursor = Cursor::At {
                sample: stream_start,
                block: 0,
                gaps: 0,
            };
            return None;
        }
        let (block, meta) = shared.locate(from);
        let Some(m) = meta else {
            // At or beyond the newest block's end: wait for the block that reaches it.
            self.cursor = Cursor::Unresolved { from };
            return self.nothing(closed, block);
        };
        if m.first_sample <= from {
            self.cursor = Cursor::At {
                sample: from,
                block,
                gaps: m.gaps_total,
            };
            return None;
        }
        // `from` lies before block `block`. Its span is a source gap if the previous block is
        // still known (it ends at or before `from`); otherwise that history is gone.
        let gap = m.first_sample - from;
        let previous_known = block
            .checked_sub(1)
            .is_some_and(|prev| shared.read_meta(prev).is_some());
        if previous_known {
            debug_assert!(gap <= m.gaps_total);
            self.cursor = Cursor::At {
                sample: from,
                block,
                gaps: m.gaps_total.saturating_sub(gap),
            };
            return None;
        }
        self.cursor = Cursor::At {
            sample: m.first_sample,
            block,
            gaps: m.gaps_total,
        };
        Some(self.report_overrun(gap, 0, m.first_sample))
    }

    /// One read attempt from a positioned cursor. `None` = retry.
    fn step<T: RingSample>(
        &mut self,
        shared: &Shared<T>,
        out: &mut [T],
        limit: u64,
        pos: u64,
        block: u64,
        gaps: u64,
    ) -> Option<ReadOutcome> {
        if out.is_empty() || pos >= limit {
            let head = shared.meta_head.load(SEQ);
            return self.nothing(false, head);
        }
        let closed = shared.closed.load(SEQ);
        let head = shared.meta_head.load(SEQ);
        if block >= head {
            return self.nothing(closed, head);
        }
        let Some(m) = shared.read_meta(block) else {
            return self.lapped(shared, pos, gaps);
        };
        if pos >= m.end() {
            self.cursor = Cursor::At {
                sample: pos,
                block: block + 1,
                gaps,
            };
            return None;
        }
        let from = pos.max(m.first_sample);
        // Only stored samples can be overwritten. A source gap (an index jump) before `from` was
        // never written, so a reader parked at the jump is not lapped by the samples after it
        // (T-072: comparing `pos` here resynced such a reader past retained post-gap blocks).
        if from < shared.oldest_valid_sample() {
            return self.lapped(shared, pos, gaps);
        }
        if from >= limit {
            // The limit falls inside the source gap before this block: account the gap up to it.
            let gap = limit - pos;
            self.gap_samples += gap;
            self.cursor = Cursor::At {
                sample: limit,
                block,
                gaps: gaps + gap,
            };
            return self.nothing(false, head);
        }
        let n = (m.end() - from).min(limit - from).min(out.len() as u64) as usize;
        let out = &mut out[..n];
        match shared.copy_out(from, out) {
            CopyOutcome::Copied => {}
            CopyOutcome::Overwritten => return self.lapped(shared, pos, gaps),
            CopyOutcome::NotCommitted => {
                // Unreachable: `block` is committed, so are its samples.
                debug_assert!(false, "committed block {block} has uncommitted samples");
                return self.nothing(false, head);
            }
        }
        let Some(provenance) = self.resolve_provenance(shared, &m) else {
            // The slot was reused, so the record is stale by now.
            return self.lapped(shared, pos, gaps);
        };

        // Success: only now does the cursor move.
        let block_start = pos <= m.first_sample;
        let gap = from - pos;
        debug_assert!(!block_start || gaps + gap == m.gaps_total);
        let end = from + n as u64;
        self.cursor = Cursor::At {
            sample: end,
            block: if end == m.end() { block + 1 } else { block },
            gaps: if block_start { m.gaps_total } else { gaps },
        };
        self.samples_read += n as u64;
        self.gap_samples += gap;
        let anchor = SampleTime {
            sample_index: m.first_sample,
            host_time: m.host_time,
        };
        Some(ReadOutcome::Data(ReadChunk {
            time: SampleTime {
                sample_index: from,
                host_time: anchor.time_of(from, provenance.tune.sample_rate_hz),
            },
            len: n,
            block_start,
            discontinuity: if block_start {
                m.flags
            } else {
                Discontinuity::NONE
            },
            dropped_before: gap,
            provenance,
        }))
    }

    fn resolve_provenance<T: RingSample>(
        &mut self,
        shared: &Shared<T>,
        m: &BlockMeta,
    ) -> Option<ProvenanceHandle> {
        if let Some((cached, handle)) = &self.provenance_cache {
            if *cached == m.provenance_key {
                return Some(handle.clone());
            }
        }
        let handle = shared.provenance(m.provenance_key, m.provenance_slot)?;
        self.provenance_cache = Some((m.provenance_key, handle.clone()));
        Some(handle)
    }

    /// The data or metadata at `pos` is gone. Continues without loss when the samples survive
    /// and the blocks between are still known; otherwise resyncs per policy and reports the
    /// skipped span exactly. `None` = retry.
    fn lapped<T: RingSample>(
        &mut self,
        shared: &Shared<T>,
        pos: u64,
        gaps: u64,
    ) -> Option<ReadOutcome> {
        let oldest = shared.oldest_valid_sample();
        let (block, meta) = shared.locate(pos);
        // Lossless when the next samples the reader needs are still stored: `pos` itself, or the
        // first sample of the block after a source gap that starts exactly at `pos`.
        let lossless = match meta {
            None => pos >= oldest,
            Some(m) => {
                pos.max(m.first_sample) >= oldest
                    && (m.first_sample <= pos
                        || m.gaps_total.checked_sub(gaps) == Some(m.first_sample - pos))
            }
        };
        if lossless {
            self.cursor = Cursor::At {
                sample: pos,
                block,
                gaps,
            };
            return None;
        }
        let (block, meta, target) = match self.policy {
            ResyncPolicy::Latest => loop {
                let head = shared.meta_head.load(SEQ);
                if head == 0 {
                    return self.nothing(false, 0);
                }
                if let Some(m) = shared.read_meta(head - 1) {
                    break (head - 1, m, m.first_sample);
                }
            },
            ResyncPolicy::Oldest => {
                let oldest = shared.oldest_valid_sample().max(pos);
                match shared.locate(oldest) {
                    (block, Some(m)) => (block, m, m.first_sample.max(oldest)),
                    // Only uncommitted samples lie beyond: nothing readable yet.
                    (block, None) => return self.nothing(false, block),
                }
            }
        };
        if target <= pos {
            self.cursor = Cursor::At {
                sample: pos,
                block,
                gaps,
            };
            return None;
        }
        let skipped = target - pos;
        debug_assert!(meta.gaps_total >= gaps && meta.gaps_total - gaps <= skipped);
        let gap = meta.gaps_total.saturating_sub(gaps).min(skipped);
        self.cursor = Cursor::At {
            sample: target,
            block,
            gaps: meta.gaps_total,
        };
        Some(self.report_overrun(skipped - gap, gap, target))
    }

    fn report_overrun(&mut self, lost: u64, gap: u64, resume_at: u64) -> ReadOutcome {
        self.lost_samples += lost;
        self.gap_samples += gap;
        self.overruns += 1;
        ReadOutcome::Overrun {
            lost_samples: lost,
            gap_samples: gap,
            resume_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use hk_model::{ClockSource, Provenance, TimestampMethod, Tune};

    use super::*;

    fn provenance(center_hz: f64) -> ProvenanceHandle {
        ProvenanceHandle::new(Provenance {
            device_id: "synthetic:ring-test".into(),
            tune: Tune {
                center_hz,
                sample_rate_hz: 1e6,
                lna_db: 16.0,
                vga_db: 20.0,
                amp_on: false,
                bandwidth_hz: 1e6,
            },
            quantisation_limited: false,
            overload: false,
            temperature_c: None,
            antenna_port: None,
            bias_tee: hk_model::BiasTee::Unknown,
            clock_source: ClockSource::Internal,
            clock_locked: true,
            calibration_state_ref: None,
            spur_mask_ref: None,
            timestamp_method: TimestampMethod::Synthetic,
            timestamp_error_budget_ns: Some(0),
            capture_artefacts: Vec::new(),
        })
    }

    fn header(first: u64, prov: &ProvenanceHandle) -> BlockHeader {
        BlockHeader {
            time: SampleTime {
                sample_index: first,
                host_time: Timestamp::from_unix_nanos((first % (1 << 40)) as i64 * 1000),
            },
            provenance: prov.clone(),
            discontinuity: Discontinuity::NONE,
            dropped_before: 0,
        }
    }

    /// Samples whose bits encode their full 64-bit stream index.
    fn counter(first: u64, n: usize) -> Vec<Complex32> {
        (0..n as u64).map(|i| encode(first + i)).collect()
    }

    fn encode(index: u64) -> Complex32 {
        Complex32::new(
            f32::from_bits((index >> 32) as u32),
            f32::from_bits(index as u32),
        )
    }

    fn decode(s: Complex32) -> u64 {
        (u64::from(s.re.to_bits()) << 32) | u64::from(s.im.to_bits())
    }

    fn push(w: &mut RingWriter<Complex32>, first: u64, n: usize, prov: &ProvenanceHandle) {
        w.push(&header(first, prov), &counter(first, n)).unwrap();
    }

    fn data(outcome: ReadOutcome) -> ReadChunk {
        match outcome {
            ReadOutcome::Data(c) => c,
            other => panic!("expected data, got {other:?}"),
        }
    }

    fn overrun(outcome: ReadOutcome) -> (u64, u64, u64) {
        match outcome {
            ReadOutcome::Overrun {
                lost_samples,
                gap_samples,
                resume_at,
            } => (lost_samples, gap_samples, resume_at),
            other => panic!("expected overrun, got {other:?}"),
        }
    }

    fn ring(samples: usize, blocks: usize) -> (RingWriter<Complex32>, RingHandle<Complex32>) {
        ring_buffer::<Complex32>(RingConfig {
            sample_capacity: samples,
            block_capacity: blocks,
        })
    }

    #[test]
    fn readers_see_contiguous_blocks_with_metadata() {
        let (mut w, ring) = ring(1024, 16);
        let p = provenance(100e6);
        let mut r = ring.reader();
        assert!(matches!(
            r.read(&mut [Complex32::default(); 8]),
            ReadOutcome::Empty
        ));
        push(&mut w, 0, 100, &p);
        push(&mut w, 100, 100, &p);
        let mut buf = vec![Complex32::default(); 64];
        let c = data(r.read(&mut buf));
        assert_eq!((c.first_sample(), c.len, c.block_start), (0, 64, true));
        assert_eq!(buf[..64], counter(0, 64)[..]);
        assert_eq!(c.provenance, p);
        let c = data(r.read(&mut buf));
        assert_eq!((c.first_sample(), c.len, c.block_start), (64, 36, false));
        // Time of a mid-block chunk is derived from the block anchor and the rate.
        assert_eq!(
            c.time.host_time.as_unix_nanos(),
            64 * 1_000_000_000 / 1_000_000
        );
        let c = data(r.read(&mut buf));
        assert_eq!((c.first_sample(), c.len, c.block_start), (100, 64, true));
        assert_eq!(ring.next_sample(), Some(200));
        drop(w);
        let _ = data(r.read(&mut buf));
        assert!(matches!(r.read(&mut buf), ReadOutcome::Closed));
    }

    /// Regression (T-003 follow-up): the writer laps the whole metadata ring between the reader's
    /// loads of `meta_head` and `meta_write_end` inside `locate`. The torn pair once gave an empty
    /// search range that `lapped` took as lossless, jumping the cursor past produced samples and
    /// reporting them as a source gap (`dropped_before`).
    #[test]
    fn locate_survives_writer_lapping_meta_ring_between_counter_loads() {
        use std::cell::{Cell, RefCell};
        use std::rc::Rc;

        const LEN: u64 = 10;
        // Six contiguous blocks; four more written at the race point (one after a 7-sample gap);
        // one more after an 8-sample gap once the race is over.
        let before: Vec<u64> = (0..6).map(|b| b * LEN).collect();
        let during = [60, 70, 87, 97];
        let after = 115;
        let blocks: Vec<u64> = before
            .iter()
            .copied()
            .chain(during)
            .chain([after])
            .collect();
        let end = after + LEN;
        let produced_below =
            |x: u64| -> u64 { blocks.iter().map(|&f| x.clamp(f, f + LEN) - f).sum() };
        let true_gaps = |a: u64, b: u64| (b - a) - (produced_below(b) - produced_below(a));

        // Sample storage retains everything; the metadata ring keeps only 4 records.
        let (w, ring) = ring(4096, 4);
        let w = Rc::new(RefCell::new(Some(w)));
        let p = provenance(100e6);
        for &f in &before {
            push(w.borrow_mut().as_mut().unwrap(), f, LEN as usize, &p);
        }
        // Positioned at 0 in block 0, whose record is gone while its samples are still stored.
        let mut r = ring.reader_at(0).with_resync_policy(ResyncPolicy::Oldest);

        let fired = Rc::new(Cell::new(false));
        {
            let (w, p, fired) = (Rc::clone(&w), p.clone(), Rc::clone(&fired));
            race_hook::set(move || {
                let mut w = w.borrow_mut();
                for f in during {
                    push(w.as_mut().unwrap(), f, LEN as usize, &p);
                }
                fired.set(true);
            });
        }

        let mut buf = vec![Complex32::default(); 64];
        let (mut pos, mut read, mut lost, mut gaps) = (0u64, 0u64, 0u64, 0u64);
        // Phase 1 drains what was written (the race fires inside the first read); phase 2 adds
        // the post-race block, closes the ring and drains to `Closed`.
        for phase in 1..=2 {
            if phase == 2 {
                assert!(fired.get(), "the race point was never reached");
                push(w.borrow_mut().as_mut().unwrap(), after, LEN as usize, &p);
                drop(w.borrow_mut().take());
            }
            loop {
                match r.read(&mut buf) {
                    ReadOutcome::Data(c) => {
                        let first = c.first_sample();
                        assert_eq!(first, pos + c.dropped_before, "chunk start vs position");
                        assert_eq!(
                            c.dropped_before,
                            true_gaps(pos, first),
                            "dropped_before must be exactly the source gap in [{pos}, {first})"
                        );
                        for (j, s) in buf[..c.len].iter().enumerate() {
                            assert_eq!(decode(*s), first + j as u64, "sample value");
                        }
                        read += c.len as u64;
                        gaps += c.dropped_before;
                        pos = c.end_sample();
                    }
                    ReadOutcome::Overrun {
                        lost_samples,
                        gap_samples,
                        resume_at,
                    } => {
                        assert_eq!(resume_at, pos + lost_samples + gap_samples, "overrun span");
                        assert_eq!(gap_samples, true_gaps(pos, resume_at), "overrun gap split");
                        lost += lost_samples;
                        gaps += gap_samples;
                        pos = resume_at;
                    }
                    ReadOutcome::Empty => break,
                    ReadOutcome::Closed => {
                        assert_eq!(phase, 2, "closed before the writer was dropped");
                        break;
                    }
                }
                // The cursor never runs ahead of what has been accounted.
                assert_eq!(
                    r.position(),
                    pos,
                    "cursor moved past the accounted position"
                );
            }
        }
        assert_eq!(pos, end, "drained to the stream end");
        assert_eq!(
            read + lost + gaps,
            end,
            "every index accounted exactly once"
        );
        assert_eq!(
            (r.samples_read(), r.lost_samples(), r.gap_samples()),
            (read, lost, gaps)
        );
        // Samples 0..60 were still stored but their records were not: lost, never gaps.
        assert_eq!((read, lost, gaps), (50, 60, 15));
    }

    #[test]
    fn capacities_are_exact() {
        let (_, ring) = ring(1000, 7);
        assert_eq!((ring.sample_capacity(), ring.block_capacity()), (1000, 7));
    }

    #[test]
    fn gaps_are_index_jumps_and_flagged() {
        let (mut w, ring) = ring(1024, 16);
        let p = provenance(100e6);
        let mut r = ring.reader();
        push(&mut w, 0, 10, &p);
        push(&mut w, 25, 10, &p);
        let mut buf = vec![Complex32::default(); 64];
        let _ = data(r.read(&mut buf));
        let c = data(r.read(&mut buf));
        assert_eq!(c.first_sample(), 25);
        assert!(c.discontinuity.contains(Discontinuity::GAP));
        assert_eq!(c.dropped_before, 15);
        assert_eq!(decode(buf[0]), 25);
        assert_eq!(r.gap_samples(), 15);
        assert!(matches!(
            w.push(&header(30, &p), &counter(30, 1)),
            Err(RingError::NonMonotonic { .. })
        ));
    }

    #[test]
    fn overrun_latest_policy_counts_and_resyncs() {
        let (mut w, ring) = ring(1024, 64);
        let p = provenance(100e6);
        let mut r = ring.reader();
        for b in 0..30 {
            push(&mut w, b * 100, 100, &p);
        }
        let mut buf = vec![Complex32::default(); 1000];
        assert_eq!(overrun(r.read(&mut buf)), (2900, 0, 2900));
        let c = data(r.read(&mut buf));
        assert_eq!((c.first_sample(), c.len), (2900, 100));
        assert_eq!(decode(buf[0]), 2900);
        assert_eq!(r.samples_read() + r.lost_samples(), 3000);
        assert_eq!(r.overruns(), 1);
    }

    #[test]
    fn overrun_oldest_policy_resumes_at_oldest_retained_sample() {
        let (mut w, ring) = ring(1024, 64);
        let p = provenance(100e6);
        let mut r = ring.reader().with_resync_policy(ResyncPolicy::Oldest);
        for b in 0..30 {
            push(&mut w, b * 100, 100, &p);
        }
        assert_eq!(ring.oldest_sample(), Some(3000 - 1024));
        let mut buf = vec![Complex32::default(); 1000];
        assert_eq!(overrun(r.read(&mut buf)), (1976, 0, 1976));
        let mut total = 0;
        let mut expect = 1976;
        while let ReadOutcome::Data(c) = r.read(&mut buf) {
            assert_eq!(c.first_sample(), expect);
            assert_eq!(decode(buf[0]), expect);
            expect += c.len as u64;
            total += c.len as u64;
        }
        assert_eq!(total, 1024);
        assert_eq!(r.samples_read() + r.lost_samples(), 3000);
    }

    #[test]
    fn overrun_separates_lost_samples_from_source_gaps() {
        // Blocks of 100 with a 1000-sample source gap before block 5; the reader is lapped
        // across the gap. Regression: the gap was counted both in the overrun and again in the
        // resumed chunk's `dropped_before`.
        let (mut w, ring) = ring(512, 64);
        let p = provenance(100e6);
        let mut r = ring.reader().with_resync_policy(ResyncPolicy::Latest);
        let mut first = 0;
        for b in 0..10 {
            if b == 5 {
                first += 1000;
            }
            push(&mut w, first, 100, &p);
            first += 100;
        }
        let mut buf = vec![Complex32::default(); 512];
        // Span [0, 1900): 900 samples lost, 1000 gap indices.
        assert_eq!(overrun(r.read(&mut buf)), (900, 1000, 1900));
        let c = data(r.read(&mut buf));
        assert_eq!((c.first_sample(), c.dropped_before), (1900, 0));
        assert!(matches!(r.read(&mut buf), ReadOutcome::Empty));
        assert_eq!(
            r.samples_read() + r.lost_samples() + r.gap_samples(),
            r.position()
        );
        assert_eq!((r.lost_samples(), r.gap_samples()), (900, 1000));
    }

    #[test]
    fn metadata_ring_lap_is_accounted_exactly() {
        // 4 metadata records but samples for 1024: history is limited by metadata. Regression:
        // readers accepted records of unwritten blocks and skipped forward silently.
        let (mut w, ring) = ring(1024, 4);
        let p = provenance(100e6);
        let mut r = ring.reader().with_resync_policy(ResyncPolicy::Oldest);
        for b in 0..20 {
            push(&mut w, b * 10, 10, &p);
        }
        let mut buf = vec![Complex32::default(); 1000];
        assert_eq!(overrun(r.read(&mut buf)), (160, 0, 160));
        let mut expect = 160;
        while let ReadOutcome::Data(c) = r.read(&mut buf) {
            assert_eq!(c.first_sample(), expect);
            for (k, s) in buf[..c.len].iter().enumerate() {
                assert_eq!(decode(*s), expect + k as u64);
            }
            expect = c.end_sample();
        }
        assert_eq!(expect, 200);
        assert_eq!(r.samples_read() + r.lost_samples(), 200);
    }

    #[test]
    fn metadata_lap_mid_block_counts_unknown_span_as_lost() {
        // The reader is mid-block when block records are overwritten; samples survive, but the
        // block boundaries in between are unknown, so the span is reported, never skipped.
        let (mut w, ring) = ring(1 << 12, 2);
        let p = provenance(100e6);
        let mut r = ring.reader();
        push(&mut w, 0, 100, &p);
        let mut buf = vec![Complex32::default(); 40];
        assert_eq!(data(r.read(&mut buf)).len, 40);
        push(&mut w, 100, 100, &p);
        push(&mut w, 200, 100, &p);
        push(&mut w, 300, 100, &p);
        assert_eq!(overrun(r.read(&mut buf)), (260, 0, 300));
        assert_eq!(r.samples_read() + r.lost_samples(), r.position());
    }

    #[test]
    fn startup_offset_beyond_capacity_is_not_an_overrun() {
        // Regression: a reader attached to an empty ring started at 0 and reported
        // Overrun { 5_000_000 } when the stream started at a large core:global_index.
        let (mut w, ring) = ring(1024, 16);
        let p = provenance(100e6);
        let mut live = ring.reader();
        let mut from_zero = ring.reader_at(0);
        push(&mut w, 5_000_000, 100, &p);
        push(&mut w, 5_000_100, 100, &p);
        let mut buf = vec![Complex32::default(); 1000];
        for r in [&mut live, &mut from_zero] {
            let c = data(r.read(&mut buf));
            assert_eq!((c.first_sample(), c.dropped_before), (5_000_000, 0));
            assert_eq!(decode(buf[0]), 5_000_000);
            assert_eq!((r.lost_samples(), r.gap_samples(), r.overruns()), (0, 0, 0));
        }
        assert_eq!(ring.stream_start(), Some(5_000_000));
    }

    #[test]
    fn startup_offset_below_capacity_is_not_silent_loss() {
        let (mut w, ring) = ring(1024, 16);
        let p = provenance(100e6);
        let mut r = ring.reader();
        push(&mut w, 300, 10, &p);
        let mut buf = vec![Complex32::default(); 64];
        let c = data(r.read(&mut buf));
        assert_eq!((c.first_sample(), c.dropped_before), (300, 0));
        assert_eq!(r.gap_samples() + r.lost_samples(), 0);
    }

    #[test]
    fn index_overflow_is_an_error_and_does_not_wedge_the_ring() {
        let (mut w, ring) = ring(1024, 16);
        let p = provenance(100e6);
        let mut r = ring.reader();
        let first = u64::MAX - 10;
        assert_eq!(
            w.push(&header(first, &p), &counter(0, 20)),
            Err(RingError::IndexOverflow { first, len: 20 })
        );
        // Ending exactly at u64::MAX is representable.
        push(&mut w, u64::MAX - 20, 20, &p);
        let mut buf = vec![Complex32::default(); 64];
        let c = data(r.read(&mut buf));
        assert_eq!((c.first_sample(), c.len), (u64::MAX - 20, 20));
        assert_eq!(decode(buf[19]), u64::MAX - 1);
        assert_eq!(
            w.push(&header(u64::MAX, &p), &counter(0, 1)),
            Err(RingError::IndexOverflow {
                first: u64::MAX,
                len: 1
            })
        );
    }

    #[test]
    fn provenance_changes_are_resolved_per_block() {
        let (mut w, ring) = ring(256, 4);
        let mut r = ring.reader();
        let mut buf = vec![Complex32::default(); 256];
        let handles: Vec<_> = (0..100u64).map(|b| provenance(100e6 + b as f64)).collect();
        // Every block gets a new provenance, so table slots are reused many times.
        for (b, p) in handles.iter().enumerate() {
            let b = b as u64;
            w.push(&header(b * 10, p), &counter(b * 10, 10)).unwrap();
            let c = data(r.read(&mut buf));
            assert_eq!(&c.provenance, p);
        }
        assert!(w.live_provenance.len() <= ring.block_capacity() + 1);
        assert!(w.slot_in_use.iter().filter(|u| **u).count() <= ring.block_capacity() + 1);
    }

    #[test]
    fn lagging_reader_never_gets_a_reused_provenance_slot() {
        let (mut w, ring) = ring(1 << 12, 2);
        let mut r = ring.reader().with_resync_policy(ResyncPolicy::Oldest);
        let handles: Vec<_> = (0..40u64).map(|b| provenance(1e6 * b as f64)).collect();
        for (b, p) in handles.iter().enumerate() {
            let b = b as u64;
            w.push(&header(b * 10, p), &counter(b * 10, 10)).unwrap();
        }
        let mut buf = vec![Complex32::default(); 64];
        loop {
            match r.read(&mut buf) {
                ReadOutcome::Data(c) => {
                    let block = (c.first_sample() / 10) as usize;
                    assert_eq!(c.provenance, handles[block]);
                }
                ReadOutcome::Overrun { .. } => {}
                ReadOutcome::Empty | ReadOutcome::Closed => break,
            }
        }
        assert_eq!(r.position(), 400);
    }

    #[test]
    fn reader_at_old_history_reports_overrun_then_reads() {
        let (mut w, ring) = ring(256, 64);
        let p = provenance(100e6);
        for b in 0..10 {
            push(&mut w, b * 50, 50, &p);
        }
        let mut r = ring.reader_at(0).with_resync_policy(ResyncPolicy::Oldest);
        let mut buf = vec![Complex32::default(); 256];
        assert_eq!(overrun(r.read(&mut buf)), (244, 0, 244));
        let c = data(r.read(&mut buf));
        assert_eq!(c.first_sample(), 244);
        let mut r = ring.reader_at(460);
        let c = data(r.read(&mut buf));
        assert_eq!((c.first_sample(), c.len, c.block_start), (460, 40, false));
    }

    #[test]
    fn reader_at_inside_a_gap_or_in_the_future() {
        let (mut w, ring) = ring(1024, 16);
        let p = provenance(100e6);
        push(&mut w, 0, 100, &p);
        push(&mut w, 150, 100, &p);
        let mut buf = vec![Complex32::default(); 256];
        // Inside the gap [100, 150): only the rest of the gap is counted.
        let mut r = ring.reader_at(120);
        let c = data(r.read(&mut buf));
        assert_eq!((c.first_sample(), c.dropped_before), (150, 30));
        // In the future: waits, then starts mid-block.
        let mut r = ring.reader_at(300);
        assert!(matches!(r.read(&mut buf), ReadOutcome::Empty));
        push(&mut w, 250, 100, &p);
        let c = data(r.read(&mut buf));
        assert_eq!((c.first_sample(), c.len, c.dropped_before), (300, 50, 0));
    }

    #[test]
    fn read_timeout_with_empty_buffer_returns_immediately() {
        let (mut w, ring) = ring(1024, 16);
        let p = provenance(100e6);
        let mut r = ring.reader();
        push(&mut w, 0, 10, &p);
        let t = Instant::now();
        assert!(matches!(
            r.read_timeout(&mut [], Duration::from_secs(5)),
            ReadOutcome::Empty
        ));
        assert!(t.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn i8_samples_round_trip() {
        let (mut w, ring) = ring_buffer::<Complex<i8>>(RingConfig {
            sample_capacity: 16,
            block_capacity: 4,
        });
        let p = provenance(1e6);
        let mut r = ring.reader();
        let samples = [Complex::new(-128i8, 127i8), Complex::new(0, -1)];
        w.push(&header(0, &p), &samples).unwrap();
        let mut buf = [Complex::default(); 4];
        let c = data(r.read(&mut buf));
        assert_eq!(&buf[..c.len], &samples);
    }

    #[test]
    fn config_for_duration() {
        let c = RingConfig::for_duration(20e6, 30.0, 65_536);
        assert_eq!(c.sample_capacity, 600_000_000);
        assert_eq!(c.block_capacity, 9156);
        // Exact sizing: 1.2 GB for 30 s of ci8, not the 2.15 GB of power-of-two rounding.
        assert_eq!(c.sample_bytes::<Complex<i8>>(), 600_000_000 * 2);
    }
}
