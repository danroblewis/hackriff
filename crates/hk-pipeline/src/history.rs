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
use std::sync::{Arc, Condvar, Mutex, PoisonError};
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

/// **Time** cells per tile at every node, and the answer to T-434's RAM caveat.
///
/// A level-0 tile is an in-memory accumulator for the whole of its duration ([`hk_store::Pyramid`]
/// keeps one open map per level), so the block *height* is **residency**: `docs/16` §6.2's V(0, 0)
/// holds a tile open for **9.1 h** at ~3 MB, which is fine for a survey and wrong for a growing
/// edge. At 64 time cells the finest node's tile spans **64 s** — 512× less residency — and the
/// client's tile stays 256 × 256 output cells regardless, because `/api/tiles` lays its grid on the
/// tile's own extent and reads however many store blocks that covers (T-438).
pub const VIEW_T_CELLS_PER_BLOCK: u32 = 64;

/// **Frequency** cells per tile at every node — scheme 1's own 1024, and deliberately NOT the same
/// number as [`VIEW_T_CELLS_PER_BLOCK`].
///
/// # The regression this constant exists to have fixed
///
/// The two axes were first set together at 64, on the reasoning that §6.2 wanted uniform tiles and
/// that a smaller block is cheaper. The second half is only true of the axis that costs memory.
/// Resident accumulator is `(span / f_cell) × t_cells_per_block × 44 B` — **independent of the
/// frequency blocking**, because a narrower block means proportionally more of them. What the
/// frequency blocking does set is the **number of tile files**, and that is paid at every seal:
/// 64 cells is a 400 kHz block, so a 21 MHz capture holds 53 level-0 blocks against scheme 1's 4,
/// and the fold cascade multiplies that across all 64 nodes — about **840 tiles written** at the
/// end-of-run seal instead of ~64.
///
/// Measured, M0 acceptance (50 runs, same box, back to back): **162.8 s** wall with 64-cell
/// frequency blocks against **31.7 s** with the view lattice off — 5.1× — for **identical user
/// CPU** (331 s vs 333 s) and unchanged peak RSS (6.42 GB vs 6.28 GB). Same CPU and 5× the wall
/// clock is not compute and not memory: every run was blocked on per-file I/O at teardown, ~2.6 s
/// of it, which is ~3 ms × 840 files. Four tests that act on a *live, unpaced, looping* run —
/// Listen's admission cap and the RDS and POCSAG recipe starts — then failed on wall-clock waits,
/// and they were the only four that do that.
///
/// The bytes were never the problem and are not changed by this: total sealed cells depend on the
/// span and the duration, not on how they are cut into files. Only the file count changes, and
/// 1024 makes it scheme 1's. The cost is edge waste on a narrow capture — a 500 kHz window still
/// rounds up to one 6.4 MHz block of cells — which is the direction to spend it in, because the
/// live edge this scheme exists for is wide.
///
/// # What it did NOT fix, and the number that is intrinsic
///
/// Widening the blocking recovered only about a quarter of the regression, and neither did moving
/// the writes to their own thread (102 s) nor sealing through the last frame (112.7 s). Varying the
/// **node count** did: a 4 × 4 lattice ran the same suite in 62.6 s against 8 × 8's 112.7 s and the
/// control's 32.9 s, i.e. roughly **1.5 s of suite wall per lattice node**. That measurement is why
/// [`VIEW_LEVELS`] is 4 and why T-453 exists; shipped, 4 × 4 with both write fixes runs it in
/// **52.1 s**.
///
/// That is not a defect, it is the **de-welding's running cost, and nobody had costed it**. T-434
/// measured a lattice at about 4× its finest level *on disk* and accepted that; the same 4× applies
/// to **tile write operations per second of capture**, and that is the part a running pipeline
/// pays. A welded ladder's coarser levels are vastly coarser in *time* — scheme 1 steps ×60, ×15,
/// ×4, ×24 — so they write almost nothing and the total is ~1.02× level 0. A de-welded ×2 lattice
/// has one tile series per time level, giving ~2× on the time axis and ~2× on the frequency axis:
/// **~4× scheme 1's tile writes, by construction** — and worse than that on a *narrow* capture,
/// where a frequency-coarser node's tile seals on the same watermark as its producer's however few
/// frequency blocks the capture spans, so the frequency arm costs a full ×4 rather than ×2.
///
/// The eager fold at every seal was itself a deviation from `docs/16` §5.2, which decided
/// *precomputed at seal time, on demand at the live edge*. **T-453 fixed it**
/// ([`hk_store::PyramidConfig::coarse_on_demand`]): capture writes node (0, 0) only, and the ~4×
/// above is what a viewer of every node pays rather than what every second of capture pays. Same
/// suite, same box, comparing the ten targets' own test time (so no build is counted): **160.4 s
/// against 190.6 s**, and the M0 slice inside it **33.5 s against 54.1 s** — against T-439's
/// lattice-off control of **32.9 s**, which is to say the lattice now costs the gate nothing
/// measurable.
///
/// **It is invisible on the product and amplified only by the harness.** One acceptance run costs
/// 10.5 s against 10.0 s with the lattice off — within noise — because a device runs *one*
/// pipeline. The suite runs 28 concurrently on one volume, several of them replaying
/// **time-compressed 48 h scenes**, so tile writes scale with stream duration and 28 pipelines'
/// worth contend for one disk. Opening the pyramid without writing it costs nothing (31.4 s at
/// 64 levels), which is what isolates the cost to the writes.
pub const VIEW_F_CELLS_PER_BLOCK: u32 = 1024;

