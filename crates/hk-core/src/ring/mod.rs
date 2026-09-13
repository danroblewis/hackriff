//! RAM ring buffer (C03, ADR-0001 always-on core, ADR-0006 pre-trigger): one writer, any number
//! of independent readers attached and detached at runtime, and pre-trigger extraction by
//! sample index.
//!
//! # Design
//!
//! - **Sample-indexed.** Position `i` in the ring *is* stream sample index `i` (the source's
//!   monotonic counter), stored at physical slot `i mod capacity` (a power of two). A source gap
//!   is an index jump: nothing is written for missing samples, and the block metadata records
//!   the gap, so a pre-trigger window `[t − pre, t + post)` maps straight onto slots and a gap is
//!   never spliced.
//! - **Block metadata ring.** A second, smaller ring holds one record per block: first sample,
//!   length, host time, discontinuity flags, dropped-before count and a provenance key. A reader
//!   reports metadata changes exactly at block boundaries.
//! - **Concurrency: a seqlock by sample index, lock-free on the hot path.**
//!   1. The writer first publishes `write_end = first + n`.
//!   2. It then stores the samples.
//!   3. Then it commits the block.
//!
//!   A reader copies `[a, b)` optimistically, then re-reads `write_end`. The copy is valid iff
//!   `a >= write_end − capacity`, because any overwrite that could have raced the copy must
//!   first have raised `write_end`. The metadata ring uses the same protocol per record. The
//!   writer never waits for a reader, never retries, and readers need no registration: attach
//!   = create a cursor, detach = drop it.
//!
//!   Sample cells are atomics (`AtomicU64` holds the two `f32` bit patterns of a `Complex32`),
//!   so a racing copy is merely stale, never undefined behaviour, and no `unsafe` is needed.
//!   `load`/`store` compile to plain moves.
//! - **Overrun.** A reader whose cursor falls behind `write_end − capacity` gets
//!   [`ReadOutcome::Overrun`] with the exact index distance skipped, then resyncs per
//!   [`ResyncPolicy`] (default: jump to the newest block, which suits real-time consumers).
//! - **Allocation.** The rings are allocated once. Per block, the writer does atomic stores and
//!   a `memcpy`-like loop, and readers copy into a caller-provided slice and take an `Arc`
//!   clone of the provenance. The only lock is the provenance table, touched when the provenance
//!   *changes* (a retune or gain step, not per block): the writer holds it for a push, a reader
//!   for a binary search. Readers cache the handle.
//! - **Waking readers.** The writer notifies a condvar only if a reader is parked, and uses
//!   `try_lock`, so it cannot block. The rare missed wake-up is bounded by the reader's
//!   2 ms wait slice.
//!
//! Sample types implement [`RingSample`]: `Complex32` (8 bytes/sample; 160 MB/s at 20 Msps) and
//! `Complex<i8>` (2 bytes/sample; 40 MB/s, the C03 recommendation for long pre-trigger rings on
//! the 8 GB Jetson).

pub mod extract;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU16, AtomicU32, AtomicU64, AtomicUsize};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use hk_model::{SampleTime, Timestamp};
use num_complex::{Complex, Complex32};

use crate::block::{BlockHeader, Discontinuity, ProvenanceHandle, SampleBlock};

pub use extract::{
    CaptureSegment, CaptureStatus, CapturedWindow, PreTriggerCapture, TriggerWindow,
};

/// Ordering for sample and metadata cells (validated by the seqlock check, not by ordering).
const DATA: std::sync::atomic::Ordering = std::sync::atomic::Ordering::Relaxed;
/// Ordering for control positions.
const CTRL: std::sync::atomic::Ordering = std::sync::atomic::Ordering::SeqCst;
/// Longest single condvar wait; bounds the latency of a wake-up missed by the writer's `try_lock`.
const WAIT_SLICE: Duration = Duration::from_millis(2);

