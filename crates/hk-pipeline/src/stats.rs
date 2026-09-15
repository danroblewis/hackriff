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
    /// C12 baselines, novelty and scoring (T-119).
    AttentionCounters {
        /// Observations folded into baselines.
        folds,
        /// Folds with novelty > 0.
        novel_folds,
        /// Change points latched.
        change_points,
        /// Candidate sets published.
        publishes,
        /// Baseline files written.
        baseline_writes,
        /// Store or database errors.
        errors,
        /// Folds whose learning the baseline memory cap refused (novelty still scored; T-132).
        refused_folds,
        /// Baseline engines saved and unloaded by the memory cap (T-132).
        unloaded_engines,
        /// Folds under a gain state beyond the subject's kept gain slots: not learned (T-132).
        gain_overflow_folds,
        /// Gauge: approximate heap bytes of the loaded baselines (T-132).
        memory_bytes,
    }
);

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
        /// Frames emitted from a reset's partial averaging (T-139; history reader only, included
        /// in `frames`).
        partial_frames,
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
        /// Analog chains that stopped because a neighbouring chain already owns the emission they
        /// refined to (T-071 dedupe, [`crate::chains::EmissionClaims`]).
        duplicate_emission,
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

counter_group!(
    /// On-demand listening (T-043, [`crate::chains::listen`]). Metadata only.
    ListenCounters {
        /// Listen requests.
        requests,
        /// Refused by the legal gate (restricted, metadata-only or unclassified class).
        refused_class,
        /// Refused at the listener cap.
        refused_busy,
        /// Refused otherwise (bad request, unknown target, outside the window, no analog mode).
        refused_other,
        /// Probes run (ring reads for mode estimation; only after the class gate passed).
        probes,
        /// Audio chains attached.
        attached,
        /// Audio chains detached.
        detached,
        /// Audio chains running now.
        active,
        /// Audio data records published.
        frames,
        /// Status records published.
        status_records,
        /// Frames withheld while the squelch was closed.
        squelched_frames,
        /// Source samples skipped to stay live (live sources only).
        skipped_samples,
        /// Records dropped for slow consumers (drop-not-block).
        consumer_dropped,
        /// Newest processing latency (chunk read to record published), µs.
        latency_us_last,
        /// Largest processing latency, µs.
        latency_us_max,
        /// Sessions ended because the window moved off the channel.
        retune_ends,
        /// Sessions ended with no consumer for the idle timeout.
        idle_ends,
        /// Demodulation or publish errors.
        errors,
        /// Sessions opened (T-066). `open == running + sum(closed_*)`.
        open,
        /// Chain threads running now (`active` counts admitted slots, which are released as soon
        /// as the client goes; the thread ends at its next read).
        running,
        /// Closed because the client went away: WebSocket close, reset, unresponsive peer, Stop.
        closed_client,
        /// Closed with no consumer for the idle timeout.
        closed_idle,
        /// Closed after no audio (squelch closed) for the squelch timeout.
        closed_squelch,
        /// Closed because an in-place retune moved the window off the channel.
        closed_retune,
        /// Closed because a re-plumb ended the chain's segment.
        closed_segment,
        /// Closed because the source or run ended.
        closed_source,
        /// Closed by a demodulation or publish error.
        closed_error,
        /// Admission limit: most chains at once.
        limit_listeners,
        /// Admission CPU budget, millicores.
        budget_mcores,
        /// Estimated millicores of the admitted chains.
        budget_used_mcores,
    }
);

impl ListenCounters {
    /// The counters plus `budget` (T-066): `{max_listeners, listeners, running, cores,
    /// used_cores}`, what `/api/status` reports.
    pub fn status_json(&self) -> Value {
        let mut v = self.to_json();
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        if let Some(o) = v.as_object_mut() {
            o.insert(
                "budget".into(),
                json!({
                    "max_listeners": l(&self.limit_listeners),
                    "listeners": l(&self.active),
                    "running": l(&self.running),
                    "cores": l(&self.budget_mcores) as f64 / 1e3,
                    "used_cores": l(&self.budget_used_mcores) as f64 / 1e3,
                }),
            );
        }
        v
    }
}

