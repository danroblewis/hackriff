//! Per-stage counters (atomics, written by the stage threads) and the JSON snapshot served as the
//! run summary and by `/api/status`.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use serde_json::{Map, Value, json};

macro_rules! counter_group {
    ($(#[$m:meta])* $name:ident { $($(#[$fm:meta])* $field:ident),* $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Default)]
        pub struct $name {
            $($(#[$fm])* pub $field: AtomicU64,)*
        }

        impl $name {
            /// A JSON object of the current values.
            pub fn to_json(&self) -> Value {
                let mut m = Map::new();
                $(m.insert(stringify!($field).into(), json!(self.$field.load(Ordering::Relaxed)));)*
                Value::Object(m)
            }
        }
    };
}

counter_group!(
    /// Capture thread.
    SourceCounters {
        /// Samples pushed into the ring.
        samples,
        /// Blocks pushed.
        blocks,
        /// Samples the source reported missing (`dropped_before`).
        source_dropped,
        /// Replay passes restarted by `--loop`.
        loops,
        /// Blocks held back by the lossless gate.
        gate_waits,
        /// Blocks the ring refused.
        ring_errors,
        /// Source read errors.
        read_errors,
        /// cf32/ci16 blocks quantised to ci8 for the ring.
        quantised_blocks,
        /// Tune changes the capture thread held for the coverage-chain poll (lossless replay).
        coverage_waits,
        /// Coverage holds released by their 10 s bound instead of the poll.
        coverage_wait_timeouts,
    }
);

counter_group!(
    /// One always-on ring reader.
    ReaderCounters {
        /// Samples read.
        samples,
        /// Samples overwritten before this reader read them (ring overruns).
        lost_samples,
        /// Overrun events.
        overruns,
        /// Source-gap indices passed.
        gap_samples,
        /// Frames produced by this reader's STFT.
        frames,
        /// STFT resets (discontinuities, gaps).
        stft_resets,
    }
);

counter_group!(
    /// Detection reader (C08–C10) and its stores.
    DetectCounters {
        /// Detection records.
        detections,
        /// Detection rows written.
        detections_written,
        /// Confirmation events.
        confirmations,
        /// Frames with more runs than the detector labels.
        dense_frames,
        /// Frames with no valid floor.
        invalid_floor_frames,
        /// Frames where the step guard switched the floor branch off.
        guarded_frames,
        /// Tracks opened.
        tracks_opened,
        /// Tracks closed.
        tracks_closed,
        /// Track merges.
        track_merges,
        /// Track rows upserted.
        track_rows,
        /// Track↔detection links written.
        track_links,
        /// Tracks confirmed (offered as POIs / chain candidates).
        tracks_confirmed,
        /// Floor-change events.
        floor_events,
        /// Anomalies opened.
        anomalies_opened,
        /// Anomalies closed.
        anomalies_closed,
        /// Explanations written by the correlator.
        explanations,
        /// Repository write batches.
        db_batches,
        /// Repository errors.
        db_errors,
        /// Verification captures handed to the scheduler.
        captures,
        /// Flushes kept on the detection reader because the writer queue was full (live).
        writer_queue_full,
        /// Flushes that waited for the writer thread (lossless replay, or the carry-over cap).
        writer_blocked,
        /// Detection records flagged `dense_skipped` (spanning dense frames or dropped runs).
        dense_flagged,
        /// Trust verdict rows written.
        verdicts_written,
    }
);

counter_group!(
    /// History reader (C26 pyramid + C33 floor product).
    HistoryCounters {
        /// Frames folded (calibrated or not).
        frames_ingested,
        /// Frames the product refused.
        frames_rejected,
        /// Frames older than the watermark.
        frames_late,
        /// Tiles written (uncalibrated + calibrated pyramids).
        tiles_written,
        /// Bytes written.
        bytes_written,
        /// Frames queued because a query held the floor product (folded by a later frame).
        frames_deferred,
        /// Queued frames dropped because a query held the product past the queue's capacity.
        frames_dropped,
    }
);

counter_group!(
    /// Spectrum stream reader (C24).
    SpectrumCounters {
        /// Rows offered to the publisher.
        rows,
        /// Rows withheld by the gated-spectrum rate cap.
        rows_gated,
        /// Publisher errors.
        errors,
    }
);

counter_group!(
    /// Runtime chains (ADR-0001 S1).
    ChainCounters {
        /// Chains attached.
        attached,
        /// Chains detached (finished).
        detached,
        /// Chain candidates refused because the class forbids their content.
        refused_class,
        /// Candidates with no matching chain spec.
        unmatched,
        /// Candidates whose channel does not fit inside the tuned window.
        outside_window,
        /// Candidates on a channel that already has a chain.
        duplicate_channel,
        /// Analog chains whose probe mode selection was not accepted.
        mode_rejected,
        /// Chain attach failures (plugin spawn, DDC plan).
        attach_errors,
        /// Samples chains read.
        samples,
        /// Samples chains lost to ring overruns (never the always-on readers').
        lost_samples,
        /// Demodulation rows written.
        demodulations,
        /// Decode rows written (own demods).
        decodes,
        /// Decodes whose content the class withheld.
        content_withheld,
        /// Emitters created by chains.
        emitters_created,
        /// Emitter labels written.
        labels,
        /// FSK bursts demodulated.
        fsk_bursts,
        /// FSK boxes whose samples had left the chain buffer.
        fsk_boxes_missed,
        /// CRC-valid frames.
        crc_valid,
        /// SigMF recordings written.
        recordings,
        /// Recordings refused by the content class.
        recordings_refused_class,
        /// Recordings discarded (samples missing).
        recordings_incomplete,
        /// Samples pushed to plugins.
        plugin_samples,
        /// Plugin decodes stored.
        plugin_decodes,
        /// Plugin records dropped (queue full / detached).
        plugin_dropped,
        /// Plugin restarts.
        plugin_restarts,
        /// Plugin records held back until the plugin's input queue had room (lossless replay).
        plugin_waits,
        /// Plugin backpressure waits abandoned (plugin failed or made no progress for 30 s).
        plugin_wait_timeouts,
        /// FSK bits records published (payload delivered).
        bits_records,
        /// FSK bits records published header-only because the class forbids content.
        bits_gated,
        /// Raster-channel attaches skipped while the channel cools down after a finished chain.
        channel_cooldown,
        /// Chain rows written without their triggering detection, which was never stored within
        /// the wait (detect reader overrun, failed store).
        detection_ref_missing,
        /// Chain errors (demod, repository).
        errors,
    }
);

counter_group!(
    /// Attention scheduler (C04).
    SchedulerCounters {
        /// Steps applied.
        steps,
        /// Discovery steps.
        sweep_steps,
        /// POI dwell steps.
        dwell_steps,
        /// Trust-test steps.
        trust_steps,
        /// POIs offered.
        pois_offered,
        /// POIs refused (queue full, invalid).
        pois_refused,
        /// Step application errors.
        apply_errors,
        /// Tune commands the replay guard did not apply (a recording cannot be retuned).
        virtual_tunes_ignored,
        /// Provenances marked as virtual tuning (replay gain/filter changes the samples never
        /// had).
        virtual_provenances,
        /// Verification captures recorded.
        verification_captures,
        /// Verification groups evaluated.
        verifications,
        /// Gain-step pairs evaluated.
        gain_pairs_run,
        /// Gain-step pairs skipped (clipped).
        gain_pairs_skipped_clipped,
        /// Retune comparisons evaluated.
        retunes_run,
        /// Retune comparisons skipped (the replay could not move).
        retunes_skipped_virtual,
        /// Rate-change comparisons evaluated (`hk_detect::rate_change`).
        rate_changes_run,
        /// Rate-change comparisons skipped (centre mismatch or same rate).
        rate_changes_skipped,
    }
);

/// All counters of one run.
#[derive(Debug, Default)]
pub struct Counters {
    /// Capture thread.
    pub source: SourceCounters,
    /// Detection reader.
    pub detect_reader: ReaderCounters,
    /// History reader.
    pub history_reader: ReaderCounters,
    /// Spectrum reader.
    pub spectrum_reader: ReaderCounters,
    /// Detection stages.
    pub detect: DetectCounters,
    /// History stores.
    pub history: HistoryCounters,
    /// Spectrum stream.
    pub spectrum: SpectrumCounters,
    /// Runtime chains.
    pub chains: ChainCounters,
    /// Scheduler.
    pub scheduler: SchedulerCounters,
    /// Stream time of the newest block end, ns.
    pub stream_time_ns: AtomicI64,
    /// Newest tuned centre, Hz (f64 bits).
    pub tune_center_bits: AtomicU64,
    /// Newest sample rate, Hz (f64 bits).
    pub tune_rate_bits: AtomicU64,
    /// Tune changes published by the capture thread (bumped after `tune_*_bits` are stored).
    pub tune_seq: AtomicU64,
    /// The newest `tune_seq` whose coverage the chain manager has evaluated.
    pub coverage_seq: AtomicU64,
    /// Trust verdicts from the control thread, awaiting the detection writer thread.
    pub verdicts: crate::verify::VerdictOutbox,
}

impl Counters {
    /// Newest tuned centre and rate, Hz.
    pub fn tune(&self) -> (f64, f64) {
        (
            f64::from_bits(self.tune_center_bits.load(Ordering::Relaxed)),
            f64::from_bits(self.tune_rate_bits.load(Ordering::Relaxed)),
        )
    }

    /// A JSON snapshot (the run summary's `counters` and `/api/status`).
    pub fn to_json(&self) -> Value {
        let (center, rate) = self.tune();
        json!({
            "source": self.source.to_json(),
            "readers": {
                "detect": self.detect_reader.to_json(),
                "history": self.history_reader.to_json(),
                "spectrum": self.spectrum_reader.to_json(),
            },
            "detect": self.detect.to_json(),
            "history": self.history.to_json(),
            "spectrum": self.spectrum.to_json(),
            "chains": self.chains.to_json(),
            "scheduler": self.scheduler.to_json(),
            "stream_time_ns": self.stream_time_ns.load(Ordering::Relaxed),
            "tune": { "center_hz": center, "sample_rate_hz": rate },
        })
    }

    /// Ring samples lost by the always-on readers.
    pub fn always_on_lost(&self) -> u64 {
        [
            &self.detect_reader,
            &self.history_reader,
            &self.spectrum_reader,
        ]
        .iter()
        .map(|r| r.lost_samples.load(Ordering::Relaxed))
        .sum()
    }
}

/// Adds `n`.
#[inline]
pub fn add(c: &AtomicU64, n: u64) {
    c.fetch_add(n, Ordering::Relaxed);
}

/// Adds one.
#[inline]
pub fn inc(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

/// Sets the value.
#[inline]
pub fn set(c: &AtomicU64, n: u64) {
    c.store(n, Ordering::Relaxed);
}

/// Reads the value.
#[inline]
pub fn get(c: &AtomicU64) -> u64 {
    c.load(Ordering::Relaxed)
}
