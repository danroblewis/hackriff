//! Always-on reader 2: spectrum history (C26 pyramid) and the noise-floor product (C33).
//!
//! Its own STFT (the detection bin width, Hann 50 % overlap, `K` for ~`history_rows_per_s`
//! frames/s, no SK) and its own `NoiseFloorTracker` feed `FloorProduct::ingest`, which folds
//! each frame into the dBFS/Hz pyramid (or the dBm/Hz one when a calibration applies). The
//! product's uncalibrated pyramid is the history `/api/history` answers from. At the end of the run
//! tiles are sealed through the last frame plus an hour.
//!
//! **Readers never stall this reader (T-037b).** `/api/history` and `/api/floor` hold the
//! product's lock for a whole query. Frames go through a [`FloorIngestQueue`], which only tries
//! the lock: while a query holds it, frames are queued (up to [`HISTORY_QUEUE_FRAMES`], then the
//! oldest are dropped and counted) and folded in order by the next frame that gets the lock
//! (`frames_deferred`, `frames_dropped`).
//!
//! **One floor tracker, one front end (T-377).** This reader's `NoiseFloorTracker` is *not* keyed
//! by origin, and does not need to be: it is fed only from `shared.ring`, and a ring carries the
//! blocks of exactly one source. `Pipeline::start` takes one `Box<dyn Source>`; a re-plumb (T-050)
//! hands that *same still-open device* back and starts the new segment with it, and each segment
//! builds its own reader and its own tracker. `Provenance::device_id` is a per-source constant —
//! `hackrf:<serial>`, `sigmf:<hw>` from the one recording's global metadata, `mock:<device>` — and
//! the source conformance suite's `device-info` check pins it equal to `DeviceInfo::device_id` on
//! every block, which is the value T-314 takes the run's `ChainKey` from. So `source_key(...)`
//! below is constant for the life of a tracker, and no frame of one front end can move another's
//! floor here. The tracker that *was* pooled is the pyramid's own (`Pyramid::floors`), whose
//! ingest contract explicitly admits interleaved sources; T-377 keys that one by
//! `FrameInput::source`. Should a ring ever carry two devices, this tracker must be split the same
//! way.
//!
//! **Source and site (T-133).** Every frame is folded with its origin: the source key of the run's
//! `device_id` and the site the attention service's site state machine gives at the frame's sample
//! time ([`frame_site`]; `unassigned` when the run has no attention service), so history tiles
//! record where their frames came from and `/api/history` / `/api/report` can filter by source and
//! site. **T-136:** frames run ahead of the occupancy close, so this thread only *peeks*
//! ([`AttentionService::site_at_peek`]): it never expires, clears or persists site state (that
//! would stamp the close's rows `unassigned` and block frames on a database write).
//!
//! **Short scheduler steps (T-139, ADR-0012 §2.10).** A row needs `K` segments (0.1 s at the run's
//! opening rate) of unchanged tuning, and every retune or gap resets the STFT. A scheduler sweep
//! hop (50 ms) is shorter, so without help a scheduler-driven run folded no history at all. Once
//! the stream has retuned (or changed rate), a reset emits the averaging in progress as a row when
//! it holds at least `K / 10` segments ([`hk_dsp::PartialFrames`]): its resolution's `n_avg` and
//! `sample_count` are what was actually averaged, so the pyramid's per-cell noise shape and
//! observed duration stay honest. A stream that never retunes (fixed tuning; gaps and gain steps
//! only) folds exactly the frames it did before, and a full row disarms partial rows until the
//! next retune, so a tune held after the scheduler left it discards at overrun gaps as before. Rows are one per step at most beyond the full
//! rows, so the reader's per-block cost is unchanged.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError, TryLockError};
use std::time::Duration;

use hk_core::{Discontinuity, ReadOutcome};
use hk_dsp::floor::{FloorConfig, NoiseFloorTracker};
use hk_dsp::{InputInfo, PartialFrames, SpectrumFrame, StftConfig, WelchConfig};
use hk_model::Timestamp;
use hk_model::attention::baseline::SiteKey;
use hk_store::history::{FrameOrigin, source_key};
use hk_store::{
    FloorIngest, FloorIngestQueue, FloorProduct, FrameInput, HistogramConfig, IngestOutcome,
    Pyramid, PyramidConfig, StoreError, ViewLattice,
};
use num_complex::Complex;