/// Frequency and time levels of the view lattice: **4 × 4 = 16 nodes**, so frequency runs
/// 6.25 kHz → 50 kHz (tiles 6.4 → 51.2 MHz) and time 1 s → 8 s (tiles 64 → 512 s). That is the
/// range a **live edge** is actually looked at over, and a live edge is what this scheme exists
/// for.
///
/// # Why not 8 × 8, which [`hk_store::history::MAX_LEVELS`] would allow
///
/// Because the reach it buys is not real, and it is not free.
///
/// **Not real:** T-438 found `docs/16` §6.2's V0 unbackable at the **coarse** end as well as the
/// fine one — a 3.28 GHz × 48-day tile needs 32 768 frequency cells at scheme 1's coarsest and no
/// finer level is affordable inside `/api/tiles`'s work budget. The nodes beyond this ladder
/// address a surface nothing can serve; an address past the coarsest node still answers, folded out
/// of it and saying so per axis in `resolution.fold`, which is the honest form of the same picture.
///
/// **Not free:** measured at ~**1.5 s of M0-acceptance wall per node** (see
/// [`VIEW_F_CELLS_PER_BLOCK`] for the isolation). With both write fixes below, 4 × 4 runs that
/// suite in **52.1 s** against 8 × 8's 112.7 s and the lattice-off control's 32.9 s — 8 × 8 would
/// cost about 80 s per merge, on a gate that runs every merge this milestone, for reach nothing
/// can back.
///
/// # What T-453 changed, and what it did not
///
/// T-453 made the coarse nodes **lazy** ([`hk_store::PyramidConfig::coarse_on_demand`],
/// `docs/16` §5.2): capture writes node (0, 0) and nothing else, and a coarse node is folded by the
/// read that asks for it — sealed on the way when its own time block has elapsed, so it is
/// precomputed for every reader after the first. The measured consequence is that **neither of the
/// two costs above scales with the node count any more**. The M0 slice went from **54.1 s to
/// 33.5 s** against a lattice-off control of 32.9 s; `hk-pipeline`'s own suite from **262 s to
/// 75 s**, because `live_edge_tiles` no longer waits on capture to fold the off-diagonal nodes; and
/// the resident accumulator floor from **2.28 MB/MHz to 0.91 MB/MHz** (bound 3.65 → 0.91), which is
/// one tile row instead of one per time level. So this constant is a **reach** decision again,
/// which is exactly what the ticket was for.
///
/// It is still 4, because the *other* half of the argument is unchanged: T-438 found the nodes
/// beyond this ladder address a surface nothing can serve, and reach nothing can back is not worth
/// having at any price. Growing it now costs what the extra reach is worth rather than what the
/// gate charges for it.
///
/// **This is a configuration, not a contract.** The alternative of ×4 steps per axis reaches the
/// same node count by changing [`hk_store::ViewLattice`]'s own shape for every future consumer;
/// that is a contract change, and the right time to consider it is when the real access pattern has
/// been measured.
pub const VIEW_LEVELS: usize = 4;

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
/// | finest tile | 25.6 MHz × **9.1 h** | 6.4 MHz × **64 s** (scheme 1's own tile shape) |
/// | accumulator | ~3 MB | ~2.9 MB |
/// | per MHz of tuned span, peak and bound | — | **0.91 MB** (~18 MB at a 20 MHz live edge) |
///
/// T-439 measured 2.28 MB/MHz peak against a 3.65 MB/MHz bound, and the reason those were
/// measurements rather than arithmetic was that **only the `level_f = 0` column stayed resident**:
/// a frequency-coarser node has the *same* time cell as its producer, so the fold that filled it
/// ran inside the producer's seal and it was sealed in the same pass instead of being left open.
/// Residency was one tile row per **time** level, not per node.
///
/// **T-453 collapsed that to one row** (plus one, inside `seal_lag`, while a tile's successor has
/// been created and it has not yet been written). With the coarse nodes built on demand
/// ([`hk_store::PyramidConfig::coarse_on_demand`]) capture opens node (0, 0)'s accumulator and no
/// other, whichever axis a node coarsens — so the floor above is the **whole lattice's**, and it
/// does not move when the lattice grows.
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
        // Sets both axes; `view_config` overrides the frequency one. `ViewLattice` has a single
        // knob because §6.2 assumed uniform tiles; the measurement above is why they differ.
        cells_per_block: VIEW_T_CELLS_PER_BLOCK,
        f_levels: VIEW_LEVELS,
        t_levels: VIEW_LEVELS,
    }
}

