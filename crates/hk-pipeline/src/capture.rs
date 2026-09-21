//! The capture thread: source → ring, at raised priority (ADR-0001 S1 condition).
//!
//! Reads native ci8 blocks (HackRF, ci8/cu8 recordings; 2 bytes/sample in the ring, and the raw
//! codes the detector's clip counts need). Other datatypes are quantised to ci8 (`round(x·128)`,
//! hk-core's normalisation) and counted. With `--loop` the source is reopened at its end and the
//! stream continues: sample indices and times are shifted past the previous pass and the first
//! block of each pass carries `GAP`, so every reader resets instead of splicing. See [`Axis`] for
//! the rule that governs that splice — capture time is one monotone axis per run (T-474).
//!
//! - **Lossless gate:** each block waits in [`crate::gate::FlowGate::wait_for_block`] with the
//!   ring's oldest and newest positions, so claims below a recording's `core:global_index` never
//!   hold it. A reopened source must still be pausable.
//! - **Virtual tuning is marked:** when the scheduler drives a replay, gain and filter changes
//!   exist only in the provenance (the samples never had them). Every block whose gains or filter
//!   differ from the first block's gets its provenance's `device_id` suffixed with
//!   [`crate::control::VIRTUAL_TUNING_DEVICE_SUFFIX`], so detections and recordings never claim a
//!   real gain state the recording did not have. (A multi-capture recording whose real gains
//!   change is over-marked, which is the safe direction.)
//! - **Coverage hold (lossless, T-037b):** after a block changes the tuned window, the capture
//!   thread publishes the tune (`tune_seq`) and, when coverage chains exist, waits until the
//!   chain manager has polled coverage for it (`coverage_seq`). A coverage chain therefore
//!   attaches and claims its samples in the flow gate before a replay shorter than the ring can
//!   be written through and closed (a 0.1 s ADS-B scene used to end with no chain). The hold is
//!   bounded by [`COVERAGE_WAIT`] and by `stop`.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use hk_core::source::SourceKind;
use hk_core::{BlockHeader, Discontinuity, ProvenanceHandle, RingWriter, Source, SourceError};
use hk_model::ProvenanceId;
use num_complex::{Complex, Complex32};

use crate::chains::spec::Trigger;
use crate::control::VIRTUAL_TUNING_DEVICE_SUFFIX;
use crate::run::{Shared, SourceFactory};
use crate::stats::{Counters, add, inc, set, thread_cpu_ns};

/// Longest coverage hold per tune change.
pub(crate) const COVERAGE_WAIT: Duration = Duration::from_secs(10);

/// Blocks between samples of the capture thread's CPU clock (T-510). One `clock_gettime` per 64
/// blocks (~0.4 s of a 20 Msps HackRF's 131 072-sample transfers) keeps the measurement off the
/// per-block path it measures, and the per-block cost is what multiplies with the number of front
/// ends a run composes: it is the only cost paid whether or not anyone looks.
pub(crate) const CPU_SAMPLE_BLOCKS: u64 = 64;

/// Waits until the chain manager has evaluated coverage for tune `seq` (see the module docs).
fn wait_for_coverage(shared: &Shared, seq: u64) {
    let c = &shared.counters;
    if c.coverage_seq.load(Ordering::SeqCst) >= seq {
        return;
    }
    inc(&c.source.coverage_waits);
    let deadline = Instant::now() + COVERAGE_WAIT;
    while c.coverage_seq.load(Ordering::SeqCst) < seq {
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        if Instant::now() >= deadline {
            inc(&c.source.coverage_wait_timeouts);
            return;
        }
        std::thread::sleep(Duration::from_micros(200));
    }
}