/// A sample type storable in the ring: each sample lives in one atomic cell.
pub trait RingSample: Copy + Default + Send + Sync + 'static {
    /// The atomic cell type.
    type Cell: Send + Sync;
    /// A zeroed cell.
    fn new_cell() -> Self::Cell;
    /// Reads a sample.
    fn load(cell: &Self::Cell) -> Self;
    /// Writes a sample.
    fn store(cell: &Self::Cell, value: Self);
}

impl RingSample for Complex32 {
    type Cell = AtomicU64;

    fn new_cell() -> AtomicU64 {
        AtomicU64::new(0)
    }

    #[inline]
    fn load(cell: &AtomicU64) -> Self {
        let bits = cell.load(DATA);
        Complex32::new(
            f32::from_bits((bits >> 32) as u32),
            f32::from_bits(bits as u32),
        )
    }

    #[inline]
    fn store(cell: &AtomicU64, value: Self) {
        cell.store(
            (u64::from(value.re.to_bits()) << 32) | u64::from(value.im.to_bits()),
            DATA,
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
        let bits = cell.load(DATA);
        Complex::new((bits >> 8) as u8 as i8, bits as u8 as i8)
    }

    #[inline]
    fn store(cell: &AtomicU16, value: Self) {
        cell.store(
            (u16::from(value.re as u8) << 8) | u16::from(value.im as u8),
            DATA,
        );
    }
}

/// Ring sizes. Both are rounded up to a power of two.
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

    /// Bytes of sample storage after rounding.
    pub fn sample_bytes<T: RingSample>(&self) -> usize {
        self.sample_capacity.max(1).next_power_of_two() * size_of::<T::Cell>()
    }
}

/// Errors pushing into the ring.
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
}

struct MetaSlot {
    first_sample: AtomicU64,
    len: AtomicU64,
    host_time_ns: AtomicI64,
    dropped_before: AtomicU64,
    flags: AtomicU32,
    provenance_key: AtomicU64,
}

#[derive(Clone, Copy, Debug)]
struct BlockMeta {
    first_sample: u64,
    len: u64,
    host_time: Timestamp,
    dropped_before: u64,
    flags: Discontinuity,
    provenance_key: u64,
}

impl BlockMeta {
    fn end(&self) -> u64 {
        self.first_sample + self.len
    }
}