counter_group!(
    /// Burst taps (T-060, [`crate::chains::taps`]): bits and symbols streams for external
    /// programs. Metadata only.
    TapCounters {
        /// Tap requests.
        requests,
        /// Requests refused (bad request, unknown target, at capacity, run ended).
        refused,
        /// Taps opened.
        attached,
        /// Taps closed by their consumer going away.
        detached,
        /// Taps open now.
        active,
        /// Bursts offered to open taps.
        bursts,
        /// Burst status records published.
        status_records,
        /// Burst data records published with payload.
        records,
        /// Data records the egress gate reduced to header-only (`GATED`).
        gated,
        /// Bursts whose class withheld content (status record only).
        withheld,
        /// Framing inferences run for live tap output.
        inferences,
        /// Publish errors.
        errors,
    }
);

counter_group!(
    /// The run's on-demand chain budget (T-071, [`crate::chains::budget`]): Listen chains and
    /// burst taps admitted against one count limit and one estimated CPU budget.
    BudgetCounters {
        /// On-demand chains admitted now (listeners + taps).
        chains,
        /// Listen chains admitted now.
        listeners,
        /// Burst taps admitted now.
        taps,
        /// Estimated millicores of the admitted chains.
        used_mcores,
        /// Most on-demand chains at once (effective).
        limit_chains,
        /// Most Listen chains at once.
        limit_listeners,
        /// Most burst taps at once.
        limit_taps,
        /// CPU budget, millicores.
        budget_mcores,
        /// Requests refused by the budget (count or CPU).
        refused_busy,
    }
);

impl BudgetCounters {
    /// What `/api/status` reports as `budget`.
    pub fn status_json(&self) -> Value {
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        json!({
            "max_chains": l(&self.limit_chains),
            "chains": l(&self.chains),
            "max_listeners": l(&self.limit_listeners),
            "listeners": l(&self.listeners),
            "max_taps": l(&self.limit_taps),
            "taps": l(&self.taps),
            "cores": l(&self.budget_mcores) as f64 / 1e3,
            "used_cores": l(&self.used_mcores) as f64 / 1e3,
            "refused_busy": l(&self.refused_busy),
        })
    }
}

/// CPU time of the calling thread, ns (0 where the clock is unavailable).
pub fn thread_cpu_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec; the call only writes it.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &raw mut ts) };
    if rc != 0 {
        return 0;
    }
    (ts.tv_sec as u64).saturating_mul(1_000_000_000) + ts.tv_nsec as u64
}

/// Accounts one chain's CPU time across the threads it runs on (T-071): the time on earlier
/// threads is kept as a base when the chain moves (a Listen probe on the opener thread, then its
/// own thread).
#[derive(Debug)]
pub struct CpuClock {
    thread: Option<std::thread::ThreadId>,
    thread_start_ns: u64,
    base_ns: u64,
}

impl CpuClock {
    /// A clock that starts on the first [`ChainStat::account_cpu`].
    pub fn new() -> Self {
        Self {
            thread: None,
            thread_start_ns: 0,
            base_ns: 0,
        }
    }
}

impl Default for CpuClock {
    fn default() -> Self {
        Self::new()
    }
}

/// One running chain's own counters (T-071): each chain writes only its own entry, so chains
/// share nothing mutable but the ring. Registered in [`ChainTable`] while it lives.
pub struct ChainStat {
    /// Run-unique id.
    pub id: u64,
    /// `listen`, `bits-tap`, `symbols-tap` or the chain spec id.
    pub kind: String,
    started: std::time::Instant,
    center_bits: AtomicU64,
    bandwidth_bits: AtomicU64,
    stream_id: std::sync::Mutex<Option<String>>,
    handle: std::sync::Mutex<Option<hk_stream::PublisherHandle>>,
    /// Samples read.
    pub samples: AtomicU64,
    /// Ring samples lost (overruns) or skipped to stay live.
    pub lost_samples: AtomicU64,
    /// CPU time, ns.
    pub cpu_ns: AtomicU64,
    /// Newest processing latency (read to publish), µs.
    pub latency_us_last: AtomicU64,
    /// Largest processing latency, µs.
    pub latency_us_max: AtomicU64,
    /// Newest backlog behind the ring writer, µs of stream time.
    pub backlog_us: AtomicU64,
    /// Records published (audio frames, bursts).
    pub records: AtomicU64,
}