/// [`view_lattice`] as a [`PyramidConfig`]: dBFS (the unit every frame of the live chain carries
/// before calibration), a coarse rollup histogram, and the run's own byte budget.
pub fn view_config(f_cell_hz: f64) -> PyramidConfig {
    PyramidConfig {
        // The frequency blocking is a FILE-COUNT decision and the time blocking a MEMORY one, so
        // they are set separately. See [`VIEW_F_CELLS_PER_BLOCK`] for the measurement that
        // separated them.
        f_cells_per_block: VIEW_F_CELLS_PER_BLOCK,
        histogram: HistogramConfig {
            lo_db: -200.0,
            step_db: 5.0,
            bins: 44,
        },
        ..PyramidConfig::view_lattice(view_lattice(f_cell_hz))
    }
}

/// Frames the view writer may hold (about a minute at 10 rows/s, matching
/// [`HISTORY_QUEUE_FRAMES`]).
pub(crate) const VIEW_QUEUE_FRAMES: usize = HISTORY_QUEUE_FRAMES;

/// The view pyramid's writer: **its own thread**, because the fold is not the expensive part —
/// the seal is, and the seal must not land on a thread that gates capture.
///
/// # The regression this type exists to have fixed
///
/// The first version folded on the history reader's own thread, protected only by a try-lock so a
/// `/api/tiles` reader could never park it. That protects against the *reader* and misses the
/// writer: `Pyramid::ingest` seals, encodes, zstd-compresses and writes tiles **inline**, and the
/// history reader holds a [`crate::gate`] cursor, so in a gated run capture cannot advance past the
/// slowest reader. Every tile boundary therefore stopped the ring for as long as the write took.
/// Scheme 1 has the same shape and gets away with it; a 64-node lattice writes about ten times as
/// much and does not.
///
/// Measured, M0 acceptance (50 runs, same box, back to back): **162.8 s** wall folding on the
/// reader thread against **31.7 s** with the view lattice off — 5.1× — for **identical user CPU**
/// (331 s vs 333 s) and unchanged peak RSS. Same CPU, five times the wall clock, is not compute and
/// not memory: the runs were *waiting*. Widening the frequency blocking 16× (fewer, larger files)
/// recovered only 26 % of it, which is what ruled out per-file overhead and pointed at the thread
/// the work was on rather than the shape of the work. The four tests that failed in the
/// coordinator's gate were exactly the four that act on a **live, unpaced, looping** run and wait in
/// wall clock for stream time to advance — Listen's admission cap, and the RDS and POCSAG recipe
/// starts.
///
/// So the frames cross a thread boundary and the growing edge is written behind it. The history
/// reader's per-frame cost becomes a clone and a push; nothing it does can block on a tile write,
/// a zstd pass, or an `/api/tiles` reader.
///
/// **Drop-oldest, not drop-newest**, past [`VIEW_QUEUE_FRAMES`]: the newest frame is the growing
/// edge, which is the whole point of the surface. That is why this is a `VecDeque` behind a
/// `Condvar` rather than a `sync_channel`, which can only refuse the newest.
pub(crate) struct ViewWriter {
    shared: Arc<ViewQueue>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Default)]
