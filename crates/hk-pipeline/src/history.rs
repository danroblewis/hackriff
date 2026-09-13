//! Always-on reader 2: spectrum history (C26 pyramid) and the noise-floor product (C33).
//!
//! Its own STFT (the detection bin width, Hann 50 % overlap, `K` for ~`history_rows_per_s`
//! frames/s, no SK) and its own `NoiseFloorTracker` feed `FloorProduct::ingest`, which folds
//! each frame into the dBFS/Hz pyramid (or the dBm/Hz one when a calibration applies). The
//! product's uncalibrated pyramid is the history `/api/history` answers from. At the end of the run
//! tiles are sealed through the last frame plus an hour.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use hk_core::ReadOutcome;
use hk_dsp::floor::{FloorConfig, NoiseFloorTracker};
use hk_dsp::{InputInfo, StftConfig, StftProcessor, WelchConfig};
use hk_model::Timestamp;
use hk_store::{FloorProduct, IngestOutcome};
use num_complex::Complex;

use crate::run::Shared;
use crate::stats::{add, inc, set};

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

/// Runs reader 2 until the ring closes.
pub(crate) fn run(shared: Arc<Shared>, product: Arc<Mutex<FloorProduct>>) -> anyhow::Result<()> {
    let mut welch = WelchConfig::new(shared.fft_len);
    welch.holds = false;
    welch.spectral_kurtosis = false;
    let rows = shared.cfg.settings.history_rows_per_s.max(0.01);
    let k = ((shared.fs / (welch.hop() as f64 * rows)).round() as usize).max(1);
    let mut stft = StftProcessor::new(StftConfig::new(welch, k))
        .map_err(|e| anyhow::anyhow!("history STFT: {e:?}"))?;
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default())
        .map_err(|e| anyhow::anyhow!("history floor tracker: {e:?}"))?;
    let mut reader = shared.ring.reader_at(0);
    let cursor = shared.gate.register(0);
    let mut buf = vec![Complex::<i8>::default(); 1 << 16];
    let rc = &shared.counters.history_reader;
    let h = &shared.counters.history;
    let mut last_end = Timestamp::UNIX_EPOCH;
    let mut frames_since_update = 0u32;
    loop {
        match reader.read_timeout(&mut buf, Duration::from_millis(50)) {
            ReadOutcome::Data(chunk) => {
                stft.push(InputInfo::from(&chunk), &buf[..chunk.len], |frame| {
                    let floor = tracker.update(frame, |_| {});
                    let mut p = product.lock().unwrap_or_else(PoisonError::into_inner);
                    match p.ingest(frame, floor) {
                        Ok(i) if i.outcome == IngestOutcome::Late => inc(&h.frames_late),
                        Ok(_) => inc(&h.frames_ingested),
                        Err(_) => inc(&h.frames_rejected),
                    }
                    let dur =
                        (frame.sample_count as f64 * 1e9 / frame.spectrum.sample_rate_hz) as i64;
                    last_end = frame.t.host_time.saturating_add_nanos(dur);
                    frames_since_update += 1;
                    if frames_since_update >= 50 {
                        frames_since_update = 0;
                        update_tiles(&shared, &p);
                    }
                });
                cursor.set(chunk.end_sample());
                add(&rc.samples, chunk.len as u64);
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
    }
    drop(cursor);
    let mut p = product.lock().unwrap_or_else(PoisonError::into_inner);
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