/// The run's capture-time axis, maintained by the capture thread (T-474).
///
/// **The decision.** A looping replay presents **one monotonically increasing capture time**. A
/// loop is a new *pass* spliced onto the same axis — index and time both continue from the end of
/// the previous pass, by the **same** offset, and the pass's first block carries `GAP` — not a new
/// epoch, and never a jump backwards. The recording's own timestamps survive in provenance and in
/// the recording; they are not the axis.
///
/// **Why this side rather than teaching every consumer to expect a wrap.** Capture time going
/// backwards is silent *data loss*, not a cosmetic glitch: `hk-store`'s spectrum-history pyramid
/// keeps one forward-only watermark and answers a frame for an already-sealed tile with
/// `IngestOutcome::Late` — counted, never errored — so rows behind it are dropped and no view can
/// ever show them again. That is measured, not feared: T-446 advanced the same watermark by an
/// hour on a retune and `frames_ingested` froze while `frames_late` climbed into the hundreds,
/// with every suite green (see [`crate::history`]'s note, and the `Late` case in `hk-store`'s
/// `history::tests`). The device contract already promises "times never go backwards" for a
/// single source (`hk_core::source::conformance`); the capture thread is the one place where
/// several sources (the passes of a `--loop`, the segments of a run) are composed into one stream,
/// so it is where that promise is kept for the composition. No reader, no pyramid and no client
/// then has to defend against a clock that runs backwards.
///
/// **Two offsets, one splice.** The index shift and the time shift are the same splice expressed
/// in the two units, so `host_time` still follows the sample counter at the tuned rate across a
/// wrap — the property the conformance suite's `timestamps` check asserts within a source.
///
/// **It splices at seams only, never per block** — see [`Axis::resume_if_rewound`] for what
/// happened when it did not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Axis {
    /// Stream index just past the last block pushed to **this segment's** ring (0 at its start,
    /// because a new segment gets a new ring).
    end_index: u64,
    /// Capture time just past the last block pushed anywhere in **this run**. Segments share it:
    /// the ring restarts, the clock does not.
    end_ns: i64,
    /// `(index, time)` offsets applied to the source's own numbers.
    shift: Option<(u64, i64)>,
}

impl Axis {
    /// A capture thread's axis, resuming the run's clock (`counters.stream_time_ns`, 0 before any
    /// block). A segment that restarts under a source whose own clock is behind — a `--loop`
    /// replay re-plumbed after some passes, say — therefore resumes where the run had reached
    /// instead of rewinding to the recording's datetime.
    fn resuming(end_ns: i64) -> Self {
        Self {
            end_index: 0,
            end_ns,
            shift: None,
        }
    }

    /// Splices a reopened source's first block onto the axis (`--loop`): one offset, applied to
    /// both units, so the new pass starts exactly where the last one ended — no overlap, and no
    /// hole either. `saturating_sub` only ever shifts *forward*: a source whose own index is
    /// already past the axis keeps its gap (flagged `GAP` below) rather than being pulled back.
    fn begin_pass(&mut self, h: &BlockHeader) {
        self.shift = Some((
            self.end_index.saturating_sub(h.time.sample_index),
            self.end_ns.saturating_sub(h.time.host_time.as_unix_nanos()),
        ));
    }

    /// The **first** block of a capture thread, when the run's clock is already past it: the
    /// segment restarted (a retune's re-plumb, a capture recovery) under a source whose own clock
    /// is behind — a `--loop` replay carried across a re-plumb has its passes' worth of offset in
    /// this thread's dead locals, not in the source. Splices it forward exactly as a pass start is,
    /// `GAP` and all, and returns `true` so the caller can count it.
    ///
    /// **Only at this seam, and deliberately.** Re-checking every block was tried and is wrong:
    /// a block's own stamp and `previous start + n/rate` are two roundings of the same instant, so
    /// they disagree by a nanosecond constantly (measured: 944 of ~950 mock blocks, every one of
    /// them exactly 1 ns). Treating that as a rewind marked a third of all blocks `GAP`, reset the
    /// STFTs and produced *no* history frames at all. Inside a source, "times never go backwards"
    /// is the source's own contract (`hk_core::source::conformance`); this is the one place where
    /// two of them meet, so it is the only place worth checking.
    fn resume_if_rewound(&mut self, h: &mut BlockHeader) -> bool {
        if self.end_ns <= 0 || h.time.host_time.as_unix_nanos() >= self.end_ns {
            return false;
        }
        self.begin_pass(h);
        h.discontinuity |= Discontinuity::GAP;
        true
    }