impl std::fmt::Debug for ChainStat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainStat")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl ChainStat {
    /// The channel the chain demodulates, Hz.
    pub fn set_channel(&self, center_hz: f64, bandwidth_hz: f64) {
        self.center_bits
            .store(center_hz.to_bits(), Ordering::Relaxed);
        self.bandwidth_bits
            .store(bandwidth_hz.to_bits(), Ordering::Relaxed);
    }

    /// The chain's output stream (its drop counters are read from `handle`).
    pub fn set_stream(&self, stream_id: &str, handle: hk_stream::PublisherHandle) {
        *self
            .stream_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(stream_id.to_owned());
        *self
            .handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
    }

    /// Stores the chain's CPU time as of now (call on the chain's thread).
    pub fn account_cpu(&self, clock: &mut CpuClock) {
        let me = std::thread::current().id();
        if clock.thread != Some(me) {
            clock.base_ns = self.cpu_ns.load(Ordering::Relaxed);
            clock.thread = Some(me);
            clock.thread_start_ns = thread_cpu_ns();
        }
        let on_thread = thread_cpu_ns().saturating_sub(clock.thread_start_ns);
        self.cpu_ns
            .store(clock.base_ns + on_thread, Ordering::Relaxed);
    }

    /// One processing latency sample, µs.
    pub fn latency(&self, us: u64) {
        self.latency_us_last.store(us, Ordering::Relaxed);
        self.latency_us_max.fetch_max(us, Ordering::Relaxed);
    }

    /// A JSON snapshot: the stream's records dropped for its consumers come from its publisher.
    pub fn to_json(&self) -> Value {
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let age_s = self.started.elapsed().as_secs_f64();
        let cpu_s = l(&self.cpu_ns) as f64 / 1e9;
        let handle = self
            .handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let (consumers, dropped) = handle.map_or((0, 0), |h| {
            let s = h.consumer_stats();
            (
                h.open_consumers(),
                s.iter().map(|c| c.records_dropped).sum::<u64>(),
            )
        });
        let f = |b: &AtomicU64| {
            let v = f64::from_bits(b.load(Ordering::Relaxed));
            if v.is_finite() && v != 0.0 {
                json!(v)
            } else {
                Value::Null
            }
        };
        json!({
            "id": self.id,
            "kind": self.kind,
            "stream_id": self
                .stream_id
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            "center_hz": f(&self.center_bits),
            "bandwidth_hz": f(&self.bandwidth_bits),
            "age_s": (age_s * 1e3).round() / 1e3,
            "samples": l(&self.samples),
            "lost_samples": l(&self.lost_samples),
            "cpu_s": (cpu_s * 1e3).round() / 1e3,
            "cpu_load": if age_s > 0.0 { ((cpu_s / age_s) * 1e3).round() / 1e3 } else { 0.0 },
            "latency_ms_last": l(&self.latency_us_last) as f64 / 1e3,
            "latency_ms_max": l(&self.latency_us_max) as f64 / 1e3,
            "backlog_ms": l(&self.backlog_us) as f64 / 1e3,
            "records": l(&self.records),
            "consumers": consumers,
            "dropped": dropped,
        })
    }
}

/// The run's running chains, each with its own [`ChainStat`] (`/api/status` `chain_stats`).
#[derive(Default)]
pub struct ChainTable {
    next: AtomicU64,
    list: std::sync::Mutex<Vec<std::sync::Arc<ChainStat>>>,
}

impl std::fmt::Debug for ChainTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainTable")
            .field("running", &self.len())
            .finish()
    }
}