use crate::attention::AttentionService;
use crate::compute::Reader;
use crate::run::Shared;
use crate::stats::{HistoryCounters, add, inc, set};

/// Frames queued while a query holds the product (about a minute at 10 rows/s).
pub(crate) const HISTORY_QUEUE_FRAMES: usize = 600;

/// T-139: a partial row needs at least `K / PARTIAL_MIN_DIVISOR` segments (10 ms at 10 rows/s).
pub(crate) const PARTIAL_MIN_DIVISOR: usize = 10;

/// Updates the tile counters from the product.
pub(crate) fn update_tiles(shared: &Shared, product: &FloorProduct) {
    let h = &shared.counters.history;
    let (c, u) = (
        product.calibrated_pyramid().stats(),
        product.uncalibrated_pyramid().stats(),
    );
    set(&h.tiles_written, c.tiles_written + u.tiles_written);
    set(&h.bytes_written, c.bytes_written + u.bytes_written);
}

/// Updates the view-scheme tile counters (T-439).
pub(crate) fn update_view_tiles(shared: &Shared, view: &Pyramid) {
    let s = view.stats();
    set(&shared.counters.history.view_tiles_written, s.tiles_written);
    set(&shared.counters.history.view_bytes_written, s.bytes_written);
}

fn tally(h: &HistoryCounters, folded: &[Result<FloorIngest, StoreError>]) {
    for r in folded {
        match r {
            Ok(i) if i.outcome == IngestOutcome::Late => inc(&h.frames_late),
            Ok(_) => inc(&h.frames_ingested),
            Err(_) => inc(&h.frames_rejected),
        }
    }
}

/// The Welch settings **this reader** averages with, and the one line in the whole chain that
/// decides whether the pyramid's "max-hold" is a max-hold of measurements or a max-hold of means
/// (T-397).
///
/// # The bug this function exists to have fixed
///
/// Every layer downstream of here is an honest maximum and says so: `Tile::add_value` keeps
/// `max = max(max, pk_db)`, `Tile::fold_child` rolls parents up as max-of-max,
/// `RegionHistory::overview` folds cells with `OverviewCell::fold` (also a max), and both
/// `/api/timeline` and `/api/coverage` serve `"fold": "max-hold"` with
/// [`crate::history`]-independent prose about it. All of that was true. What was false was the
/// *input*: this reader set `holds = false`, so [`hk_dsp::Spectrum::max_hold`] came back **empty**,
/// [`hk_store::FrameInput::from_dsp`] therefore set `peak: None`, and
/// `FloorProduct::ingest`'s `let peak = frame.peak.unwrap_or(frame.psd)` quietly substituted the
/// **Welch-averaged PSD**.
///
/// A history frame averages `K = fs / (hop · history_rows_per_s)` segments — order a thousand at
/// 20 Msps and 10 rows/s — so a burst occupying one segment was attenuated by ~10·log10(K) ≈ 30 dB
/// before the first `max` ever ran. Maxing averages is not max-holding: the peak was gone. That is
/// exactly the user's report — *"yellow/red peaks not showing because averaging washes them out"* —
/// and it is why the strips could never reach the top of the colour ramp whatever ramp they used.
///
/// The fix is not a second max anywhere. It is to let the per-segment max the accumulator already
/// knows how to keep actually reach the store, through the `peak` field that has always been wired
/// for it. `psd` is untouched, so percentiles, the floor tracker and occupancy see exactly what
/// they saw before; only `max_db` changes, and it changes from a mean to the maximum it claims to
/// be.
///
/// Cost: two extra O(bins) passes per segment in [`hk_dsp::welch::Accumulators::add`], against an
/// N·log N FFT on the same segment — and the accumulation is CPU-side for every compute provider,
/// so no backend path changes. SK stays off; it is not read from this chain.
pub(crate) fn history_welch(fft_len: usize) -> WelchConfig {
    let mut welch = WelchConfig::new(fft_len);
    // ON, deliberately: `max_hold` is what the pyramid's `max_db` is a max *of*. See above.
    welch.holds = true;
    welch.spectral_kurtosis = false;
    welch
}

