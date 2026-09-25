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
        /// Blocks pushed forward to keep capture time monotone because a source's own clock was
        /// behind the run's (T-474). A `--loop` wrap is an expected splice and is counted in
        /// `loops`, not here; a count here means a segment restarted under a rewound source.
        time_splices,
        /// Blocks held back by the lossless gate.
        gate_waits,
        /// Blocks the ring refused.
        ring_errors,
        /// Source read errors.
        read_errors,
        /// **Blocks refused because their provenance could not be true** (T-541): a sample rate
        /// that is zero, negative or not finite, or a centre that is not finite. A front end
        /// cannot produce such a block, so it is a corrupt transfer or an uninitialised driver
        /// struct, and it is dropped at this boundary rather than divided by downstream. Non-zero
        /// is a defect signal about the front end, never a normal outcome.
        bad_blocks,
        /// cf32/ci16 blocks quantised to ci8 for the ring.
        quantised_blocks,
        /// Tune changes the capture thread held for the coverage-chain poll (lossless replay).
        coverage_waits,
        /// Coverage holds released by their 10 s bound instead of the poll.
        coverage_wait_timeouts,
        /// CPU time of the capture thread(s), ns (T-510: the per-block cost every front end pays
        /// whether or not anyone looks; sampled every [`crate::capture::CPU_SAMPLE_BLOCKS`] blocks
        /// and at the thread's end).
        cpu_ns,
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
        /// CPU time of this reader's thread(s), ns (T-510; history reader only: the per-row cost
        /// of the growing edge, paid on every front end's ring whether or not anyone looks).
        cpu_ns,
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
        /// Track↔detection links written (T-913: written, not attempted).
        track_links,
        /// Track↔detection links asked for whose detection was not stored — never written, or
        /// aged out by retention before the link was drained (T-913). Any growth here is lost
        /// track membership, not a rounding detail.
        track_links_dropped,
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
        /// Presence extensions published on the `presence` stream (T-388).
        presence_extensions,
        /// Presence extensions a tick left out at its cap; those boxes grow on the poll instead.
        presence_extensions_truncated,
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
        /// T-439: frames folded into the de-welded **view** lattice (the growing edge).
        view_frames,
        /// View-lattice frames folded behind the watermark — T-446's reading, on the second
        /// pyramid: a non-zero value here means a segment end sealed a run that continues.
        view_late,
        /// View-lattice frames the pyramid refused.
        view_rejected,
        /// View-lattice frames dropped past the writer queue's capacity — the writer thread is
        /// more than a minute behind, which on a growing edge means the disk is the bottleneck.
        view_dropped,
        /// View-lattice tiles written.
        view_tiles_written,
        /// View-lattice bytes written.
        view_bytes_written,
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
        /// T-416: declined probes whose measurement was written anyway, so the refusal leaves a
        /// record rather than silence. Below `mode_rejected` means a refusal went unrecorded.
        declined_measurements,
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
        /// T-247: M3 classification rows the C15 cascade wrote (`crate::classify`).
        classifications,
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
        /// Records offered to a plugin process that had not reported ready (T-223; 0 for a plugin
        /// that declares no readiness signal). Per process (T-224): a restart re-arms readiness,
        /// and a process that never signals counts every record fed to it after the bounded wait.
        plugin_fed_before_ready,
        /// Readiness waits that timed out, so the chain fed the plugin anyway (T-223).
        plugin_ready_timeouts,
        /// FSK bits records published (payload delivered).
        bits_records,
        /// FSK bits records published header-only because the class forbids content.
        bits_gated,
        /// Raster-channel attaches skipped while the channel cools down after a finished chain.
        channel_cooldown,
        /// Analog chains that stopped because a neighbouring chain already owns the emission they
        /// refined to (T-071 dedupe, [`crate::chains::EmissionClaims`]).
        duplicate_emission,
        /// T-186: analog chains that identified their emission (mode, pilot, refined tuning,
        /// family and explanations) from the leading refine window, before the full window.
        identifications,
        /// T-209: pilot-locked analog sessions without a decoded identity whose emitter was not
        /// placed because the window's front end was overloaded or clipping.
        mode_emitters_withheld,
        /// T-926: analog windows restarted inside the ring's history after the chain was lapped
        /// (an overrun while it computed its probe or early identification) — instead of writing
        /// the fragment it had, too short for RDS.
        window_restarts,
        /// Chain rows written without their triggering detection, which was never stored within
        /// the wait (detect reader overrun, failed store).
        detection_ref_missing,
        /// T-287: control-channel hunt passes run ([`crate::chains::trunk`]). One pass is one
        /// occupancy sweep of the tuned window's raster plus the demodulations it admitted.
        cc_passes,
        /// Raster channels measured for occupancy across those passes.
        cc_channels,
        /// Channels that reached FCO **candidacy**: continuous, and on the raster. Candidacy is
        /// not evidence of a control channel — C23's named pitfall is that continuous data
        /// emitters look exactly like this.
        cc_candidates,
        /// Candidates actually demodulated (bounded per pass by the spec's `max_demods`).
        cc_demods,
        /// Candidates the per-pass admission cap refused a demodulation.
        cc_admission_refused,
        /// T-546: hunt passes where the receiver's own offset from the channel grid was **fitted
        /// and found to exceed the raster tolerance** (docs/19 §7.6a). It is a property of the
        /// receiver, not of any signal, so one pass counts once however many channels it
        /// re-aligned. A HackRF One at −9.6 ppm hides every 800 MHz channel without it.
        cc_grid_corrections,
        /// T-546: confirmed control channels whose decode was filed back onto the **inventory
        /// emitter** at the same frequency — the product vision's "successful decode confirms it".
        cc_attached,
        /// T-546: confirmed control channels whose symbol structure (level count, symbol rate,
        /// outer deviation) was blindly measured and persisted as `estimated_params`.
        cc_structures,
        /// Control channels **confirmed** by frame sync *and* CRC (`CcConfirmer::confirm`).
        cc_confirmed,
        /// TrunkSystem rows written, one per distinct confirmed control channel (metadata only:
        /// a protocol, a frequency and times).
        cc_systems,
        /// T-268: CRC-valid TSBKs decoded from confirmed control channels.
        cc_tsbks,
        /// Identifier updates (IDEN_UP) seen, before the agreement gate.
        cc_iden_ups,
        /// T-271: CRC-valid DMR CSBKs decoded from confirmed control channels. The DMR twin of
        /// `cc_tsbks`, counted apart from it because they are different framings on different
        /// systems and summing them would hide which protocol a run actually found.
        cc_csbks,
        /// T-271: DMR Tier III grants decoded. **Every one of these is also counted in
        /// `cc_grants_unmapped`**, and that is the point rather than a bookkeeping accident: DMR
        /// announces no channel parameters this build could corroborate, so a logical channel
        /// number has no on-air base or step to resolve through and no frequency is ever produced
        /// for one. The grant itself — talkgroup, radio, channel, timeslot — is fully recorded.
        cc_dmr_grants,
        /// T-345: CRC-valid NXDN CACs decoded from confirmed outbound RCCH frames. Counted apart
        /// from `cc_tsbks` and `cc_csbks` for the same reason those are counted apart from each
        /// other: summing them would hide which protocol a run actually found.
        cc_cacs,
        /// T-345: NXDN Type-C channel assignments decoded. **Every one of these is also counted in
        /// `cc_grants_unmapped`**, and that is the point rather than a bookkeeping accident: the
        /// NXDN air interface carries a channel *number* and defines no mapping from one to hertz,
        /// so no frequency is ever produced for one until a channel map is configured. The
        /// assignment itself — channel, call type, source unit, destination group or unit — is
        /// fully recorded.
        cc_nxdn_grants,
        /// Channel-table entries **admitted**: an identifier corroborated by agreeing
        /// announcements and appended to `trunk_channel_plan`. Far fewer than `cc_iden_ups`,
        /// because a single unrepeated announcement never enters a band plan.
        cc_iden_admitted,
        /// Grants whose channel number resolved to a frequency.
        cc_grants_mapped,
        /// Grants recorded as `unmapped-channel`: an identifier never announced, or one decoded
        /// too long ago to trust. Logged, never resolved to a plausible-looking wrong frequency
        /// (C23's stale-IDEN pitfall).
        cc_grants_unmapped,
        /// T-269: grants recorded as `outside-window`. The channel number resolved to a real
        /// frequency, and that frequency fell outside the ≤20 MHz window the radio is holding, so
        /// it could not be followed (C23's span limit). **Logged, never dropped** — a dropped
        /// grant is indistinguishable from a system with no traffic.
        cc_grants_outside_window,
        /// Granted channels followed: one channelizer (C11) allocation each, over the window the
        /// hunt already buffered.
        cc_follows,
        /// Followable grants the per-pass `max_follows` cap refused.
        cc_follow_refused,
        /// Passes that followed nothing because no raster channel was quiet enough to measure a
        /// noise floor against. Boundaries are a comparison; without a reference there is nothing
        /// to compare to, so nothing is claimed.
        cc_follow_no_reference,
        /// Followed channels that carried no transmission at all inside the window. The grant row
        /// still stands; no call is invented for an observation that was not made.
        cc_follow_silent,
        /// T-628: passes whose receiver **alias** was resolved — the absolute offset behind the
        /// modulo-raster grid fit, chosen by the crystal's bound alone or by which admissible
        /// alias put energy on the most granted channels. Following needs it: a grant is an
        /// absolute frequency.
        cc_alias_resolved,
        /// T-628: passes that TRIED the alias and could not settle it (a tie, nothing occupied, or
        /// no alias inside the bound). Distinct from never trying, which counts in neither.
        cc_alias_unresolved,
        /// T-628: in-window grants NOT followed because the alias was unresolved. The grant rows
        /// stand; no call is filed from a guessed offset.
        cc_follow_unresolved,
        /// `CallRecord` rows written (metadata only: who, where, when, on what channel — never
        /// audio, never content).
        cc_calls,
        /// Calls whose end was **observed**, by a silence timeout on the granted channel. The
        /// rest carry `t_end = NULL`, which the model defines as "still open, or never observed".
        cc_calls_closed,
        /// T-308: calls still keyed when the buffered window ended — written **open and
        /// truncated** (`t_end` NULL, `observed_until` set), never closed at the window's edge.
        /// Each one's duration is a lower bound, and the count is how often the dwell, not the
        /// radio, decided where a call stopped being measured.
        cc_calls_truncated,
        /// T-308: truncated calls a later pass **continued** — the same row grown across an
        /// unobserved gap no longer than the silence timeout itself. Counted apart from
        /// `cc_calls`, because a continuation is not a new call. Ordinarily zero: the built-in
        /// hunt's gap between passes is 9.5 s, 105x the bound.
        cc_calls_continued,
        /// T-270: calls a decoded encryption indication flagged as **encrypted**. Metadata about
        /// the call, never its content — nothing is decrypted, and no audio exists to suppress.
        cc_calls_encrypted,
        /// Calls the encryption check refused a voice path (`VoicePermit`): encrypted, or — just
        /// as firmly — **unknown**. Only a call whose own LDU2 ALGID said clear escapes it
        /// (T-330). The check sits where a vocoder would, so a later audio path cannot skip it by
        /// forgetting to ask.
        cc_voice_refused,
        /// T-330: followed calls whose encryption state was decided by their **own** LDU2 ALGID
        /// rather than by the grant — the authoritative statement, folded in by `CallHeader`.
        cc_calls_algid,
        /// T-849: followed FDMA channels demodulated for P25 Phase 1 voice frames — one DDC and
        /// at most two C4FM demodulations each, over the window already held. Metadata only: the
        /// IMBE voice codewords are skipped by position and no audio exists.
        cc_voice_frame_demods,
        /// T-849: LDU1s whose link control decoded (NID within the BCH bound, Reed–Solomon
        /// valid) on followed channels.
        cc_ldu1,
        /// T-849: LDU2s whose encryption sync — the call's own ALGID and key id — decoded on
        /// followed channels. Folded into each call's encryption state (T-330).
        cc_ldu2,
        /// T-297: characterising chains attached ([`crate::chains::sweep`]).
        ///
        /// Counted **apart from** `attached`, which has always meant a chain attached to
        /// demodulate, decode or record a candidate. A chain that only measures a region and
        /// writes evidence about it is not one of those, and several suites pin `attached`
        /// exactly; giving the measuring chains their own pair keeps those assertions literally
        /// true instead of quietly inflating them.
        sweep_attached,
        /// Characterising chains that finished.
        sweep_detached,
        /// Sweep-characterisation windows examined. One pass is one channelised window of a
        /// candidate region and one two-lag sweep test on its strongest frame.
        sweep_passes,
        /// Regions a sweep rate was measured for and written to their emitter's features.
        sweep_characterised,
        /// Regions examined that were **not** characterised: the two lags did not both peak and
        /// agree, so nothing is claimed about them. Not a failure — a carrier, a wideband burst
        /// and a 2-FSK burst all land here by design.
        sweep_uncharacterised,
        /// Sweep chains the concurrency cap refused to attach.
        sweep_admission_refused,
        /// T-558: attaches the run-wide chain cap ([`crate::chains::MAX_RUNTIME_CHAINS`])
        /// refused. Every chain is a thread and a retain buffer, so an uncapped survey — which
        /// meets a great many emitters — is an OOM and a thread exhaustion, not a busy device.
        admission_refused,
        /// Characterisations measured but not written: no inventory emitter covered the region
        /// within the bounded wait, so the measurement had nothing to be evidence about.
        sweep_no_emitter,
        /// T-878: classifying chains attached ([`crate::chains::classify`]). Counted apart from
        /// `attached` and from `sweep_attached`, for the reason `sweep_attached` is.
        classify_attached,
        /// Classifying chains that finished.
        classify_detached,
        /// Classifying chains the concurrency cap refused to attach.
        classify_admission_refused,
        /// Member boxes a classifying chain could not use: their samples had left its buffer, or
        /// arrived after the ring closed.
        classify_missed,
        /// Classifications the cascade abstained on upstream (the box could not be extracted or
        /// normalised), so nothing was written. Not an `unknown`: nothing was measured.
        classify_abstained,
        /// Classifications made but not written: the inventory recorded no entry for the track
        /// within the bounded wait, so the row had nothing to be evidence about.
        classify_no_emitter,
        /// Chain errors (demod, repository).
        errors,
        /// T-605: chain errors that came back from the **storage engine** — a write the database
        /// refused, counted apart from every other chain error.
        ///
        /// It is counted apart because it is not a busy radio or a signal that would not
        /// demodulate: it is a stage that could not complete, and a run that keeps going past one
        /// looks exactly like a run where that stage completed and agreed. A caught-and-printed
        /// constraint violation left `UNIQUE constraint failed: emission_features.features_id` in
        /// the log of six green gates before anyone chased it. A test can assert **zero** of
        /// these over a run; it cannot assert anything useful about `errors`, which legitimately
        /// moves for reasons a healthy run has.
        storage_errors,
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
        /// Closed because the client AFFIRMATIVELY went away: a WebSocket close frame, a
        /// hang-up, or a data message from a consumer. T-633: this counter is what an operator
        /// reads to blame their own client, so a peer the server reaped and a transport fault
        /// are counted below instead, and an end nobody attributed is not counted here at all.
        closed_client,
        /// Closed because the server stopped hearing the peer (no pong for the peer timeout) and
        /// reaped it. Nobody said the client went away (T-633).
        closed_unresponsive,
        /// Closed by a reset or a read/write error on the connection (T-633). The connection was
        /// torn down; that the client went away is a guess, so it is not counted as one.
        closed_transport,
        /// Closed with the session guard dropped and no reason reported by the transport
        /// (T-633). Unattributed is its own answer, never folded into `closed_client`.
        closed_unattributed,
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
    /// On-demand channelised IQ streams (T-165, [`crate::chains::iq`]): raw down-converted
    /// samples for an emitter or an explicit band. Metadata only (the records themselves are
    /// gated content, never counted here).
    IqCounters {
        /// Open requests.
        requests,
        /// Requests refused (bad request, unknown target, outside the window, at capacity, run
        /// ended, unrealisable).
        refused,
        /// Chains opened.
        attached,
        /// Chains closed (client gone, idle, retune off-window, segment/source ended, error).
        detached,
        /// Data records published with payload.
        records,
        /// Data records the egress gate reduced to header-only (`GATED`).
        gated,
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
    /// On-demand channelised IQ streams (T-165).
    pub iq: IqCounters,
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
    /// T-904: the detection store's size and the retention thread's last pass
    /// (`/api/status` `storage`).
    pub storage: crate::retention::StorageCounters,
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
            // T-271: what this build can and cannot follow, and why. Present in **every** run,
            // including one that found nothing — which is exactly the run where it matters, because
            // a trunked system whose control traffic cannot be followed and a band with no traffic
            // at all otherwise produce the same empty picture. Static, from
            // `hk_detect::trunk::support`: a statement about the build, not about an observation.
            "trunking": { "support": hk_detect::trunk::support_json() },
            "listen": self.listen.status_json(),
            "taps": taps,
            "iq": self.iq.to_json(),
            "budget": self.budget.status_json(),
            "chain_stats": self.chain_stats.to_json(),
            "scheduler": self.scheduler.to_json(),
            "attention": self.attention.to_json(),
            "stream_time_ns": self.stream_time_ns.load(Ordering::Relaxed),
            "tune": { "center_hz": center, "sample_rate_hz": rate },
            "compute": self.compute.to_json(),
            "observations": self.observations.to_json(),
            "storage": self.storage.to_json(),
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

/// T-605: records one chain error that came back from the **storage engine**, naming it and
/// counting it twice — once in `errors` with every other chain error, and once in
/// `storage_errors` on its own.
///
/// The second count is the point. A chain that catches a `RepoError`, prints it and carries on
/// leaves a run that looks exactly like a run where that write succeeded; the printed line is the
/// only evidence, and nobody reads the log of a green suite. `storage_errors` is a number a test
/// can require to be **zero** over a run, which `errors` can never be.
pub fn storage_error(c: &ChainCounters, what: &str, err: &hk_model::RepoError) {
    inc(&c.errors);
    inc(&c.storage_errors);
    eprintln!("hk-pipeline: {what}: {err}");
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