impl ChainTable {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<std::sync::Arc<ChainStat>>> {
        self.list
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Registers a chain of `kind`; the entry leaves the table when the guard drops.
    pub fn register(self: &std::sync::Arc<Self>, kind: &str) -> ChainStatGuard {
        let stat = std::sync::Arc::new(ChainStat {
            id: self.next.fetch_add(1, Ordering::Relaxed),
            kind: kind.to_owned(),
            started: std::time::Instant::now(),
            center_bits: AtomicU64::new(0),
            bandwidth_bits: AtomicU64::new(0),
            stream_id: std::sync::Mutex::new(None),
            handle: std::sync::Mutex::new(None),
            samples: AtomicU64::new(0),
            lost_samples: AtomicU64::new(0),
            cpu_ns: AtomicU64::new(0),
            latency_us_last: AtomicU64::new(0),
            latency_us_max: AtomicU64::new(0),
            backlog_us: AtomicU64::new(0),
            records: AtomicU64::new(0),
        });
        self.lock().push(std::sync::Arc::clone(&stat));
        ChainStatGuard {
            stat,
            table: std::sync::Arc::clone(self),
        }
    }

    /// Running chains.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// No chain is running.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Snapshots of the running chains.
    pub fn to_json(&self) -> Value {
        let list: Vec<std::sync::Arc<ChainStat>> = self.lock().clone();
        Value::Array(list.iter().map(|s| s.to_json()).collect())
    }
}

/// A registered chain's stats; dropping it removes the entry.
#[derive(Debug)]
pub struct ChainStatGuard {
    stat: std::sync::Arc<ChainStat>,
    table: std::sync::Arc<ChainTable>,
}

impl std::ops::Deref for ChainStatGuard {
    type Target = ChainStat;
    fn deref(&self) -> &ChainStat {
        &self.stat
    }
}

impl ChainStatGuard {
    /// The shared entry (for a reader that updates it).
    pub fn stat(&self) -> std::sync::Arc<ChainStat> {
        std::sync::Arc::clone(&self.stat)
    }
}

impl Drop for ChainStatGuard {
    fn drop(&mut self) {
        self.table
            .lock()
            .retain(|s| !std::sync::Arc::ptr_eq(s, &self.stat));
    }
}

counter_group!(
    /// Observation log producers (T-115; the writer's own counters are on the store).
    ObservationCounters {
        /// Records offered to the writer queue.
        records_offered,
        /// Records dropped because the writer queue was full.
        records_dropped,
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
    /// On-demand listening (T-043).
    pub listen: ListenCounters,
    /// Burst taps (T-060).
    pub taps: TapCounters,
    /// The on-demand chain budget (T-071).
    pub budget: BudgetCounters,
    /// Serialises admission against the budget (control plane, never the sample path).
    pub admission: std::sync::Mutex<()>,
    /// Per-chain CPU, latency and drop counters of the running chains (T-071).
    pub chain_stats: std::sync::Arc<ChainTable>,
    /// Scheduler.
    pub scheduler: SchedulerCounters,
    /// C12 baselines/novelty/score (T-119).
    pub attention: AttentionCounters,
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
    /// Compute provider selections of the run (T-056).
    pub compute: crate::compute::ComputeReport,
    /// Observation log producers (T-115).
    pub observations: ObservationCounters,
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
        // Both on-demand caps side by side: `listen.budget` (T-066) and `taps.max_taps` (T-060).
        let mut taps = self.taps.to_json();
        let max_taps = self.budget.limit_taps.load(Ordering::Relaxed);
        taps["max_taps"] = json!(if max_taps > 0 {
            max_taps
        } else {
            crate::chains::taps::MAX_TAPS as u64
        });
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
            "listen": self.listen.status_json(),
            "taps": taps,
            "budget": self.budget.status_json(),
            "chain_stats": self.chain_stats.to_json(),
            "scheduler": self.scheduler.to_json(),
            "attention": self.attention.to_json(),
            "stream_time_ns": self.stream_time_ns.load(Ordering::Relaxed),
            "tune": { "center_hz": center, "sample_rate_hz": rate },
            "compute": self.compute.to_json(),
            "observations": self.observations.to_json(),
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