/// Scheme id of the view lattice. Scheme 1 is the welded ladder `/api/history` and `/api/floor`
/// answer from; this is the de-welded lattice of `docs/16` §6.2/§8.2, and the two live side by
/// side in the same data directory under their own scheme roots.
pub const VIEW_SCHEME: u16 = 2;

/// Time cell of the view lattice's node (0, 0). **Not configurable, and 1 s rather than
/// `docs/16` §6.2's 128 s** — T-437's finding F1: a 128 s finest time cell puts the whole 120 s IQ
/// retention inside *one* cell, so `level_t` pins at 0 and the de-welding buys nothing on the axis
/// it was introduced for. The floor is the decision, not the ratio.
pub const VIEW_T_CELL: Duration = Duration::from_secs(1);

/// Cells per tile on both axes of the view lattice.
///
/// **64, not `docs/16` §6.2's 256, and this is the answer to T-434's RAM caveat.** A level-0 tile
/// is an in-memory accumulator for its whole duration ([`hk_store::Pyramid`] keeps one open map per
/// level), so the block height is *residency*: §6.2's V(0, 0) holds a tile open for **9.1 h** at
/// ~3 MB, which is fine for a survey and wrong for a growing edge. At 64 × 64 the finest node's
/// tile spans **64 s** and costs **~191 KB** — 512× less residency and 17× less memory per tile —
/// and the client's tile stays 256 × 256 output cells regardless, because `/api/tiles` lays its
/// grid on the tile's own extent and reads however many store blocks that covers (T-438).
///
/// Uniformity across the lattice is what matters for §5.5's budget (a tile *count* is a byte
/// count), and every node here is 64 × 64.
pub const VIEW_CELLS_PER_BLOCK: u32 = 64;

/// Frequency and time levels of the view lattice. 8 × 8 = 64 nodes, which is exactly
/// [`hk_store::history::MAX_LEVELS`]: frequency reaches ×128 the floor and time reaches 128 s
/// cells (8192 s tiles), which is the range a *live edge* is looked at over. Wider or older than
/// that folds out of the coarsest node and says so per axis (`resolution.fold`), rather than
/// pretending to a resolution nothing measured.
pub const VIEW_LEVELS: usize = 8;

/// The view lattice this run opens: `f_cell_hz` × 1 s at node (0, 0), doubling independently on
/// each axis (`docs/16` §8.2).
///
/// # What the floor costs, measured — T-434's RAM caveat, answered
///
/// Measured at the steady state through a real pyramid, at the shipped 6.25 kHz floor (scheme 1's
/// own level-0 cell, so node (0, 0) is literally the store's finest cell on **both** axes):
///
/// | | `docs/16` §6.2's V(0, 0) | this floor |
/// |---|---|---|
/// | finest tile | 25.6 MHz × **9.1 h** | 400 kHz × **64 s** |
/// | accumulator | ~3 MB | **187 KB** |
/// | per MHz of tuned span, peak | — | **2.28 MB** (~46 MB at a 20 MHz live edge) |
/// | per MHz of tuned span, bound | — | **3.65 MB** (~73 MB) |
///
/// The surprise in that measurement, and the reason the number is a measurement rather than
/// arithmetic: **only the `level_f = 0` column stays resident.** A frequency-coarser node has the
/// *same* time cell as its producer, so the fold that fills it runs inside the producer's seal —
/// after the watermark has already passed that tile's end — and it is sealed in the same pass
/// instead of being left open. Residency is one tile row per **time** level, not per node, which
/// is an eighth of the obvious estimate.
///
/// `view_f_cell_hz` is the knob and it divides all of that linearly (a 25 kHz floor is ~18 MB at a
/// 20 MHz live edge, worst case). It is a **frequency** knob deliberately: the time floor is F1's
/// fix and is not negotiable.
///
/// The histogram is coarse (5 dB bins) rather than scheme 1's 0.5 dB, because a de-welded fold
/// **cannot carry percentiles at all** ([`hk_store::LevelConfig::t_factor`]: the parent's histogram
/// is the child's only when a child tile is one parent time cell, and no node here is) and
/// `/api/tiles` says so on the wire instead of approximating one. Level 0's own `p_lo`/`p_hi` are
/// exact per time column and unaffected; this only shrinks the per-tile rollup histogram that
/// nothing in this scheme reads.
pub fn view_lattice(f_cell_hz: f64) -> ViewLattice {
    ViewLattice {
        scheme: VIEW_SCHEME,
        f_cell_hz,
        t_cell: VIEW_T_CELL,
        cells_per_block: VIEW_CELLS_PER_BLOCK,
        f_levels: VIEW_LEVELS,
        t_levels: VIEW_LEVELS,
    }
}