    /// Places `h` on the axis, in place.
    fn place(&self, h: &mut BlockHeader) {
        let Some((di, dn)) = self.shift else {
            return;
        };
        h.time.sample_index = h.time.sample_index.wrapping_add(di);
        h.time.host_time = h.time.host_time.saturating_add_nanos(dn);
    }

    /// Records where the block just pushed ended, on the axis.
    fn advance(&mut self, end_index: u64, end_ns: i64) {
        self.end_index = end_index;
        self.end_ns = end_ns;
    }
}

/// Provenance marks for a scheduler-driven replay (see the module docs).
#[derive(Default)]
struct VirtualMarks {
    base: Option<[u64; 4]>,
    marked: Vec<(ProvenanceId, ProvenanceHandle)>,
}

impl VirtualMarks {
    fn apply(&mut self, prov: &mut ProvenanceHandle, counters: &Counters) {
        let t = &prov.get().tune;
        let key = [
            t.lna_db.to_bits(),
            t.vga_db.to_bits(),
            u64::from(t.amp_on),
            t.bandwidth_hz.to_bits(),
        ];
        if *self.base.get_or_insert(key) == key {
            return;
        }
        let id = prov.id();
        if let Some((_, m)) = self.marked.iter().find(|(i, _)| *i == id) {
            *prov = m.clone();
            return;
        }
        let mut record = prov.get().clone();
        record.device_id = format!("{}{VIRTUAL_TUNING_DEVICE_SUFFIX}", record.device_id);
        let m = ProvenanceHandle::new(record);
        if self.marked.len() >= 256 {
            self.marked.clear();
        }
        self.marked.push((id, m.clone()));
        inc(&counters.scheduler.virtual_provenances);
        *prov = m;
    }
}