struct ViewQueueState {
    /// A queued frame keeps **its own** origin, so a deferred frame is recorded as of the time and
    /// front end it was taken at (T-133), not whenever the writer got to it.
    pending: VecDeque<(SpectrumFrame, FrameOrigin)>,
    /// Set once the reader has handed over everything; `Some(true)` also asks for the final seal.
    finish: Option<bool>,
}

struct ViewQueue {
    state: Mutex<ViewQueueState>,
    wake: Condvar,
    capacity: usize,
}

impl ViewWriter {
    /// Starts the writer thread for `view`.
    pub(crate) fn start(
        view: Arc<Mutex<Pyramid>>,
        shared: Arc<Shared>,
        capacity: usize,
    ) -> anyhow::Result<Self> {
        let q = Arc::new(ViewQueue {
            state: Mutex::new(ViewQueueState::default()),
            wake: Condvar::new(),
            capacity: capacity.max(1),
        });
        let (qt, st) = (Arc::clone(&q), Arc::clone(&shared));
        let thread = std::thread::Builder::new()
            .name("hk-view".into())
            .spawn(move || view_writer(&qt, &view, &st))?;
        Ok(Self {
            shared: q,
            thread: Some(thread),
        })
    }

    /// Hands `frame` to the writer. Never blocks, never waits on the pyramid.
    pub(crate) fn push(&self, h: &HistoryCounters, frame: &SpectrumFrame, origin: FrameOrigin) {
        let mut st = self
            .shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        st.pending.push_back((frame.clone(), origin));
        let mut dropped = 0;
        while st.pending.len() > self.shared.capacity {
            st.pending.pop_front();
            dropped += 1;
        }
        drop(st);
        add(&h.view_dropped, dropped);
        self.shared.wake.notify_one();
    }