/// [`view_lattice`] as a [`PyramidConfig`]: dBFS (the unit every frame of the live chain carries
/// before calibration), a coarse rollup histogram, and the run's own byte budget.
pub fn view_config(f_cell_hz: f64) -> PyramidConfig {
    PyramidConfig {
        histogram: HistogramConfig {
            lo_db: -200.0,
            step_db: 5.0,
            bins: 44,
        },
        ..PyramidConfig::view_lattice(view_lattice(f_cell_hz))
    }
}

/// Frames held while a tile read holds the view pyramid (about a minute at 10 rows/s, matching
/// [`HISTORY_QUEUE_FRAMES`]).
pub(crate) const VIEW_QUEUE_FRAMES: usize = HISTORY_QUEUE_FRAMES;

/// What one [`ViewIngest::ingest`] did.
#[derive(Debug, Default)]
pub(crate) struct ViewIngested {
    /// Frames folded by this call (this one, plus anything queued before it).
    pub folded: u64,
    /// Frames folded behind the watermark — the direct reading of T-446's defect.
    pub late: u64,
    /// Frames the pyramid refused.
    pub rejected: u64,
    /// The pyramid was busy: this frame was queued instead.
    pub deferred: bool,
    /// Queued frames dropped beyond the capacity.
    pub dropped: u64,
}

/// Folds live-chain frames into the view pyramid **without ever waiting for a reader** — the
/// always-on invariant, in the one place T-439 could have broken it.
///
/// `/api/tiles` holds this pyramid's lock per chunk (T-438 re-acquires it per output-row chunk
/// precisely so a fan-out cannot lock ingest out for a whole tile), but "short" is not "never", and
/// the history reader is on the same thread that drains the ring. So this only ever *tries* the
/// lock: a frame that finds it held is queued and folded, in arrival order, by the next call that
/// gets it. Beyond the capacity the **oldest** are dropped and counted — dropping the newest would
/// stall the growing edge, which is the one thing the surface is for.
///
/// This is [`hk_store::FloorIngestQueue`]'s contract applied to a bare [`Pyramid`]; it is a
/// separate type only because that one folds a *pair* (spectrum + floor frame) into a
/// `FloorProduct`, and the view scheme stores the uncalibrated frame alone.
#[derive(Debug)]
pub(crate) struct ViewIngest {
    /// A queued frame keeps **its own** origin, so a deferred frame is still recorded as of the
    /// time and front end it was taken at (T-133), not the one that happened to drain it.
    pending: VecDeque<(SpectrumFrame, FrameOrigin)>,
    capacity: usize,
}

impl ViewIngest {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            pending: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    fn fold(p: &mut Pyramid, frame: &SpectrumFrame, origin: FrameOrigin, out: &mut ViewIngested) {
        match p.ingest(&FrameInput::from_dsp(frame).with_origin(origin)) {
            Ok(IngestOutcome::Folded) => out.folded += 1,
            Ok(IngestOutcome::Late) => out.late += 1,
            Err(_) => out.rejected += 1,
        }
    }