/// Runs until the source ends (and is not reopened) or `stop` is set; dropping the writer then
/// closes the ring, which ends every reader.
pub(crate) fn run(
    mut source: Box<dyn Source>,
    mut reopen: Option<SourceFactory>,
    mut writer: RingWriter<Complex<i8>>,
    shared: Arc<Shared>,
) -> anyhow::Result<()> {
    let c = &shared.counters.source;
    let mut ci8: Vec<Complex<i8>> = Vec::new();
    let mut f32s: Vec<Complex32> = Vec::new();
    let mut native = true;
    let mut pass_start = false;
    let mut first_block = true;
    // T-474: the run's capture clock, not this segment's. A segment that restarts (a retune's
    // re-plumb, a capture recovery) gets a new ring but the same time axis.
    let mut axis = Axis::resuming(shared.counters.stream_time_ns.load(Ordering::SeqCst));
    let mut marks = (shared.cfg.drive_scheduler
        && source.capabilities().kind == SourceKind::Replay)
        .then(VirtualMarks::default);
    // T-287: an occupancy chain attaches off the tune exactly as a coverage chain does, so it
    // needs the same hold — capture must not run past the samples the hunt will claim.
    let hold_for_coverage = shared.gate.enabled()
        && shared
            .specs
            .iter()
            .any(|s| matches!(s.trigger, Trigger::Coverage | Trigger::Occupancy));
    // T-541: whether `run::supervise` will try to **recover** from a read error here rather than
    // end the run. Exactly the predicate that decided `SupState::live` when the run started
    // (`run::Pipeline::start`), evaluated on the source this thread actually holds.
    let recoverable = shared.cfg.live_window_class && source.capabilities().controllable;
    let mut last_tune: Option<(u64, u64)> = None;
    // T-510: this thread's CPU time, added to `source.cpu_ns` as **deltas**, so the segments of a
    // re-plumbed run (each its own capture thread) accumulate into the one counter.
    let mut cpu_mark = thread_cpu_ns();
    let mut cpu_blocks = 0u64;
    let mut account_cpu = |c: &crate::stats::SourceCounters| {
        let now = thread_cpu_ns();
        add(&c.cpu_ns, now.saturating_sub(cpu_mark));
        cpu_mark = now;
    };
    let result = loop {
        if shared.stop.load(Ordering::SeqCst) {
            break Ok(());
        }
        let read = if native {
            match source.read_block_ci8(&mut ci8) {
                Err(SourceError::Unsupported { .. }) => {
                    native = false;
                    continue;
                }
                r => r,
            }
        } else {
            match source.read_block(&mut f32s) {
                Ok(Some(h)) => {
                    ci8.clear();
                    ci8.extend(f32s.iter().map(|z| {
                        let q = |x: f32| (x * 128.0).round().clamp(-128.0, 127.0) as i8;
                        Complex::new(q(z.re), q(z.im))
                    }));
                    inc(&c.quantised_blocks);
                    Ok(Some(h))
                }
                r => r,
            }
        };
        let mut h = match read {
            Ok(Some(h)) => h,
            Ok(None) => match reopen.as_mut() {
                Some(open) if !shared.stop.load(Ordering::SeqCst) => {
                    source = open()?;
                    if shared.gate.enabled() && !source.pausable() {
                        break Err(anyhow::anyhow!(
                            "the reopened source ({}) cannot pause, so it cannot run lossless",
                            source.capabilities().driver
                        ));
                    }
                    native = true;
                    pass_start = true;
                    inc(&c.loops);
                    continue;
                }
                _ => break Ok(()),
            },
            Err(e) => {
                inc(&c.read_errors);
                // **T-541: a device error ends this segment, not the stream.** `run::supervise`
                // restarts capture under the same stream ids, so a consumer arriving in the gap
                // is between windows — exactly as at a re-plumb (T-530). Without this the
                // segment's publishers called `finish()`, `/ws/spectrum/live` answered `410 Gone`
                // for the whole recovery, and `ops/stage.sh`'s `healthy()` restarted `hk serve`
                // underneath the user for a fault it was already handling. It also keeps
                // `history::run` from sealing the pyramid at a boundary the run continues past,
                // which T-446 showed is a permanent data loss rather than a cosmetic one.
                //
                // Set before the break, so it is in force before `drop(writer)` closes the ring
                // and the readers run their `finish`.
                if recoverable {
                    shared.successor_grace_ms.store(
                        crate::run::RECOVERY_SUCCESSOR_GRACE.as_millis() as u64,
                        Ordering::SeqCst,
                    );
                    shared.continues.store(true, Ordering::SeqCst);
                }
                break Err(anyhow::Error::from(e).context("reading the source"));
            }
        };
        if ci8.is_empty() {
            continue;
        }
        let fs = h.provenance.tune.sample_rate_hz;
        // **T-541: a block whose provenance cannot be true is dropped HERE, at the boundary.**
        //
        // A rate of zero or a NaN centre is not a signal condition to be handled downstream; it
        // is a corrupt USB transfer or an uninitialised driver struct, and this is the one place
        // that sees it before anything divides by it. Measured, not feared: a front end returning
        // `sample_rate_hz = 0` made `hk_detect`'s tracker compute a block duration of
        // `i64::MAX` ns and **panic** on `t0 + dur` (`track/tracker.rs`'s `observe_frame`), and
        // it would have stored `stream_time_ns = i64::MAX` — poisoning the run's capture clock
        // for every later block and every reader, permanently, from one bad transfer.
        //
        // Dropped rather than escalated to a segment failure on purpose: one corrupt block is not
        // a reason to restart capture, and `bad_blocks` makes it visible either way. A front end
        // that produces nothing else stops delivering samples, which the run already reports.
        if !(fs.is_finite() && fs > 0.0 && h.provenance.tune.center_hz.is_finite()) {
            inc(&c.bad_blocks);
            continue;
        }
        let first_of_thread = std::mem::take(&mut first_block);
        if pass_start {
            pass_start = false;
            axis.begin_pass(&h);
            // The stream did not start again: it continued. Readers reset on the `GAP` instead.
            h.discontinuity = Discontinuity::from_bits_truncate(
                (h.discontinuity.bits() & !Discontinuity::STREAM_START.bits())
                    | Discontinuity::GAP.bits(),
            );
        } else if first_of_thread && axis.resume_if_rewound(&mut h) {
            inc(&c.time_splices);
        }
        axis.place(&mut h);
        if let Some(m) = marks.as_mut() {
            m.apply(&mut h.provenance, &shared.counters);
        }
        let n = ci8.len() as u64;
        let end = h.time.sample_index + n;
        if shared.gate.enabled() {
            shared.gate.wait_for_block(
                h.time.sample_index,
                end,
                shared.ring.oldest_sample(),
                shared.ring.next_sample(),
                &shared.stop,
            );
        }
        if writer.push(&h, &ci8).is_err() {
            inc(&c.ring_errors);
            continue;
        }
        add(&c.samples, n);
        inc(&c.blocks);
        cpu_blocks += 1;
        if cpu_blocks % CPU_SAMPLE_BLOCKS == 0 {
            account_cpu(c);
        }
        add(&c.source_dropped, h.dropped_before);
        set(&c.gate_waits, shared.gate.waits());
        let end_ns = h.time.host_time.as_unix_nanos() + (n as f64 * 1e9 / fs).round() as i64;
        axis.advance(end, end_ns);
        let counters = &shared.counters;
        counters.stream_time_ns.store(end_ns, Ordering::Relaxed);
        counters
            .tune_center_bits
            .store(h.provenance.tune.center_hz.to_bits(), Ordering::Relaxed);
        counters
            .tune_rate_bits
            .store(fs.to_bits(), Ordering::Relaxed);
        // Published after the tune itself, so a poll never acknowledges a tune it did not see.
        let tune = (h.provenance.tune.center_hz.to_bits(), fs.to_bits());
        if last_tune != Some(tune) {
            last_tune = Some(tune);
            let seq = counters.tune_seq.fetch_add(1, Ordering::SeqCst) + 1;
            if hold_for_coverage {
                wait_for_coverage(&shared, seq);
            }
        }
    };
    account_cpu(c);
    drop(writer);
    result
}

