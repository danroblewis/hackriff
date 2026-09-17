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
    if !continues && folded_anything {
        p.seal_through(last_end.saturating_add_nanos(3_600_000_000_000))
            .map_err(|e| anyhow::anyhow!("sealing history: {e}"))?;
    }
    p.checkpoint()
        .map_err(|e| anyhow::anyhow!("history checkpoint: {e}"))?;
    update_tiles(&shared, &p);
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