    /// Folds `frame` (and anything queued before it) if the pyramid's lock is free **now**.
    pub(crate) fn ingest(
        &mut self,
        view: &Mutex<Pyramid>,
        frame: &SpectrumFrame,
        origin: FrameOrigin,
    ) -> ViewIngested {
        let mut out = ViewIngested::default();
        let mut guard = match view.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(e)) => e.into_inner(),
            Err(TryLockError::WouldBlock) => {
                self.pending.push_back((frame.clone(), origin));
                while self.pending.len() > self.capacity {
                    self.pending.pop_front();
                    out.dropped += 1;
                }
                out.deferred = true;
                return out;
            }
        };
        self.drain_into(&mut guard, &mut out);
        Self::fold(&mut guard, frame, origin, &mut out);
        out
    }

    fn drain_into(&mut self, p: &mut Pyramid, out: &mut ViewIngested) {
        for (f, o) in self.pending.drain(..) {
            Self::fold(p, &f, o, out);
        }
    }

    /// Folds what is left under a lock the caller already holds (end of stream).
    pub(crate) fn drain(&mut self, p: &mut Pyramid) -> ViewIngested {
        let mut out = ViewIngested::default();
        self.drain_into(p, &mut out);
        out
    }
}

/// T-136: the site a frame at sample time `t` is folded under: the attention service's assignment
/// peeked at `t` (never advancing or persisting the state machine; only the occupancy close does),
/// `unassigned` without a service.
pub(crate) fn frame_site(attention: Option<&AttentionService>, t: Timestamp) -> SiteKey {
    attention.map_or(SiteKey::Unassigned, |a| a.site_at_peek(t))
}