#[cfg(test)]
mod tests {
    use hk_model::{ClockSource, Provenance, TimestampMethod, Tune};

    use super::*;

    fn provenance(lna_db: f64) -> ProvenanceHandle {
        ProvenanceHandle::new(Provenance {
            device_id: "sigmf:hackrf".into(),
            tune: Tune {
                center_hz: 433.92e6,
                sample_rate_hz: 2e6,
                lna_db,
                vga_db: 20.0,
                amp_on: false,
                bandwidth_hz: 1.75e6,
            },
            overload: false,
            quantisation_limited: false,
            temperature_c: None,
            antenna_port: None,
            bias_tee: hk_model::BiasTee::Unknown,
            clock_source: ClockSource::Internal,
            clock_locked: true,
            calibration_state_ref: None,
            spur_mask_ref: None,
            timestamp_method: TimestampMethod::Unknown,
            timestamp_error_budget_ns: None,
            capture_artefacts: Vec::new(),
        })
    }

    #[test]
    fn virtual_gain_changes_are_marked_and_the_recorded_state_is_not() {
        let counters = Counters::default();
        let mut marks = VirtualMarks::default();
        let real = provenance(24.0);
        let mut p = real.clone();
        marks.apply(&mut p, &counters);
        assert_eq!(
            p.id(),
            real.id(),
            "the recording's own gain state is unchanged"
        );
        let stepped = provenance(32.0);
        let mut a = stepped.clone();
        marks.apply(&mut a, &counters);
        assert_eq!(a.device_id, "sigmf:hackrf+virtual-tuning");
        assert_eq!(a.tune.lna_db, 32.0);
        let mut b = stepped.clone();
        marks.apply(&mut b, &counters);
        assert_eq!(a.id(), b.id(), "one marked handle per source handle");
        let mut back = real.clone();
        marks.apply(&mut back, &counters);
        assert_eq!(back.device_id, "sigmf:hackrf");
        assert_eq!(
            counters
                .scheduler
                .virtual_provenances
                .load(Ordering::Relaxed),
            1
        );
    }

    // ---- T-474: the capture-time axis ----

    /// The pretend recording: 5 blocks of 100 k samples at 2 Msps — 0.25 s a pass.
    const AX_FS: f64 = 2e6;
    const AX_BLOCK: u64 = 100_000;
    const AX_BLOCKS: u64 = 5;
    const AX_PASS_NS: i64 = 250_000_000;
    /// The recording's own datetime: every pass starts here, because a reopened source starts the
    /// recording again. That is the fact this axis exists to absorb.
    const AX_T0: i64 = 1_789_000_000_000_000_000;