    /// Drains what is queued, seals when `seal` (T-446's **one** decision, passed in rather than
    /// recomputed), checkpoints, and joins.
    pub(crate) fn finish(mut self, seal: bool) {
        {
            let mut st = self
                .shared
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            st.finish = Some(seal);
        }
        self.shared.wake.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for ViewWriter {
    /// A run that unwinds still stops the thread; it just does not seal, because an unfinished
    /// segment must not advance a monotonic watermark (T-446).
    fn drop(&mut self) {
        if let Some(t) = self.thread.take() {
            {
                let mut st = self
                    .shared
                    .state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                st.finish.get_or_insert(false);
            }
            self.shared.wake.notify_all();
            let _ = t.join();
        }
    }
}

/// The writer thread: fold, and at the end seal (if asked) and checkpoint.
///
/// The thread takes the pyramid's lock **per batch**, not per frame, so a `/api/tiles` reader
/// interleaves with it at batch granularity and neither waits long for the other. Nothing here can
/// reach the ring.
fn view_writer(q: &ViewQueue, view: &Mutex<Pyramid>, shared: &Shared) {
    let h = &shared.counters.history;
    let mut batch: Vec<(SpectrumFrame, FrameOrigin)> = Vec::new();
    loop {
        let finish = {
            let mut st = q.state.lock().unwrap_or_else(PoisonError::into_inner);
            while st.pending.is_empty() && st.finish.is_none() {
                st = q.wake.wait(st).unwrap_or_else(PoisonError::into_inner);
            }
            batch.extend(st.pending.drain(..));
            st.finish
        };
        if !batch.is_empty() {
            let mut p = view.lock().unwrap_or_else(PoisonError::into_inner);
            let (mut folded, mut late, mut rejected) = (0u64, 0u64, 0u64);
            for (f, o) in batch.drain(..) {
                match p.ingest(&FrameInput::from_dsp(&f).with_origin(o)) {
                    Ok(IngestOutcome::Folded) => folded += 1,
                    Ok(IngestOutcome::Late) => late += 1,
                    Err(_) => rejected += 1,
                }
            }
            add(&h.view_frames, folded);
            add(&h.view_late, late);
            add(&h.view_rejected, rejected);
            update_view_tiles(shared, &p);
        }
        if let Some(seal) = finish {
            let mut p = view.lock().unwrap_or_else(PoisonError::into_inner);
            if seal && let Some(t) = p.latest_frame_end() {
                // **Through the last frame, NOT an hour past it — and that is the whole
                // difference between 70 ms and 1288 ms.**
                //
                // Scheme 1's run-end seal adds an hour of slack so that every partially-filled
                // tile is forced shut and a finished replay's history is complete on disk at every
                // level. That is 9 files for a five-rung ladder. For a 64-node lattice the same
                // gesture seals every node's current tile however little of it was observed:
                // measured on a 10 s, 21 MHz run, **273 files and 1288 ms to persist 0.1 MB**,
                // against scheme 1's 9 files and 67 ms. The bytes were never the cost; the file
                // creations are, and under the acceptance suite's 28 concurrent runs they
                // serialise on one volume — 31.7 s of suite became 162.8 s, and the four tests
                // that failed were exactly the four that wait in wall clock on a live run.
                //
                // Sealing through the last frame costs 70 ms, the same as scheme 1, and loses
                // nothing that was measured: level 0's open tiles are written by `checkpoint`
                // below, and a coarse node fills when a finer tile actually completes — which is
                // what a growing edge does anyway. Forcing it early would write an 8192 s tile to
                // record ten seconds, and call it sealed.
                //
                // The monotonic watermark still applies: a segment that CONTINUES must not seal at
                // all (T-446), which is why `seal` arrives from the reader rather than being
                // decided here.
                let _ = p.seal_through(t);
            }
            let _ = p.checkpoint();
            update_view_tiles(shared, &p);
            return;
        }
    }
}

/// T-136: the site a frame at sample time `t` is folded under: the attention service's assignment
/// peeked at `t` (never advancing or persisting the state machine; only the occupancy close does),
/// `unassigned` without a service.
pub(crate) fn frame_site(attention: Option<&AttentionService>, t: Timestamp) -> SiteKey {
    attention.map_or(SiteKey::Unassigned, |a| a.site_at_peek(t))
}

/// Reader 2's STFT: the history bin width, `K` for ~`rows_per_s` frames/s, T-139 partial rows
/// and the T-524 DC notch-and-interpolate.
pub(crate) fn history_stft_config(fs: f64, fft_len: usize, rows_per_s: f64) -> StftConfig {
    let welch = history_welch(fft_len);
    let rows = rows_per_s.max(0.01);
    let k = ((fs / (welch.hop() as f64 * rows)).round() as usize).max(1);
    let mut stft_cfg = StftConfig::new(welch, k);
    // T-524: history (the pyramid, the view lattice, the tiles, the trace and every sweep hop) is
    // folded from notch-and-interpolated frames, so a sweep does not stamp the LO spike at each
    // hop's centre. Detection runs its own STFT without this and keeps its DC rule.
    stft_cfg.dc_notch_half_bins = Some(crate::observe::DC_INTERP_HALF_BINS);
    // T-139: scheduler steps shorter than a row still leave a (reduced-averaging) row.
    stft_cfg.partial = Some(PartialFrames {
        min_segments: k.div_ceil(PARTIAL_MIN_DIVISOR),
        arm_on: Discontinuity::RETUNE | Discontinuity::RATE_CHANGE,
    });
    stft_cfg
}

/// Runs reader 2 until the ring closes.
pub(crate) fn run(
    shared: Arc<Shared>,
    product: Arc<Mutex<FloorProduct>>,
    attention: Option<Arc<AttentionService>>,
    view: Option<Arc<Mutex<Pyramid>>>,
) -> anyhow::Result<()> {
    let rows = shared.cfg.settings.history_rows_per_s;
    let stft_cfg = history_stft_config(shared.fs, shared.fft_len, rows);
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
    // there is no live-versus-history path to keep consistent. The WRITING happens on
    // [`ViewWriter`]'s own thread, because this one gates capture and a tile seal must not.
    let view_writer = match view {
        Some(v) => Some(ViewWriter::start(
            v,
            Arc::clone(&shared),
            VIEW_QUEUE_FRAMES,
        )?),
        None => None,
    };
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
        // T-439: the growing edge. A clone and a push — no lock on the pyramid, no tile write, no
        // zstd, nothing that a reader or the disk can make slow. This thread holds a gate cursor.
        if let Some(w) = view_writer.as_ref() {
            w.push(h, frame, origin);
        }
        let dur = (frame.sample_count as f64 * 1e9 / frame.spectrum.sample_rate_hz) as i64;
        last_end = frame.t.host_time.saturating_add_nanos(dur);
        frames_since_update += 1;
        if frames_since_update >= 50 {
            if let Ok(p) = product.try_lock() {
                frames_since_update = 0;
                update_tiles(&shared, &p);
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
    // same `seal`. Recomputing the condition there would be a second place for `&&`/`||` to bind
    // wrongly and for a re-plumb to advance a monotonic watermark an hour past the last frame —
    // the defect T-446 measured, which would silently stop the growing edge after the first retune
    // and so falsify §8's central claim. `finish` drains, seals-or-not, checkpoints and joins, so
    // every view counter is final before this returns.
    if let Some(w) = view_writer {
        w.finish(seal);
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

    /// T-524: two adjacent sweep hops, each with an LO-leakage carrier at its own centre on top of
    /// noise. Through reader 2's STFT config and into the pyramid, neither hop's centre shows the
    /// spike in stored history; without the notch the same input does (so the test bites).
    #[test]
    fn t524_adjacent_sweep_hops_store_no_spike_at_either_centre() {
        let hop_centres = [CENTER, CENTER + FS / 2.0];
        let peak_over_floor = |notch: bool| -> Vec<f32> {
            let mut cfg = super::history_stft_config(FS, N, 20.0);
            if !notch {
                cfg.dc_notch_half_bins = None;
            }
            let dir = TempDir::new(if notch { "t524n" } else { "t524r" });
            let mut p = Pyramid::open(&dir.0, PyramidConfig::default()).unwrap();
            let mut stft = hk_dsp::StftProcessor::new(cfg).unwrap();
            let per_hop = 4 * cfg.frame_samples() as usize;
            let mut rng = 0x5eed_u64;
            let mut noise = || {
                rng = rng
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((rng >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 0.02
            };
            for (h, &c) in hop_centres.iter().enumerate() {
                let mut prov = provenance().get().clone();
                prov.tune.center_hz = c;
                let prov = ProvenanceHandle::new(prov);
                // Noise plus a constant DC offset: the LO leakage every tune carries.
                let samples: Vec<Complex32> = (0..per_hop)
                    .map(|_| Complex32::new(0.05 + noise(), noise()))
                    .collect();
                let start = (h * per_hop) as u64;
                stft.push(
                    InputInfo {
                        time: SampleTime {
                            sample_index: start,
                            host_time: Timestamp::from_unix_nanos(
                                T0 + (start as f64 * 1e9 / FS) as i64,
                            ),
                        },
                        discontinuity: if h == 0 {
                            Discontinuity::STREAM_START
                        } else {
                            Discontinuity::RETUNE
                        },
                        dropped_before: 0,
                        provenance: &prov,
                    },
                    &samples,
                    |frame: &SpectrumFrame| {
                        p.ingest(&FrameInput::from_dsp(frame)).unwrap();
                    },
                );
            }
            p.seal_through(Timestamp::from_unix_nanos(T0 + 3600 * S))
                .unwrap();
            hop_centres
                .iter()
                .map(|&c| {
                    let h = p
                        .query(&RegionQuery {
                            freq: FreqRange::centered(c, 0.4 * FS),
                            time: TimeRange::new(
                                Timestamp::from_unix_nanos(T0),
                                Timestamp::from_unix_nanos(T0 + 60 * S),
                            ),
                            resolution: Resolution::Level(0),
                        })
                        .unwrap();
                    let mut vals: Vec<f32> = h
                        .cells
                        .iter()
                        .filter(|c| c.observed() && c.max_db.is_finite())
                        .map(|c| c.max_db)
                        .collect();
                    assert!(!vals.is_empty(), "no stored history around {c} Hz");
                    vals.sort_by(f32::total_cmp);
                    vals[vals.len() - 1] - vals[vals.len() / 2]
                })
                .collect()
        };
        let raw = peak_over_floor(false);
        assert!(
            raw.iter().all(|&d| d > 20.0),
            "the un-notched input must show the LO spike (else the test does not bite): {raw:?}"
        );
        let notched = peak_over_floor(true);
        assert!(
            notched.iter().all(|&d| d < 6.0),
            "stored history still shows a spike at a hop centre: {notched:?} dB over the median"
        );
    }
}