/// Runs reader 2 until the ring closes.
pub(crate) fn run(
    shared: Arc<Shared>,
    product: Arc<Mutex<FloorProduct>>,
    attention: Option<Arc<AttentionService>>,
    view: Option<Arc<Mutex<Pyramid>>>,
) -> anyhow::Result<()> {
    let welch = history_welch(shared.fft_len);
    let rows = shared.cfg.settings.history_rows_per_s.max(0.01);
    let k = ((shared.fs / (welch.hop() as f64 * rows)).round() as usize).max(1);
    let mut stft_cfg = StftConfig::new(welch, k);
    // T-139: scheduler steps shorter than a row still leave a (reduced-averaging) row.
    stft_cfg.partial = Some(PartialFrames {
        min_segments: k.div_ceil(PARTIAL_MIN_DIVISOR),
        arm_on: Discontinuity::RETUNE | Discontinuity::RATE_CHANGE,
    });
    let mut stft = crate::compute::stft(
        &shared.compute,
        &shared.counters.compute,
        Reader::History,
        stft_cfg,
    )?;
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default())
        .map_err(|e| anyhow::anyhow!("history floor tracker: {e:?}"))?;
    let mut queue = FloorIngestQueue::new(HISTORY_QUEUE_FRAMES);
    // T-439: the same frames, into the de-welded view lattice. One STFT, one floor tracker, one
    // origin — the live edge and the history are the SAME write, which is exactly §8's claim that
    // there is no live-versus-history path to keep consistent.
    let mut view_queue = ViewIngest::new(VIEW_QUEUE_FRAMES);
    let mut reader = shared.ring.reader_at(0);
    let cursor = shared.gate.register(0);
    let mut buf = vec![Complex::<i8>::default(); 1 << 16];
    let rc = &shared.counters.history_reader;
    let h = &shared.counters.history;
    let mut last_end = Timestamp::UNIX_EPOCH;
    let mut frames_since_update = 0u32;
    let mut on_frame = |frame: &SpectrumFrame| {
        let floor = tracker.update(frame, |_| {});
        let site = frame_site(attention.as_deref(), frame.t.host_time);
        // T-304: the source key comes from each frame's own provenance (the device that actually
        // produced it), not the run config's device_id — a replay's segments (or, in future, a
        // multi-source run) can carry blocks from more than one device in one run.
        let origin = FrameOrigin {
            source: source_key(&frame.provenance.device_id),
            site: Some(site),
        };
        let r = queue.ingest_from(&product, frame, floor, origin);
        if r.deferred {
            inc(&h.frames_deferred);
        }
        add(&h.frames_dropped, r.dropped);
        tally(h, &r.folded);
        // T-439: the growing edge. Never blocking — `ViewIngest` only tries the lock, so a tile
        // fan-out cannot hold up the thread that is draining the ring.
        if let Some(v) = view.as_deref() {
            let g = view_queue.ingest(v, frame, origin);
            add(&h.view_frames, g.folded);
            add(&h.view_late, g.late);
            add(&h.view_rejected, g.rejected);
            add(&h.view_dropped, g.dropped);
            if g.deferred {
                inc(&h.view_deferred);
            }
        }
        let dur = (frame.sample_count as f64 * 1e9 / frame.spectrum.sample_rate_hz) as i64;
        last_end = frame.t.host_time.saturating_add_nanos(dur);
        frames_since_update += 1;
        if frames_since_update >= 50 {
            if let Ok(p) = product.try_lock() {
                frames_since_update = 0;
                update_tiles(&shared, &p);
            }
            if let Some(Ok(v)) = view.as_deref().map(Mutex::try_lock) {
                update_view_tiles(&shared, &v);
            }
        }
    };
    loop {
        match reader.read_timeout(&mut buf, Duration::from_millis(50)) {
            ReadOutcome::Data(chunk) => {
                stft.push(InputInfo::from(&chunk), &buf[..chunk.len], &mut on_frame);
                cursor.set(chunk.end_sample());
                add(&rc.samples, chunk.len as u64);
            }
            // Nothing new for a read timeout: deliver an asynchronous provider's rows (T-056).
            ReadOutcome::Empty if stft.in_flight() > 0 => {
                stft.flush(&mut on_frame);
            }
            ReadOutcome::Overrun { .. } | ReadOutcome::Empty => {}
            ReadOutcome::Closed => break,
        }
        set(&rc.lost_samples, reader.lost_samples());
        set(&rc.overruns, reader.overruns());
        set(&rc.gap_samples, reader.gap_samples());
        let st = stft.stats();
        set(&rc.frames, st.frames);
        set(&rc.stft_resets, st.resets);
        set(&rc.partial_frames, st.partial_frames);
    }
    // Stream end or detach: frames still in flight are folded in before the queue drains.
    stft.flush(&mut on_frame);
    let st = stft.stats();
    set(&rc.frames, st.frames);
    set(&rc.stft_resets, st.resets);
    set(&rc.partial_frames, st.partial_frames);
    drop(cursor);
    let mut p = product.lock().unwrap_or_else(PoisonError::into_inner);
    tally(h, &queue.drain(&mut p));
    // A re-plumbed run (T-050) continues in a new segment: sealing now would make its frames late.
    //
    // **T-446 — the parentheses are the whole fix, and their absence was a permanent data loss.**
    // This guard went in as `!continues && A || B` over the pre-existing `A || B`. Rust reads that
    // as `(!continues && A) || B`, so `!continues` guarded only the first disjunct — and `B` is
    // `frames_ingested`, a RUN-WIDE counter shared by every segment (see the module docs on
    // `Shared`). After any segment has folded one frame, `B` is true forever, so **every** re-plumb
    // sealed anyway. Sealing advances the pyramid's monotonic `watermark_ns` to the last frame
    // **plus an hour**, and `Pyramid::ingest` answers `IngestOutcome::Late` for any frame whose
    // level-0 block ends at or before the watermark. So the first retune stopped spectrum history
    // for an hour of capture time — measured on the mock at 100.8 -> 433.92 MHz: `frames_ingested`
    // froze at 453 while `frames_late` climbed to 547, `/api/timeline` served
    // `grid.observed_cells = 0` for the new centre, and a retune *back* recovered nothing because
    // the watermark never retreats. The IQ ring and the record-derived coverage plane stayed
    // healthy throughout, which is what made it look like a read-side bug: the system knew it was
    // looking, stored the samples, and wrote no measurements.
    let continues = shared.continues.load(Ordering::SeqCst);
    let folded_anything = last_end.as_unix_nanos() > 0
        || shared
            .counters
            .history
            .frames_ingested
            .load(Ordering::Relaxed)
            > 0;
    let seal = !continues && folded_anything;
    if seal {
        p.seal_through(last_end.saturating_add_nanos(3_600_000_000_000))
            .map_err(|e| anyhow::anyhow!("sealing history: {e}"))?;
    }
    p.checkpoint()
        .map_err(|e| anyhow::anyhow!("history checkpoint: {e}"))?;
    update_tiles(&shared, &p);
    drop(p);
    // T-439 + T-446: the view pyramid ends its segment under the **same** decision, read from the
    // same `seal`. Recomputing the condition here would be a second place for `&&`/`||` to bind
    // wrongly and for a re-plumb to advance a monotonic watermark an hour past the last frame —
    // the defect T-446 measured, which would silently stop the growing edge after the first
    // retune and so falsify §8's central claim.
    if let Some(v) = view.as_deref() {
        let mut v = v.lock().unwrap_or_else(PoisonError::into_inner);
        let g = view_queue.drain(&mut v);
        add(&h.view_frames, g.folded);
        add(&h.view_late, g.late);
        add(&h.view_rejected, g.rejected);
        if seal {
            v.seal_through(last_end.saturating_add_nanos(3_600_000_000_000))
                .map_err(|e| anyhow::anyhow!("sealing view history: {e}"))?;
        }
        v.checkpoint()
            .map_err(|e| anyhow::anyhow!("view history checkpoint: {e}"))?;
        update_view_tiles(&shared, &v);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_core::ProvenanceHandle;
    use hk_model::{
        ClockSource, FreqRange, Provenance, SampleTime, TimeRange, TimestampMethod, Tune,
    };
    use hk_store::{FrameInput, Pyramid, PyramidConfig, RegionQuery, Resolution};
    use num_complex::Complex32;

    const S: i64 = 1_000_000_000;
    const T0: i64 = 1_789_300_800 * S;
    const FS: f64 = 2_048_000.0;
    const CENTER: f64 = 100_000_000.0;
    /// Bins, and so the segment length. Small, so the test is quick.
    const N: usize = 256;
    /// Segments averaged into one history frame. The real chain runs ~1000 at 20 Msps and
    /// 10 rows/s; 64 is enough to make an average and a maximum ~18 dB apart.
    const K: usize = 64;

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "hk-pipeline-{tag}-{}-{:?}",
                std::process::id(),
                std::time::Instant::now()
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn provenance() -> ProvenanceHandle {
        ProvenanceHandle::new(Provenance {
            device_id: "synthetic:t397".into(),
            tune: Tune {
                center_hz: CENTER,
                sample_rate_hz: FS,
                lna_db: 0.0,
                vga_db: 0.0,
                amp_on: false,
                bandwidth_hz: FS,
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

    /// `K` segments' worth of samples in which **exactly one** segment carries a strong tone at DC
    /// and the rest are silent: a burst one averaging interval long, the shape of every signal in
    /// the 902–928 MHz playground the user calls the canonical case.
    fn one_segment_burst(hop: usize) -> Vec<Complex32> {
        // The STFT hops by `hop`, so segment `i` spans `[i·hop, i·hop + N)`. Filling the window of
        // one segment only is enough: neighbours see part of it, which only helps the mean.
        let mut v = vec![Complex32::new(0.0, 0.0); K * hop + N];
        let burst = K / 2;
        for x in &mut v[burst * hop..burst * hop + N] {
            *x = Complex32::new(1.0, 0.0);
        }
        v
    }

    /// Runs `welch` over the burst and returns `(psd peak dB, max-hold peak dB)` of the first full
    /// frame, per Hz. The max-hold is empty when holds are off, and then reads as the PSD — which
    /// is exactly the substitution the store makes.
    fn frame_peaks(welch: WelchConfig) -> (f32, f32) {
        let mut stft = hk_dsp::StftProcessor::new(StftConfig::new(welch, K)).unwrap();
        let prov = provenance();
        let samples = one_segment_burst(welch.hop());
        let mut got = None;
        stft.push(
            InputInfo {
                time: SampleTime {
                    sample_index: 0,
                    host_time: Timestamp::from_unix_nanos(T0),
                },
                discontinuity: Discontinuity::STREAM_START,
                dropped_before: 0,
                provenance: &prov,
            },
            &samples,
            |frame: &SpectrumFrame| {
                if got.is_some() {
                    return;
                }
                let s = &frame.spectrum;
                let peak =
                    |v: &[f32]| 10.0 * v.iter().copied().fold(0.0f32, f32::max).max(1e-30).log10();
                let mean_db = peak(&s.psd);
                let hold_db = if s.max_hold.len() == s.psd.len() {
                    peak(&s.max_hold)
                } else {
                    mean_db
                };
                got = Some((mean_db, hold_db));
            },
        );
        got.expect("no full frame")
    }

    /// **T-397's measurement, not a declaration.** `/api/coverage` and `/api/timeline` both state
    /// `"fold": "max-hold"`, and the fold really was one — but the values it folded had already
    /// been averaged, because this reader turned the per-segment holds off. This asserts what the
    /// user actually sees: a burst lasting one averaging interval reaches the pyramid's `max_db`
    /// **at its own level**, not `10·log10(K)` below it.
    ///
    /// The control is the old configuration. With `holds = false` the same signal, the same STFT
    /// and the same ingest land ~18 dB lower (≈ `10·log10(64)`), so the assertion below is
    /// load-bearing rather than a tautology about a strong tone.
    #[test]
    fn t397_the_history_chain_stores_the_burst_peak_and_not_the_average_of_it() {
        let welch = history_welch(N);
        assert!(
            welch.holds,
            "the whole point: the per-segment holds are kept"
        );
        assert!(!welch.spectral_kurtosis, "SK is not read from this chain");

        let (mean_db, hold_db) = frame_peaks(welch);
        let mut washed = welch;
        washed.holds = false;
        let (_, substituted_db) = frame_peaks(washed);

        // The mutation: with holds off, `FrameInput::from_dsp` has no peak to offer and the store
        // maxes the *mean* instead. That is the ~10·log10(K) the user was losing.
        let expect_loss = 10.0 * (K as f32).log10();
        assert_eq!(
            substituted_db, mean_db,
            "holds off must substitute the averaged PSD: {substituted_db} vs {mean_db}"
        );
        assert!(
            hold_db - substituted_db > 0.5 * expect_loss,
            "the burst peak must stand above the average by most of {expect_loss:.1} dB, \
             got {hold_db:.1} vs {substituted_db:.1}"
        );

        // And the peak survives the whole store path: ingest → level-0 tile → query.
        let dir = TempDir::new("t397");
        let mut p = Pyramid::open(&dir.0, PyramidConfig::default()).unwrap();
        let prov = provenance();
        let samples = one_segment_burst(welch.hop());
        let mut stft = hk_dsp::StftProcessor::new(StftConfig::new(welch, K)).unwrap();
        let mut frames = 0;
        stft.push(
            InputInfo {
                time: SampleTime {
                    sample_index: 0,
                    host_time: Timestamp::from_unix_nanos(T0),
                },
                discontinuity: Discontinuity::STREAM_START,
                dropped_before: 0,
                provenance: &prov,
            },
            &samples,
            |frame: &SpectrumFrame| {
                p.ingest(&FrameInput::from_dsp(frame)).unwrap();
                frames += 1;
            },
        );
        assert!(frames >= 1, "no frame ingested");
        p.seal_through(Timestamp::from_unix_nanos(T0 + 3600 * S))
            .unwrap();
        let h = p
            .query(&RegionQuery {
                freq: FreqRange::centered(CENTER, 0.5 * FS),
                time: TimeRange::new(
                    Timestamp::from_unix_nanos(T0),
                    Timestamp::from_unix_nanos(T0 + 60 * S),
                ),
                resolution: Resolution::Level(0),
            })
            .unwrap();
        let served = h
            .cells
            .iter()
            .filter(|c| c.observed() && c.max_db.is_finite())
            .map(|c| c.max_db)
            .fold(f32::NEG_INFINITY, f32::max);
        assert!(
            served.is_finite(),
            "the pyramid served no observed cell for the burst"
        );
        // The served maximum is the frame's max-hold, within the store's 0.01 dB rounding and the
        // per-Hz density offset the ingest applies uniformly to both. Comparing the *gap* keeps
        // that offset out of it: what matters is that the burst stands proud of the mean by the
        // averaging gain, which is the property the strips render.
        let quiet = h
            .cells
            .iter()
            .filter(|c| c.observed() && c.p_low_db.is_finite())
            .map(|c| c.p_low_db)
            .fold(f32::INFINITY, f32::min);
        assert!(
            served - quiet > 0.5 * expect_loss,
            "the stored max must be a max-hold, not a max of means: {served:.1} over {quiet:.1}"
        );
    }
}