    fn ax_raw(index: u64, stream_start: bool) -> BlockHeader {
        BlockHeader {
            time: hk_model::SampleTime {
                sample_index: index,
                host_time: hk_model::Timestamp::from_unix_nanos(
                    AX_T0 + (index as f64 * 1e9 / AX_FS).round() as i64,
                ),
            },
            provenance: provenance(24.0),
            discontinuity: if stream_start {
                Discontinuity::STREAM_START
            } else {
                Discontinuity::NONE
            },
            dropped_before: 0,
        }
    }

    /// Drives `axis` through `passes` passes of the recording exactly as [`run`] does, returning
    /// each placed header and whether it had to be rescued forward.
    fn ax_drive(axis: &mut Axis, passes: u64) -> Vec<(BlockHeader, bool)> {
        let mut out = Vec::new();
        for p in 0..passes {
            for b in 0..AX_BLOCKS {
                let mut h = ax_raw(b * AX_BLOCK, p == 0 && b == 0);
                let mut rescued = false;
                if p > 0 && b == 0 {
                    axis.begin_pass(&h);
                    h.discontinuity = Discontinuity::from_bits_truncate(
                        (h.discontinuity.bits() & !Discontinuity::STREAM_START.bits())
                            | Discontinuity::GAP.bits(),
                    );
                } else if p == 0 && b == 0 {
                    rescued = axis.resume_if_rewound(&mut h);
                }
                axis.place(&mut h);
                let end = h.time.sample_index + AX_BLOCK;
                let end_ns = h.time.host_time.as_unix_nanos()
                    + (AX_BLOCK as f64 * 1e9 / AX_FS).round() as i64;
                axis.advance(end, end_ns);
                out.push((h, rescued));
            }
        }
        out
    }

    /// The decision, asserted: a looping replay is one monotone axis, and the splice is exact in
    /// both units. A wrap that moved time backwards would be silently dropped by the history
    /// pyramid (see [`Axis`]), so "monotone" is a data-integrity claim, not a cosmetic one.
    #[test]
    fn a_looping_replay_presents_one_monotone_capture_time_axis() {
        let mut axis = Axis::resuming(0);
        let placed = ax_drive(&mut axis, 3);
        let per_block_ns = (AX_BLOCK as f64 * 1e9 / AX_FS).round() as i64;
        let (first, _) = &placed[0];
        let t0 = first.time.host_time.as_unix_nanos();
        let i0 = first.time.sample_index;
        for (k, (h, rescued)) in placed.iter().enumerate() {
            assert!(
                !rescued,
                "block {k} needed rescuing: the splice was not exact"
            );
            let t = h.time.host_time.as_unix_nanos();
            let i = h.time.sample_index;
            // Contiguous in both units, with no hole and no overlap at a wrap.
            assert_eq!(
                t,
                t0 + k as i64 * per_block_ns,
                "block {k}: capture time is not contiguous across the wrap"
            );
            assert_eq!(
                i,
                i0 + k as u64 * AX_BLOCK,
                "block {k}: index is not contiguous"
            );
            // The two units carry the SAME splice: time still follows the sample counter at the
            // tuned rate, which is what the device contract promises within one source.
            assert_eq!(
                t - t0,
                ((i - i0) as f64 * 1e9 / AX_FS).round() as i64,
                "block {k}: time and the sample counter disagree"
            );
        }
        // Three passes of it, and the axis really did advance by three passes.
        assert_eq!(
            axis.end_ns - t0,
            3 * AX_PASS_NS,
            "the passes were spliced onto the axis, not laid on top of each other"
        );
        // Each wrap is marked, and the stream started exactly once.
        for (k, (h, _)) in placed.iter().enumerate() {
            let wrap = k as u64 % AX_BLOCKS == 0 && k > 0;
            assert_eq!(
                h.discontinuity.contains(Discontinuity::GAP),
                wrap,
                "block {k}: GAP should mark a wrap and nothing else"
            );
            assert_eq!(
                h.discontinuity.contains(Discontinuity::STREAM_START),
                k == 0,
                "block {k}: the stream starts once, and a wrap is not a start"
            );
        }
    }

