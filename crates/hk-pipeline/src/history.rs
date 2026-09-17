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

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use hk_core::{Discontinuity, ReadOutcome};
use hk_dsp::floor::{FloorConfig, NoiseFloorTracker};
use hk_dsp::{InputInfo, PartialFrames, SpectrumFrame, StftConfig, WelchConfig};
use hk_model::Timestamp;
use hk_model::attention::baseline::SiteKey;
use hk_store::history::{FrameOrigin, source_key};
use hk_store::{FloorIngest, FloorIngestQueue, FloorProduct, IngestOutcome, StoreError};
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

fn tally(h: &HistoryCounters, folded: &[Result<FloorIngest, StoreError>]) {
    for r in folded {
        match r {
            Ok(i) if i.outcome == IngestOutcome::Late => inc(&h.frames_late),
            Ok(_) => inc(&h.frames_ingested),
            Err(_) => inc(&h.frames_rejected),
        }
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
) -> anyhow::Result<()> {
    let mut welch = WelchConfig::new(shared.fft_len);
    welch.holds = false;
    welch.spectral_kurtosis = false;
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
    let continues = shared.continues.load(Ordering::SeqCst);
    if !continues && last_end.as_unix_nanos() > 0
        || shared
            .counters
            .history
            .frames_ingested
            .load(Ordering::Relaxed)
            > 0
    {
        p.seal_through(last_end.saturating_add_nanos(3_600_000_000_000))
            .map_err(|e| anyhow::anyhow!("sealing history: {e}"))?;
    }
    p.checkpoint()
        .map_err(|e| anyhow::anyhow!("history checkpoint: {e}"))?;
    update_tiles(&shared, &p);
    Ok(())
}