struct Shared<T: RingSample> {
    cells: Box<[T::Cell]>,
    sample_mask: u64,
    meta: Box<[MetaSlot]>,
    meta_mask: u64,
    /// End (exclusive) of the newest sample write, published *before* the samples are stored.
    sample_write_end: AtomicU64,
    /// Blocks whose metadata write has started, published *before* the record is stored.
    meta_write_end: AtomicU64,
    /// Blocks fully committed (samples and metadata).
    meta_head: AtomicU64,
    /// `(key, handle)` sorted by key; covers every block still in the metadata ring.
    provenance: Mutex<VecDeque<(u64, ProvenanceHandle)>>,
    closed: AtomicBool,
    waiters: AtomicUsize,
    wake_lock: Mutex<()>,
    wake: Condvar,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<T: RingSample> Shared<T> {
    fn sample_capacity(&self) -> u64 {
        self.sample_mask + 1
    }

    fn meta_capacity(&self) -> u64 {
        self.meta_mask + 1
    }

    /// Samples below this index may have been overwritten.
    fn oldest_valid_sample(&self) -> u64 {
        self.sample_write_end
            .load(CTRL)
            .saturating_sub(self.sample_capacity())
    }

    /// Reads a committed block's metadata; `None` if it has been (or is being) overwritten.
    fn read_meta(&self, block: u64) -> Option<BlockMeta> {
        let slot = &self.meta[(block & self.meta_mask) as usize];
        let meta = BlockMeta {
            first_sample: slot.first_sample.load(DATA),
            len: slot.len.load(DATA),
            host_time: Timestamp::from_unix_nanos(slot.host_time_ns.load(DATA)),
            dropped_before: slot.dropped_before.load(DATA),
            flags: Discontinuity::from_bits_truncate(slot.flags.load(DATA) as u8),
            provenance_key: slot.provenance_key.load(DATA),
        };
        let oldest = self
            .meta_write_end
            .load(CTRL)
            .saturating_sub(self.meta_capacity());
        (block >= oldest).then_some(meta)
    }

    /// Copies samples `[first, first + out.len())`; `false` if any may have been overwritten.
    fn copy_out(&self, first: u64, out: &mut [T]) -> bool {
        let n = out.len();
        let phys = (first & self.sample_mask) as usize;
        let run = n.min(self.cells.len() - phys);
        let (head, tail) = out.split_at_mut(run);
        for (dst, cell) in head.iter_mut().zip(&self.cells[phys..phys + run]) {
            *dst = T::load(cell);
        }
        for (dst, cell) in tail.iter_mut().zip(&self.cells[..n - run]) {
            *dst = T::load(cell);
        }
        first >= self.oldest_valid_sample()
    }

    fn copy_in(&self, first: u64, samples: &[T]) {
        let n = samples.len();
        let phys = (first & self.sample_mask) as usize;
        let run = n.min(self.cells.len() - phys);
        let (head, tail) = samples.split_at(run);
        for (src, cell) in head.iter().zip(&self.cells[phys..phys + run]) {
            T::store(cell, *src);
        }
        for (src, cell) in tail.iter().zip(&self.cells[..n - run]) {
            T::store(cell, *src);
        }
    }

    fn provenance(&self, key: u64) -> Option<ProvenanceHandle> {
        let table = lock(&self.provenance);
        table
            .binary_search_by_key(&key, |(k, _)| *k)
            .ok()
            .map(|i| table[i].1.clone())
    }

    /// The smallest committed block whose end is after `sample` (with its metadata), or the
    /// head block index and `None` if `sample` is at or beyond the newest block's end.
    fn locate(&self, sample: u64) -> (u64, Option<BlockMeta>) {
        'retry: loop {
            let head = self.meta_head.load(CTRL);
            let lo = self
                .meta_write_end
                .load(CTRL)
                .saturating_sub(self.meta_capacity())
                .min(head);
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
    let sample_capacity = config.sample_capacity.max(1).next_power_of_two();
    let block_capacity = config.block_capacity.max(1).next_power_of_two();
    let shared = Arc::new(Shared {
        cells: (0..sample_capacity).map(|_| T::new_cell()).collect(),
        sample_mask: sample_capacity as u64 - 1,
        meta: (0..block_capacity)
            .map(|_| MetaSlot {
                first_sample: AtomicU64::new(0),
                len: AtomicU64::new(0),
                host_time_ns: AtomicI64::new(0),
                dropped_before: AtomicU64::new(0),
                flags: AtomicU32::new(0),
                provenance_key: AtomicU64::new(0),
            })
            .collect(),
        meta_mask: block_capacity as u64 - 1,
        sample_write_end: AtomicU64::new(0),
        meta_write_end: AtomicU64::new(0),
        meta_head: AtomicU64::new(0),
        provenance: Mutex::new(VecDeque::with_capacity(64)),
        closed: AtomicBool::new(false),
        waiters: AtomicUsize::new(0),
        wake_lock: Mutex::new(()),
        wake: Condvar::new(),
    });
    (
        RingWriter {
            shared: Arc::clone(&shared),
            next_block: 0,
            stream_end: None,
            next_provenance_key: 0,
            current_provenance: None,
            provenance_refs: VecDeque::with_capacity(64),
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
    next_provenance_key: u64,
    current_provenance: Option<(u64, ProvenanceHandle)>,
    /// `(provenance key, first block using it)`, oldest first; mirrors the shared table.
    provenance_refs: VecDeque<(u64, u64)>,
}

impl<T: RingSample> RingWriter<T> {
    /// Appends a block. Never blocks on readers.
    ///
    /// `header.first_sample()` must not precede the previous block's end. A later start is a
    /// gap: the stored record gets [`Discontinuity::GAP`] and `dropped_before` from the counter
    /// jump.
    pub fn push(&mut self, header: &BlockHeader, samples: &[T]) -> Result<(), RingError> {
        let shared = &*self.shared;
        let n = samples.len() as u64;
        if n == 0 {
            return Err(RingError::EmptyBlock);
        }
        if n > shared.sample_capacity() {
            return Err(RingError::BlockTooLarge {
                len: samples.len(),
                capacity: shared.sample_capacity() as usize,
            });
        }
        let first = header.first_sample();
        let mut flags = header.discontinuity;
        let dropped_before = match self.stream_end {
            Some(end) if first < end => {
                return Err(RingError::NonMonotonic {
                    expected_at_least: end,
                    got: first,
                });
            }
            Some(end) => first - end,
            None => header.dropped_before,
        };
        if dropped_before > 0 {
            flags |= Discontinuity::GAP;
        }

        let block = self.next_block;
        let provenance_key = self.provenance_key_for(&header.provenance, block);
        let shared = &*self.shared;

        shared.meta_write_end.store(block + 1, CTRL);
        let slot = &shared.meta[(block & shared.meta_mask) as usize];
        slot.first_sample.store(first, DATA);
        slot.len.store(n, DATA);
        slot.host_time_ns
            .store(header.time.host_time.as_unix_nanos(), DATA);
        slot.dropped_before.store(dropped_before, DATA);
        slot.flags.store(u32::from(flags.bits()), DATA);
        slot.provenance_key.store(provenance_key, DATA);

        shared.sample_write_end.store(first + n, CTRL);
        shared.copy_in(first, samples);

        shared.meta_head.store(block + 1, CTRL);
        self.next_block = block + 1;
        self.stream_end = Some(first + n);

        if shared.waiters.load(CTRL) > 0 {
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

    fn provenance_key_for(&mut self, handle: &ProvenanceHandle, block: u64) -> u64 {
        if let Some((key, current)) = &self.current_provenance {
            if current == handle {
                return *key;
            }
        }
        let key = self.next_provenance_key;
        self.next_provenance_key += 1;
        // Blocks older than this are gone from the metadata ring once `block` is written.
        let oldest_block = (block + 1).saturating_sub(self.shared.meta_capacity());
        {
            let mut table = lock(&self.shared.provenance);
            while self.provenance_refs.len() >= 2 && self.provenance_refs[1].1 <= oldest_block {
                self.provenance_refs.pop_front();
                table.pop_front();
            }
            table.push_back((key, handle.clone()));
        }
        self.provenance_refs.push_back((key, block));
        self.current_provenance = Some((key, handle.clone()));
        key
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
        self.shared.closed.store(true, CTRL);
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
    /// Samples retained (after rounding to a power of two).
    pub fn sample_capacity(&self) -> usize {
        self.shared.sample_capacity() as usize
    }

    /// Block metadata records retained (after rounding).
    pub fn block_capacity(&self) -> usize {
        self.shared.meta_capacity() as usize
    }

    /// Blocks committed so far.
    pub fn blocks_written(&self) -> u64 {
        self.shared.meta_head.load(CTRL)
    }

    /// Stream sample index one past the newest committed block, if any.
    pub fn next_sample(&self) -> Option<u64> {
        loop {
            let head = self.shared.meta_head.load(CTRL);
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
        self.shared.closed.load(CTRL)
    }

    /// A reader starting at the live edge: it sees the next block written.
    pub fn reader(&self) -> RingReader<T> {
        let shared = &*self.shared;
        let (next_sample, next_block) = loop {
            let head = shared.meta_head.load(CTRL);
            if head == 0 {
                break (0, 0);
            }
            if let Some(m) = shared.read_meta(head - 1) {
                break (m.end(), head);
            }
        };
        RingReader::new(Arc::clone(&self.shared), next_sample, next_block, None)
    }

    /// A reader starting at stream sample `sample` (e.g. a pre-trigger position). If that
    /// history has already been overwritten, the first read reports [`ReadOutcome::Overrun`].
    pub fn reader_at(&self, sample: u64) -> RingReader<T> {
        let shared = &*self.shared;
        let (block, meta) = shared.locate(sample);
        let mut pending = None;
        let mut position = sample;
        if let Some(m) = meta {
            if sample < m.first_sample {
                // `sample` lies before block `block`. That is a known gap (or before the stream
                // started) only if the previous block's metadata is still retained.
                let known = block == 0 || shared.read_meta(block - 1).is_some();
                if !known {
                    pending = Some(m.first_sample - sample);
                    position = m.first_sample;
                }
            }
        }
        if position < shared.oldest_valid_sample() && meta.is_some() {
            // Samples gone even though metadata survives: report on first read.
            pending.get_or_insert(0);
        }
        RingReader::new(Arc::clone(&self.shared), position, block, pending)
    }

    /// Starts a pre-trigger capture of `window`; see [`PreTriggerCapture`].
    pub fn pre_trigger(&self, window: TriggerWindow) -> PreTriggerCapture<T> {
        PreTriggerCapture::new(self, window)
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
    /// Samples the source dropped before the block (only when `block_start`).
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
    /// The reader was lapped: `dropped_samples` stream indices were skipped, and reading
    /// resumes at `resume_at`.
    Overrun {
        /// Index distance skipped (includes any source gaps inside the skipped span).
        dropped_samples: u64,
        /// Stream index the next read starts from.
        resume_at: u64,
    },
    /// No new data yet.
    Empty,
    /// The writer is gone and everything has been read.
    Closed,
}

/// An independent cursor over the ring. Attach with [`RingHandle::reader`] or
/// [`RingHandle::reader_at`]; detach by dropping.
pub struct RingReader<T: RingSample> {
    shared: Arc<Shared<T>>,
    next_sample: u64,
    next_block: u64,
    policy: ResyncPolicy,
    pending_overrun: Option<u64>,
    provenance_cache: Option<(u64, ProvenanceHandle)>,
    samples_read: u64,
    dropped_samples: u64,
    overruns: u64,
}

impl<T: RingSample> RingReader<T> {
    fn new(
        shared: Arc<Shared<T>>,
        next_sample: u64,
        next_block: u64,
        pending_overrun: Option<u64>,
    ) -> Self {
        Self {
            shared,
            next_sample,
            next_block,
            policy: ResyncPolicy::default(),
            pending_overrun,
            provenance_cache: None,
            samples_read: 0,
            dropped_samples: 0,
            overruns: 0,
        }
    }

    /// Sets the resync policy.
    pub fn with_resync_policy(mut self, policy: ResyncPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Stream index of the next sample this reader will return.
    pub fn position(&self) -> u64 {
        self.next_sample
    }

    /// Samples returned so far.
    pub fn samples_read(&self) -> u64 {
        self.samples_read
    }

    /// Total index distance skipped by overruns.
    pub fn dropped_samples(&self) -> u64 {
        self.dropped_samples
    }

    /// Number of overruns reported.
    pub fn overruns(&self) -> u64 {
        self.overruns
    }

    /// Copies the next available samples into `out` without waiting. A chunk never spans a
    /// block boundary.
    pub fn read(&mut self, out: &mut [T]) -> ReadOutcome {
        self.read_until(out, u64::MAX)
    }

    /// Like [`RingReader::read`], waiting up to `timeout` for data.
    pub fn read_timeout(&mut self, out: &mut [T], timeout: Duration) -> ReadOutcome {
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
        if let Some(dropped) = self.pending_overrun.take() {
            if dropped > 0 || self.next_sample >= self.shared.oldest_valid_sample() {
                return self.report_overrun(dropped, self.next_block);
            }
            return self.resync();
        }
        if out.is_empty() || self.next_sample >= limit {
            return ReadOutcome::Empty;
        }
        let shared = Arc::clone(&self.shared);
        loop {
            let closed = shared.closed.load(CTRL);
            let head = shared.meta_head.load(CTRL);
            if self.next_block >= head {
                return if closed {
                    ReadOutcome::Closed
                } else {
                    ReadOutcome::Empty
                };
            }
            if self.next_sample < shared.oldest_valid_sample() {
                return self.resync();
            }
            let Some(meta) = shared.read_meta(self.next_block) else {
                // Metadata lapped (samples may survive): relocate, or report if unknowable.
                let (block, m) = shared.locate(self.next_sample);
                match m {
                    Some(m) if m.first_sample <= self.next_sample => {
                        self.next_block = block;
                        continue;
                    }
                    _ => return self.resync(),
                }
            };
            if self.next_sample >= meta.end() {
                self.next_block += 1;
                continue;
            }
            let block_start = self.next_sample <= meta.first_sample;
            if block_start {
                // Skips a source gap (recorded in the block's metadata) or a pre-stream position.
                self.next_sample = meta.first_sample;
            }
            if self.next_sample >= limit {
                return ReadOutcome::Empty;
            }
            let n = (meta.end() - self.next_sample)
                .min(limit - self.next_sample)
                .min(out.len() as u64) as usize;
            let out = &mut out[..n];
            if !shared.copy_out(self.next_sample, out) {
                return self.resync();
            }
            let Some(provenance) = self.resolve_provenance(meta.provenance_key) else {
                return self.resync();
            };
            let anchor = SampleTime {
                sample_index: meta.first_sample,
                host_time: meta.host_time,
            };
            let chunk = ReadChunk {
                time: SampleTime {
                    sample_index: self.next_sample,
                    host_time: anchor.time_of(self.next_sample, provenance.tune.sample_rate_hz),
                },
                len: n,
                block_start,
                discontinuity: if block_start {
                    meta.flags
                } else {
                    Discontinuity::NONE
                },
                dropped_before: if block_start { meta.dropped_before } else { 0 },
                provenance,
            };
            self.next_sample += n as u64;
            if self.next_sample == meta.end() {
                self.next_block += 1;
            }
            self.samples_read += n as u64;
            return ReadOutcome::Data(chunk);
        }
    }

    /// Parks until the ring may have new data or `deadline` passes. `false` once past deadline.
    pub(crate) fn wait_for_data(&self, deadline: Instant) -> bool {
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let shared = &*self.shared;
        shared.waiters.fetch_add(1, CTRL);
        {
            let guard = lock(&shared.wake_lock);
            if shared.meta_head.load(CTRL) <= self.next_block && !shared.closed.load(CTRL) {
                let slice = (deadline - now).min(WAIT_SLICE);
                drop(
                    shared
                        .wake
                        .wait_timeout(guard, slice)
                        .unwrap_or_else(PoisonError::into_inner),
                );
            }
        }
        shared.waiters.fetch_sub(1, CTRL);
        true
    }

    fn resolve_provenance(&mut self, key: u64) -> Option<ProvenanceHandle> {
        if let Some((cached, handle)) = &self.provenance_cache {
            if *cached == key {
                return Some(handle.clone());
            }
        }
        let handle = self.shared.provenance(key)?;
        self.provenance_cache = Some((key, handle.clone()));
        Some(handle)
    }

    /// Moves the cursor past lost data according to the policy and reports the overrun.
    fn resync(&mut self) -> ReadOutcome {
        let shared = Arc::clone(&self.shared);
        let (block, target) = match self.policy {
            ResyncPolicy::Latest => loop {
                let head = shared.meta_head.load(CTRL);
                if head == 0 {
                    break (0, self.next_sample);
                }
                if let Some(m) = shared.read_meta(head - 1) {
                    break (head - 1, m.first_sample);
                }
            },
            ResyncPolicy::Oldest => {
                let oldest = shared.oldest_valid_sample().max(self.next_sample);
                match shared.locate(oldest) {
                    (block, Some(m)) => (block, m.first_sample.max(oldest)),
                    (block, None) => (block, oldest),
                }
            }
        };
        let target = target.max(self.next_sample);
        let dropped = target - self.next_sample;
        self.next_sample = target;
        self.report_overrun(dropped, block)
    }

    fn report_overrun(&mut self, dropped: u64, block: u64) -> ReadOutcome {
        self.next_block = block;
        self.dropped_samples += dropped;
        self.overruns += 1;
        ReadOutcome::Overrun {
            dropped_samples: dropped,
            resume_at: self.next_sample,
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
            clock_source: ClockSource::Internal,
            clock_locked: true,
            calibration_state_ref: None,
            spur_mask_ref: None,
            timestamp_method: TimestampMethod::Synthetic,
            timestamp_error_budget_ns: Some(0),
        })
    }

    fn header(first: u64, prov: &ProvenanceHandle) -> BlockHeader {
        BlockHeader {
            time: SampleTime {
                sample_index: first,
                host_time: Timestamp::from_unix_nanos(first as i64 * 1000),
            },
            provenance: prov.clone(),
            discontinuity: Discontinuity::NONE,
            dropped_before: 0,
        }
    }

    /// Samples whose value encodes their stream index.
    fn counter(first: u64, n: usize) -> Vec<Complex32> {
        (0..n as u64)
            .map(|i| Complex32::new((first + i) as f32, -((first + i) as f32)))
            .collect()
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

    #[test]
    fn readers_see_contiguous_blocks_with_metadata() {
        let (mut w, ring) = ring_buffer::<Complex32>(RingConfig {
            sample_capacity: 1024,
            block_capacity: 16,
        });
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

    #[test]
    fn gaps_are_index_jumps_and_flagged() {
        let (mut w, ring) = ring_buffer::<Complex32>(RingConfig {
            sample_capacity: 1024,
            block_capacity: 16,
        });
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
        assert_eq!(buf[0], Complex32::new(25.0, -25.0));
        assert!(matches!(
            w.push(&header(30, &p), &counter(30, 1)),
            Err(RingError::NonMonotonic { .. })
        ));
    }

    #[test]
    fn overrun_latest_policy_counts_and_resyncs() {
        let (mut w, ring) = ring_buffer::<Complex32>(RingConfig {
            sample_capacity: 1024,
            block_capacity: 64,
        });
        let p = provenance(100e6);
        let mut r = ring.reader();
        for b in 0..30 {
            push(&mut w, b * 100, 100, &p);
        }
        let mut buf = vec![Complex32::default(); 1000];
        match r.read(&mut buf) {
            ReadOutcome::Overrun {
                dropped_samples,
                resume_at,
            } => assert_eq!((dropped_samples, resume_at), (2900, 2900)),
            other => panic!("expected overrun, got {other:?}"),
        }
        let c = data(r.read(&mut buf));
        assert_eq!((c.first_sample(), c.len), (2900, 100));
        assert_eq!(buf[0], Complex32::new(2900.0, -2900.0));
        assert_eq!(r.samples_read() + r.dropped_samples(), 3000);
        assert_eq!(r.overruns(), 1);
    }

    #[test]
    fn overrun_oldest_policy_resumes_at_oldest_retained_sample() {
        let (mut w, ring) = ring_buffer::<Complex32>(RingConfig {
            sample_capacity: 1024,
            block_capacity: 64,
        });
        let p = provenance(100e6);
        let mut r = ring.reader().with_resync_policy(ResyncPolicy::Oldest);
        for b in 0..30 {
            push(&mut w, b * 100, 100, &p);
        }
        assert_eq!(ring.oldest_sample(), Some(3000 - 1024));
        let mut buf = vec![Complex32::default(); 1000];
        match r.read(&mut buf) {
            ReadOutcome::Overrun {
                dropped_samples,
                resume_at,
            } => assert_eq!((dropped_samples, resume_at), (1976, 1976)),
            other => panic!("expected overrun, got {other:?}"),
        }
        let mut total = 0;
        let mut expect = 1976;
        while let ReadOutcome::Data(c) = r.read(&mut buf) {
            assert_eq!(c.first_sample(), expect);
            assert_eq!(buf[0], Complex32::new(expect as f32, -(expect as f32)));
            expect += c.len as u64;
            total += c.len as u64;
        }
        assert_eq!(total, 1024);
        assert_eq!(r.samples_read() + r.dropped_samples(), 3000);
    }

    #[test]
    fn metadata_ring_smaller_than_sample_history_is_accounted() {
        // 8 metadata records but samples for 1024: history is limited by metadata.
        let (mut w, ring) = ring_buffer::<Complex32>(RingConfig {
            sample_capacity: 1024,
            block_capacity: 8,
        });
        let p = provenance(100e6);
        let mut r = ring.reader().with_resync_policy(ResyncPolicy::Oldest);
        for b in 0..20 {
            push(&mut w, b * 10, 10, &p);
        }
        let mut buf = vec![Complex32::default(); 1000];
        match r.read(&mut buf) {
            ReadOutcome::Overrun {
                dropped_samples, ..
            } => assert_eq!(dropped_samples, 120),
            other => panic!("expected overrun, got {other:?}"),
        }
        while let ReadOutcome::Data(_) = r.read(&mut buf) {}
        assert_eq!(r.samples_read() + r.dropped_samples(), 200);
    }

    #[test]
    fn provenance_changes_are_resolved_per_block() {
        let (mut w, ring) = ring_buffer::<Complex32>(RingConfig {
            sample_capacity: 256,
            block_capacity: 4,
        });
        let mut r = ring.reader();
        let mut buf = vec![Complex32::default(); 256];
        // Many provenance changes: the table must stay bounded and correct.
        for b in 0..100u64 {
            let p = provenance(100e6 + b as f64);
            w.push(&header(b * 10, &p), &counter(b * 10, 10)).unwrap();
            let c = data(r.read(&mut buf));
            assert_eq!(c.provenance, p);
        }
        assert!(lock(&ring.shared.provenance).len() <= ring.block_capacity() + 1);
    }

    #[test]
    fn reader_at_old_history_reports_overrun_then_reads() {
        let (mut w, ring) = ring_buffer::<Complex32>(RingConfig {
            sample_capacity: 256,
            block_capacity: 64,
        });
        let p = provenance(100e6);
        for b in 0..10 {
            push(&mut w, b * 50, 50, &p);
        }
        let mut r = ring.reader_at(0).with_resync_policy(ResyncPolicy::Oldest);
        let mut buf = vec![Complex32::default(); 256];
        match r.read(&mut buf) {
            ReadOutcome::Overrun {
                dropped_samples,
                resume_at,
            } => assert_eq!((dropped_samples, resume_at), (500 - 256, 500 - 256)),
            other => panic!("expected overrun, got {other:?}"),
        }
        let c = data(r.read(&mut buf));
        assert_eq!(c.first_sample(), 244);
        let mut r = ring.reader_at(460);
        let c = data(r.read(&mut buf));
        assert_eq!((c.first_sample(), c.len, c.block_start), (460, 40, false));
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
        assert_eq!(c.sample_bytes::<Complex<i8>>(), (1 << 30) * 2);
    }
}