    /// A segment that restarts (a retune's re-plumb, a capture recovery) gets a new ring but not a
    /// new clock: the source it inherits has been looping and its own timestamps are back at the
    /// recording's datetime, hundreds of seconds behind what the run has published. Before T-474
    /// the splice lived in the capture thread's locals and died with the segment, so the clock
    /// rewound there — into already-sealed tiles, where frames are dropped as late.
    #[test]
    fn a_segment_that_restarts_under_a_rewound_source_never_rewinds_the_clock() {
        let mut first = Axis::resuming(0);
        let a = ax_drive(&mut first, 4);
        let reached = first.end_ns;
        assert_eq!(reached, AX_T0 + 4 * AX_PASS_NS);

        // The new segment: a new ring (so `end_index` starts at 0) and the run's clock.
        let mut next = Axis::resuming(reached);
        let b = ax_drive(&mut next, 2);
        assert!(b[0].1, "the rewound first block was not rescued");
        assert_eq!(
            b[0].0.time.host_time.as_unix_nanos(),
            reached,
            "the new segment must resume where the run had reached"
        );
        assert!(
            b[0].0.discontinuity.contains(Discontinuity::GAP),
            "a rescued block is a discontinuity: readers must reset, not splice the waveform"
        );
        let mut prev = a.last().unwrap().0.time.host_time.as_unix_nanos();
        for (k, (h, _)) in b.iter().enumerate() {
            let t = h.time.host_time.as_unix_nanos();
            assert!(
                t > prev,
                "block {k} of the new segment moved capture time backwards"
            );
            prev = t;
        }
        assert_eq!(
            next.end_ns,
            reached + 2 * AX_PASS_NS,
            "the new segment's passes land after everything the run had published"
        );
    }

    /// **The rounding-noise case, which this nearly got wrong.** A block's own stamp and
    /// `previous start + n/rate` are two roundings of one instant, so they disagree by a
    /// nanosecond constantly — measured on the mock SDR at 944 of ~950 blocks, every one of them
    /// exactly 1 ns. An earlier draft checked monotonicity on *every* block and duly "rescued"
    /// each of those: a third of all blocks were marked `GAP`, every reader's STFT reset, and the
    /// run folded **no history frames at all** (`hk-cli`'s decoded-capture contract test timed out
    /// waiting for frames). Inside a pass the source's own contract governs and nothing here
    /// second-guesses it.
    #[test]
    fn a_nanosecond_of_rounding_noise_inside_a_pass_is_not_a_rewind() {
        let block_ns = (AX_BLOCK as f64 * 1e9 / AX_FS).round() as i64;
        let mut axis = Axis::resuming(0);
        let mut h0 = ax_raw(0, true);
        axis.place(&mut h0);
        axis.advance(AX_BLOCK, h0.time.host_time.as_unix_nanos() + block_ns);
        // The next block stamps itself 1 ns before the end the axis derived for the previous one.
        let mut h1 = ax_raw(AX_BLOCK, false);
        let jittered = h1.time.host_time.as_unix_nanos() - 1;
        h1.time.host_time = hk_model::Timestamp::from_unix_nanos(jittered);
        axis.place(&mut h1);
        assert_eq!(
            h1.time.host_time.as_unix_nanos(),
            jittered,
            "a nanosecond of rounding noise is not a rewind and must not move the block"
        );
        assert!(
            !h1.discontinuity.contains(Discontinuity::GAP),
            "rounding noise must not be marked as a discontinuity: every reader would reset"
        );
    }

    /// The seam check is a floor, not a clamp: a source whose clock is legitimately ahead — a live
    /// radio after a gap, a recording that starts later — is left exactly as it is.
    #[test]
    fn a_source_whose_clock_is_ahead_is_left_alone() {
        let mut axis = Axis::resuming(AX_T0 - 60_000_000_000);
        let placed = ax_drive(&mut axis, 1);
        assert!(placed.iter().all(|(_, r)| !r), "nothing needed rescuing");
        assert_eq!(placed[0].0.time.host_time.as_unix_nanos(), AX_T0);
        assert_eq!(placed[0].0.time.sample_index, 0);
        assert!(
            !placed[0].0.discontinuity.contains(Discontinuity::GAP),
            "an untouched block is not a discontinuity"
        );
    }
}
