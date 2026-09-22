//! Observation log wiring (T-115, ADR-0012 §1): collects applied steps and analysed extents from
//! the control thread and hands records to `hk_store::observation` on a bounded queue that never
//! blocks the pipeline. T-115 also owns the single observer call site in `control.rs`.
//!
//! - **[`ObservationLog`]** (one per run, across `--loop` segments): the store under
//!   `<data>/observations`, its writer thread, and the `observations` stream (ADR-0004 `messages`
//!   kind, metadata only: `dwell` and `sweep-summary` records), published from the writer thread.
//! - **[`Observer`]** (one per segment's scheduler): [`Observer::tick`] runs at the top of every
//!   control-thread tick. It sees the newest applied step; when a new step appears the previous
//!   one closes at the new step's start. A step is **settled** at its start when the analysed data
//!   already carries its tuning, else at the stream time of the first tick whose published tune
//!   (`Counters::tune_center_bits`/`tune_rate_bits`, set by the capture thread from block
//!   headers) matches it; that is at most one block late. A source that cannot retune (a replay
//!   behind the replay guard) observes its own window whatever the step asked, and the record
//!   says so. Dropped samples are the source's and the detection reader's losses over the step.
//!   Every time is stream (sample-clock) time.
//! - **Never blocks:** records are offered with `try_send`; a full queue drops and counts
//!   (`/observations/records_dropped`). Serialisation, file writes and stream publishing happen
//!   on the writer thread.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use hk_core::scheduler::observe::{ObservationRecorder, StepObservation, WindowRule};
use hk_core::scheduler::{CompiledPlan, ScheduleStep};
use hk_model::attention::ATTENTION_SCHEMA_VERSION;
use hk_model::attention::baseline::SiteKey;
use hk_model::attention::observation::{DwellRecord, ObservationRecord, Reason};
use hk_model::{ContentClass, SurveyId, TimeRange, Timestamp};
use hk_store::observation::{
    ObservationLogConfig, ObservationQueue, ObservationStore, ObservationWriter, RecordTap,
};
use hk_stream::{MessageRecord, Publisher, PublisherConfig, StreamHeader, StreamKind};
use serde_json::{Value, json};

use crate::config::StreamSink;
use crate::stats::{Counters, inc};

/// Stream id of the observations stream.
pub const OBSERVATIONS_STREAM_ID: &str = "observations";
/// Message schema of the observations stream.
pub const OBSERVATIONS_MESSAGE_SCHEMA: &str = "hackriff.observation/1";
/// Equivalent noise bandwidth of the Hann analysis window, bins.
pub const HANN_ENBW_BINS: f64 = 1.5;
/// Half-width of the DC notch the observed extent excludes, Hz: the detector's DC-spur tolerance
/// (`hk_detect::DcRule::default().tolerance_hz`).
pub const DC_NOTCH_HALF_HZ: f64 = 15e3;
/// Half-width, bins, of the DC notch the display and history frames interpolate across (T-524):
/// the Hann main lobe of the LO-leakage tone (a pure DC tone reads in bins ±1 and nulls at ±2).
/// Deliberately **not** [`DC_NOTCH_HALF_HZ`]: that is the detector's tolerance, and interpolating
/// ±15 kHz of history smears neighbouring channels' real bursts across the centre (AWARE-042 ch3
/// at 446.04375 MHz, next to a 446.05 MHz tune centre, read 0.436 occupancy against truth 0.294).
pub const DC_INTERP_HALF_BINS: usize = 2;

/// The run's observation log (see the module docs).
pub struct ObservationLog {
    writer: ObservationWriter,
    /// The front end every record of this log names (T-378): the source's own
    /// `DeviceInfo::device_id`, which is also what `ChainKey::of_device` hashes into the run's
    /// receive chain and `hk_store::history::source_key` hashes into each frame's history origin.
    /// `None` for a source that states no identity, and then the records say nothing about which
    /// radio looked rather than borrowing the one that happens to be running.
    device_id: Option<String>,
}

impl ObservationLog {
    /// Opens the log under `<data_dir>/observations` and starts its writer; offers the
    /// `observations` stream through `sink`. `device_id` is the front end the records name.
    ///
    /// `retention` is the run's `(days, MiB)` override (T-406,
    /// `ScanPlan.extra.pipeline.observation_retention_days` / `observation_max_mb`); `(None, None)`
    /// keeps the defaults, which `docs/16` §5.4 sizes so the coverage record outlives the
    /// spectrum-history pyramid it explains.
    pub(crate) fn open(
        data_dir: &Path,
        device_id: Option<String>,
        sink: Option<&StreamSink>,
        retention: (Option<f64>, Option<f64>),
    ) -> anyhow::Result<Self> {
        Self::open_with(
            ObservationLogConfig::new(data_dir.join("observations"))
                .with_retention(retention.0, retention.1),
            device_id,
            sink,
        )
    }

    /// [`ObservationLog::open`] with explicit log settings.
    pub(crate) fn open_with(
        config: ObservationLogConfig,
        device_id: Option<String>,
        sink: Option<&StreamSink>,
    ) -> anyhow::Result<Self> {
        let store = ObservationStore::open(config)?;
        let tap = match sink {
            Some(sink) => {
                let mut header = StreamHeader::new(
                    OBSERVATIONS_STREAM_ID,
                    StreamKind::Messages,
                    // Scheduler metadata (where and when the radio looked), never RF content.
                    ContentClass::Unrestricted,
                    format!("hk-pipeline:observations@{}", env!("CARGO_PKG_VERSION")),
                );
                header.message_schema = Some(OBSERVATIONS_MESSAGE_SCHEMA.into());
                header.max_frame_len = 64 * 1024;
                let publisher = Publisher::new(header.clone(), PublisherConfig::default())?;
                sink(&header, publisher.handle());
                Some(stream_tap(publisher))
            }
            None => None,
        };
        Ok(Self {
            writer: ObservationWriter::spawn(store, tap)?,
            device_id,
        })
    }

    /// The store (queries).
    pub fn store(&self) -> ObservationStore {
        self.writer.store().clone()
    }

    /// An observer for one segment's scheduler.
    pub(crate) fn observer(
        &self,
        plan: &CompiledPlan,
        fft_bins: usize,
        fixed_tuning: Option<(f64, f64)>,
        survey_id: Option<SurveyId>,
        counters: Arc<Counters>,
    ) -> Observer {
        Observer {
            recorder: ObservationRecorder::new(
                plan,
                rule(fft_bins),
                fixed_tuning,
                survey_id,
                self.device_id.clone(),
            ),
            queue: self.writer.queue(),
            store: self.store(),
            device_id: self.device_id.clone(),
            fixed: fixed_tuning.is_some(),
            counters,
            current: None,
            last_now_ns: 0,
        }
    }

    /// An interactive observer for one segment of a live run without the scheduler. Create it
    /// before the segment's capture thread starts.
    pub(crate) fn interactive(
        &self,
        fft_bins: usize,
        survey_id: Option<SurveyId>,
        counters: Arc<Counters>,
    ) -> InteractiveObserver {
        let device_id = self.device_id.clone();
        self.interactive_for(device_id, fft_bins, survey_id, counters)
    }

    /// [`Self::interactive`] naming a **different** front end (T-510): a multi-source run has one
    /// observation log but N radios, and the coverage map is computed per device from these
    /// records ([`hk_store::coverage::record_device`]). A further front end's dwell must therefore
    /// say *its* `device_id`, not the log's — otherwise the primary's coverage silently answers
    /// for a band only the second radio looked at, and "grey = genuinely unobserved" stops being
    /// true. `counters` must be that front end's own, because the tune and stream time the
    /// observer reads are per front end.
    pub(crate) fn interactive_for(
        &self,
        device_id: Option<String>,
        fft_bins: usize,
        survey_id: Option<SurveyId>,
        counters: Arc<Counters>,
    ) -> InteractiveObserver {
        InteractiveObserver {
            rule: rule(fft_bins),
            survey_id,
            store: self.store(),
            device_id,
            queue: self.writer.queue(),
            seq0: counters.tune_seq.load(Ordering::SeqCst),
            counters,
            open: None,
            last_now_ns: 0,
            records: 0,
        }
    }
}

fn rule(fft_bins: usize) -> WindowRule {
    WindowRule {
        fft_bins,
        dc_half_hz: DC_NOTCH_HALF_HZ,
        enbw_bins: HANN_ENBW_BINS,
    }
}

/// Publishes `dwell` and `sweep-summary` messages (metadata only) from the writer thread.
fn stream_tap(mut publisher: Publisher) -> RecordTap {
    let mut extents: HashMap<u64, Vec<(f64, f64)>> = HashMap::new();
    Box::new(move |rec: &ObservationRecord| {
        let (t, metadata) = match rec {
            ObservationRecord::Geometry(g) => {
                if extents.len() > 16 {
                    extents.clear();
                }
                let hops = g.hops.iter().map(|w| (w.usable.lo_hz, w.usable.hi_hz));
                extents.insert(g.id, hops.collect());
                return;
            }
            ObservationRecord::Dwell(d) => (
                d.observed.end,
                json!({
                    "kind": "dwell",
                    "reason_text": d.reason.text(),
                    "record": d,
                }),
            ),
            ObservationRecord::Sweep(s) => {
                let (lo, hi) = extents.get(&s.geometry).map_or((None, None), |hops| {
                    let visited = s.visits.iter().filter_map(|v| hops.get(v.hop as usize));
                    let (lo, hi) = visited.fold((f64::INFINITY, f64::NEG_INFINITY), |a, h| {
                        (a.0.min(h.0), a.1.max(h.1))
                    });
                    (lo.is_finite().then_some(lo), hi.is_finite().then_some(hi))
                });
                (
                    s.span.end,
                    json!({
                        "kind": "sweep-summary",
                        "plan_version": s.plan_version,
                        "geometry": s.geometry,
                        "t0_s": seconds(s.span.start),
                        "t1_s": seconds(s.span.end),
                        "visits": s.visits.len(),
                        "observed_s": s.visits.iter().map(|v| f64::from(v.observed_ms)).sum::<f64>() * 1e-3,
                        "f_lo_hz": lo,
                        "f_hi_hz": hi,
                        "preempted_hops": s.preempted_hops,
                        "dropped_samples": s.dropped_samples,
                        "overload_hops": s.overload_hops,
                    }),
                )
            }
        };
        let msg = MessageRecord {
            t,
            emitter_id: None,
            provenance_ref: None,
            content_class: ContentClass::Unrestricted,
            decode_id: None,
            annotation_id: None,
            decoder: None,
            frame_model: Some("observation".into()),
            crc_status: None,
            identity: None,
            metadata,
            content: None,
        };
        // A slow subscriber loses messages (the publisher's own drop policy), never the log.
        let _ = publisher.publish_message(&msg);
    })
}

fn seconds(t: Timestamp) -> Value {
    json!(t.as_unix_nanos() as f64 * 1e-9)
}

struct InFlight {
    step: ScheduleStep,
    settled_ns: Option<i64>,
    tune_seq: u64,
    dropped0: u64,
}

/// Turns one segment's applied steps into records (see the module docs).
pub(crate) struct Observer {
    recorder: ObservationRecorder,
    queue: ObservationQueue,
    /// T-596: the log's open-dwell slot, so the step in flight is evidence of coverage before it
    /// seals. See [`hk_store::observation::ObservationStore::note_open_dwell`].
    store: ObservationStore,
    device_id: Option<String>,
    fixed: bool,
    counters: Arc<Counters>,
    current: Option<InFlight>,
    last_now_ns: i64,
}

impl Observer {
    /// Runs at the top of each control tick with the newest applied step.
    pub(crate) fn tick(&mut self, now_ns: i64, latest: Option<&ScheduleStep>) {
        self.last_now_ns = self.last_now_ns.max(now_ns);
        if let Some(step) = latest {
            if self.current.as_ref().is_none_or(|c| c.step.seq != step.seq) {
                self.close(step.t_start.as_unix_nanos());
                // A long non-sweep step closes the open sweep record at its start.
                let (queue, counters) = (&self.queue, &self.counters);
                self.recorder
                    .begin(step, &mut |r| offer(queue, counters, r));
                self.current = Some(InFlight {
                    step: *step,
                    settled_ns: None,
                    tune_seq: self.counters.tune_seq.load(Ordering::Relaxed),
                    dropped0: self.dropped(),
                });
            }
        }
        let (center, rate) = self.tune();
        let fixed = self.fixed;
        let seq_now = self.counters.tune_seq.load(Ordering::Relaxed);
        if let Some(c) = self.current.as_mut().filter(|c| c.settled_ns.is_none()) {
            let tuned =
                (center - c.step.center_hz).abs() <= 1.0 && (rate - c.step.rate_hz).abs() <= 1.0;
            if fixed || (tuned && seq_now == c.tune_seq) {
                // Already analysing this tuning (or never able to change it).
                c.settled_ns = Some(c.step.t_start.as_unix_nanos());
            } else if tuned {
                c.settled_ns = Some(now_ns.max(c.step.t_start.as_unix_nanos()));
            }
        }
        self.publish_open();
    }

    /// T-596: the step in flight, published to the log's open-dwell slot so the live edge is
    /// covered between the retune and the seal. Replaced every tick (one slot per front end), and
    /// dropped by [`Self::close`] the instant the sealed record can speak for it.
    fn publish_open(&self) {
        let Some(c) = self.current.as_ref() else {
            return;
        };
        let (center, rate) = if self.fixed {
            self.tune()
        } else {
            (c.step.center_hz, c.step.rate_hz)
        };
        let obs = StepObservation {
            step: c.step,
            center_hz: center,
            rate_hz: rate,
            settled: c.settled_ns.map(Timestamp::from_unix_nanos),
            end: Timestamp::from_unix_nanos(self.last_now_ns),
            dropped_samples: self.dropped().saturating_sub(c.dropped0),
            overload: false,
        };
        match self.recorder.open_dwell(&obs) {
            Some(rec) => self.store.note_open_dwell(rec),
            None => self.store.clear_open_dwell(self.device_id.as_deref()),
        }
    }

    fn tune(&self) -> (f64, f64) {
        (
            f64::from_bits(self.counters.tune_center_bits.load(Ordering::Relaxed)),
            f64::from_bits(self.counters.tune_rate_bits.load(Ordering::Relaxed)),
        )
    }

    fn dropped(&self) -> u64 {
        lost_samples(&self.counters)
    }

    fn close(&mut self, end_ns: i64) {
        let Some(c) = self.current.take() else { return };
        // The sealed record now speaks for this window; the provisional one must not double up.
        self.store.clear_open_dwell(self.device_id.as_deref());
        let (center, rate) = if self.fixed {
            self.tune()
        } else {
            (c.step.center_hz, c.step.rate_hz)
        };
        let obs = StepObservation {
            step: c.step,
            center_hz: center,
            rate_hz: rate,
            settled: c.settled_ns.map(Timestamp::from_unix_nanos),
            end: Timestamp::from_unix_nanos(end_ns),
            dropped_samples: self.dropped().saturating_sub(c.dropped0),
            overload: false,
        };
        let (queue, counters) = (&self.queue, &self.counters);
        self.recorder
            .observe(&obs, &mut |r| offer(queue, counters, r));
    }
}

fn offer(queue: &ObservationQueue, counters: &Counters, rec: ObservationRecord) {
    inc(&counters.observations.records_offered);
    if !queue.offer(rec) {
        inc(&counters.observations.records_dropped);
    }
}

impl Drop for Observer {
    /// The segment ended: the in-flight step closes at the last stream time seen, and the open
    /// sweep record is flushed.
    fn drop(&mut self) {
        let end = self.last_now_ns;
        self.close(end);
        let (queue, counters) = (&self.queue, &self.counters);
        self.recorder.flush(&mut |r| offer(queue, counters, r));
    }
}

/// Samples dropped so far by the source and the detection reader.
fn lost_samples(c: &Counters) -> u64 {
    c.source.source_dropped.load(Ordering::Relaxed)
        + c.detect_reader.lost_samples.load(Ordering::Relaxed)
}

/// Longest interactive dwell record, ns: a steady tune is split so the log stays fresh and every
/// record fits the queries' one-hour look-ahead.
pub const INTERACTIVE_RECORD_MAX_NS: i64 = 60_000_000_000;

struct OpenTune {
    center_bits: u64,
    rate_bits: u64,
    intent: u64,
    start_ns: i64,
    dropped0: u64,
}

/// Records the tuning of a live run without the scheduler (interactive `hk serve`): one
/// [`DwellRecord`] at the `interactive` tier per steady tune, closed when the published tune
/// changes and split every [`INTERACTIVE_RECORD_MAX_NS`] of stream time. Interactive visits add
/// coverage and observed seconds but are never activity independent (ADR-0012 §2.5).
///
/// Polled on the control thread; it only reads what the capture thread already publishes
/// (`tune_seq`, the tune, `stream_time_ns`), so the capture path is unchanged. Boundaries are
/// the stream time of the last poll under the old tune (at most one poll late); the reason's
/// intent id is the `tune_seq` that first published the tune.
pub(crate) struct InteractiveObserver {
    rule: WindowRule,
    survey_id: Option<SurveyId>,
    /// T-596: the log's open-dwell slot (see [`Observer::publish_open`]). This is the path that
    /// T-588 measured: an interactive record seals only when the tune changes or after
    /// [`INTERACTIVE_RECORD_MAX_NS`], so without it the log says nothing about the band the radio
    /// is sitting on for up to a minute after every retune.
    store: ObservationStore,
    /// The front end this observer's records name (T-378); see `ObservationLog::device_id`.
    device_id: Option<String>,
    queue: ObservationQueue,
    counters: Arc<Counters>,
    /// `tune_seq` before this segment's capture started: nothing is recorded until it moves.
    seq0: u64,
    open: Option<OpenTune>,
    last_now_ns: i64,
    records: u64,
}

impl InteractiveObserver {
    /// One control-thread poll.
    pub(crate) fn tick(&mut self) {
        let c = &self.counters;
        let seq = c.tune_seq.load(Ordering::SeqCst);
        if seq == self.seq0 {
            return;
        }
        // The capture thread stores the time and tune before bumping `tune_seq`.
        let center_bits = c.tune_center_bits.load(Ordering::Relaxed);
        let rate_bits = c.tune_rate_bits.load(Ordering::Relaxed);
        let now = c.stream_time_ns.load(Ordering::Relaxed);
        if now <= 0
            || f64::from_bits(rate_bits).partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater)
        {
            return;
        }
        let now = now.max(self.last_now_ns);
        match &self.open {
            Some(o) if o.center_bits == center_bits && o.rate_bits == rate_bits => {
                if now - o.start_ns >= INTERACTIVE_RECORD_MAX_NS {
                    let intent = o.intent;
                    self.close(now);
                    self.start(center_bits, rate_bits, intent, now);
                }
            }
            Some(_) => {
                let at = self.last_now_ns;
                self.close(at);
                self.start(center_bits, rate_bits, seq, at);
            }
            None => self.start(center_bits, rate_bits, seq, now),
        }
        self.last_now_ns = now;
        self.publish_open(now);
    }

    /// T-596: the tune in flight, published to the log's open-dwell slot.
    fn publish_open(&self, now_ns: i64) {
        match self.open.as_ref().and_then(|o| self.record(o, now_ns)) {
            Some(rec) => self.store.note_open_dwell(rec),
            None => self.store.clear_open_dwell(self.device_id.as_deref()),
        }
    }

    /// The dwell record for `o` ending at `end_ns`, or `None` when no time has passed. One
    /// builder, so the provisional record and the sealed one cannot drift apart: the span is the
    /// only difference between them.
    fn record(&self, o: &OpenTune, end_ns: i64) -> Option<DwellRecord> {
        if end_ns <= o.start_ns {
            return None;
        }
        let span = TimeRange::new(
            Timestamp::from_unix_nanos(o.start_ns),
            Timestamp::from_unix_nanos(end_ns),
        );
        let reason = Reason::Interactive { intent: o.intent };
        Some(DwellRecord {
            schema: ATTENTION_SCHEMA_VERSION,
            survey_id: self.survey_id,
            seq: self.records,
            plan_version: 0,
            site: SiteKey::Unassigned,
            device_id: self.device_id.clone(),
            reason,
            tier: reason.tier(),
            window: self
                .rule
                .window(f64::from_bits(o.center_bits), f64::from_bits(o.rate_bits)),
            rf_path: 0,
            planned: span,
            observed: span,
            preempted: false,
            dropped_samples: lost_samples(&self.counters).saturating_sub(o.dropped0),
            overload: false,
            provenance_ref: None,
        })
    }

    fn start(&mut self, center_bits: u64, rate_bits: u64, intent: u64, start_ns: i64) {
        self.open = Some(OpenTune {
            center_bits,
            rate_bits,
            intent,
            start_ns,
            dropped0: lost_samples(&self.counters),
        });
    }

    fn close(&mut self, end_ns: i64) {
        let Some(o) = self.open.take() else { return };
        // The sealed record now speaks for this window (T-596); drop the provisional one first so
        // no reader can ever see both.
        self.store.clear_open_dwell(self.device_id.as_deref());
        let Some(rec) = self.record(&o, end_ns) else {
            return;
        };
        self.records += 1;
        offer(&self.queue, &self.counters, ObservationRecord::Dwell(rec));
    }
}

impl Drop for InteractiveObserver {
    /// The segment ended: the open tune closes at the last stream time seen.
    fn drop(&mut self) {
        self.close(self.last_now_ns);
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use hk_core::SourceCapabilities;
    use hk_core::scheduler::{Purpose, Scheduler, SchedulerConfig, SyntheticClock};
    use hk_model::FreqRange;
    use hk_store::observation::RecordQuery;

    use super::*;

    /// A `DeviceInfo::device_id` spelling, as a real source states it.
    const TEST_DEVICE: &str = "mock:hackrf-one";

    #[test]
    fn observe_a_long_intent_between_sweeps_leaves_the_sweep_queryable_in_its_own_hour() {
        let dir = std::env::temp_dir().join(format!("hk-t115-intent-{}", std::process::id()));
        let log = ObservationLog::open_with(
            ObservationLogConfig::new(dir.join("observations")),
            Some(TEST_DEVICE.into()),
            None,
        )
        .unwrap();
        let store = log.store();
        // 10 min into an hour: a 2 h intent ends two hours later.
        let t0 = Timestamp::from_unix_nanos(1_789_297_800_000_000_000);
        let plan = crate::config::replay_plan(100e6, 2e6, t0);
        let mut sc = SchedulerConfig::from_plan(&plan).unwrap();
        sc.sweep_rate_hz = 2e6;
        sc.dwell_min_rate_hz = 2e6;
        sc.max_span_hz = 2e6;
        let caps = SourceCapabilities::hackrf_one();
        let mut s = Scheduler::new(&plan, sc, &caps, SyntheticClock::new(t0)).unwrap();
        let mut steps = Vec::new();
        s.run_synthetic(2_000, &mut steps);
        let tpl = *steps
            .iter()
            .find(|st| matches!(st.purpose, Purpose::Sweep { .. }))
            .expect("a sweep hop");
        let counters = Arc::new(Counters::default());
        counters
            .tune_center_bits
            .store(tpl.center_hz.to_bits(), Ordering::Relaxed);
        counters
            .tune_rate_bits
            .store(tpl.rate_hz.to_bits(), Ordering::Relaxed);
        let mut obs = log.observer(s.plan(), 1024, None, None, Arc::clone(&counters));

        let hour_ns = 3_600_000_000_000;
        let mut hop = tpl;
        hop.seq = 1;
        hop.t_start = t0.saturating_add_nanos(1_000_000_000);
        hop.duration_ns = 50_000_000;
        let mut intent = hop;
        intent.seq = 2;
        intent.t_start = hop.t_end();
        intent.duration_ns = 2 * hour_ns;
        intent.purpose = Purpose::UserIntent { intent: 9 };
        let mut next = hop;
        next.seq = 3;
        next.t_start = intent.t_end();
        for st in [&hop, &intent, &next] {
            obs.tick(st.t_start.as_unix_nanos(), Some(st));
        }
        drop(obs);
        drop(log);

        let span = TimeRange::new(hop.t_start, hop.t_end());
        let page = store.query(&RecordQuery {
            freq: FreqRange::new(0.0, 7e9),
            span,
            tier: None,
            cursor: 0,
            limit: 100,
        });
        assert!(
            page.records
                .iter()
                .any(|r| matches!(r, ObservationRecord::Sweep(sw) if sw.span.end == hop.t_end())),
            "the sweep before the intent is found in its own hour: {page:?}"
        );
        let ch = FreqRange::centered(tpl.center_hz + 200e3, 1e3);
        assert_eq!(store.observations_of(ch, span).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn observe_stalled_writer_drops_and_counts_without_stalling_the_control_thread() {
        let dir = std::env::temp_dir().join(format!("hk-t115-stall-{}", std::process::id()));
        let mut cfg = ObservationLogConfig::new(dir.join("observations"));
        cfg.queue_len = 4;
        let log = ObservationLog::open_with(cfg, Some(TEST_DEVICE.into()), None).unwrap();
        let t0 = Timestamp::from_unix_nanos(1_789_297_800_000_000_000);
        let plan = crate::config::replay_plan(100e6, 2e6, t0);
        let mut sc = SchedulerConfig::from_plan(&plan).unwrap();
        sc.sweep_rate_hz = 2e6;
        sc.dwell_min_rate_hz = 2e6;
        sc.max_span_hz = 2e6;
        let caps = SourceCapabilities::hackrf_one();
        let mut s = Scheduler::new(&plan, sc, &caps, SyntheticClock::new(t0)).unwrap();
        let counters = Arc::new(Counters::default());
        let mut obs = log.observer(s.plan(), 1024, None, None, Arc::clone(&counters));
        let mut steps = Vec::new();
        s.run_synthetic(20_000, &mut steps);

        let store = log.store();
        let guard = store.stall();
        let started = Instant::now();
        for st in &steps {
            obs.tick(st.t_start.as_unix_nanos(), Some(st));
        }
        drop(obs);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the observer waited on a stalled writer"
        );
        let c = &counters.observations;
        let (offered, dropped) = (
            c.records_offered.load(Ordering::Relaxed),
            c.records_dropped.load(Ordering::Relaxed),
        );
        assert!(offered > 1000, "{offered} records offered");
        assert!(dropped >= offered - 5, "{dropped} of {offered} dropped");
        assert_eq!(store.stats().dropped.load(Ordering::Relaxed), dropped);

        drop(guard);
        drop(log);
        assert_eq!(
            store.stats().written.load(Ordering::Relaxed) + dropped,
            offered,
            "every record not dropped was written"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **T-596 — the dwell in flight is evidence of coverage before it seals.**
    ///
    /// An interactive record is written when the tune changes or after
    /// [`INTERACTIVE_RECORD_MAX_NS`] (60 s) of a steady tune, so for up to a minute after every
    /// retune the log holds nothing for the band the radio is sitting on. With the IQ ring refused
    /// — T-588 hit exactly that, because the **disk was full** — the coverage map then has no
    /// evidence at all for the live edge, and 18 s of measured rows read `unobserved`.
    ///
    /// Counted, not timed: the open slot holds exactly one dwell, its extent is the whole of the
    /// steady tune so far, its window is the tuning (notch and all, so T-595's `excluded` mark
    /// survives at the live edge), and the log has sealed **zero** records for it. When the tune
    /// does change, the sealed record takes over and the slot is empty again — never both.
    ///
    /// RED without the fix: drop `publish_open` from `InteractiveObserver::tick` and the open
    /// slot is empty while the log holds nothing, which is the 18 s of grey.
    #[test]
    fn the_tune_in_flight_is_published_as_an_open_dwell_before_any_record_seals() {
        let dir = std::env::temp_dir().join(format!("hk-t596-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let log = ObservationLog::open_with(
            ObservationLogConfig::new(dir.join("observations")),
            Some(TEST_DEVICE.into()),
            None,
        )
        .unwrap();
        let store = log.store();
        let counters = Arc::new(Counters::default());
        let mut obs = log.interactive(1024, None, Arc::clone(&counters));

        let t0: i64 = 1_789_297_800_000_000_000;
        let (centre, rate) = (446_000_000.0f64, 2e6f64);
        let tune = |c: f64, r: f64, now: i64| {
            counters
                .tune_center_bits
                .store(c.to_bits(), Ordering::SeqCst);
            counters.tune_rate_bits.store(r.to_bits(), Ordering::SeqCst);
            counters.stream_time_ns.store(now, Ordering::SeqCst);
            counters.tune_seq.fetch_add(1, Ordering::SeqCst);
        };

        tune(centre, rate, t0);
        obs.tick();
        // 18 s later — the span T-588 measured as grey — still far short of the 60 s seal.
        counters
            .stream_time_ns
            .store(t0 + 18_000_000_000, Ordering::SeqCst);
        obs.tick();

        let open = store.open_dwells();
        assert_eq!(open.len(), 1, "one front end, one open dwell: {open:?}");
        let ObservationRecord::Dwell(d) = &open[0] else {
            panic!("the open record is a dwell: {open:?}")
        };
        assert_eq!(d.device_id.as_deref(), Some(TEST_DEVICE));
        assert_eq!(d.observed.start.as_unix_nanos(), t0);
        assert_eq!(
            d.observed.end.as_unix_nanos(),
            t0 + 18_000_000_000,
            "the open dwell runs to the newest sample, which is what the live edge needs"
        );
        assert_eq!(d.window.center_hz, centre);
        assert_eq!(d.window.sample_rate_hz, rate);
        assert!(
            d.window.dc_excluded.is_some(),
            "the open dwell declares the same DC notch as the sealed one, so T-595's \
             \"excluded\" mark does not flip when the seal catches up: {:?}",
            d.window
        );
        // And nothing has sealed: this is the whole point.
        store.flush();
        let records = store
            .query(&RecordQuery {
                freq: FreqRange::new(centre - rate, centre + rate),
                span: TimeRange::new(
                    Timestamp::from_unix_nanos(t0 - 1),
                    Timestamp::from_unix_nanos(t0 + 60_000_000_000),
                ),
                tier: None,
                cursor: 0,
                limit: 64,
            })
            .records;
        assert!(
            records.is_empty(),
            "no record has sealed yet - the open dwell is the only evidence: {records:?}"
        );

        // The retune closes it: the sealed record speaks, and the slot never doubles up.
        tune(centre + 4e6, rate, t0 + 20_000_000_000);
        obs.tick();
        let open = store.open_dwells();
        assert_eq!(open.len(), 1, "one open dwell, the new tune's: {open:?}");
        let ObservationRecord::Dwell(d2) = &open[0] else {
            panic!("{open:?}")
        };
        assert_eq!(
            d2.window.center_hz,
            centre + 4e6,
            "the slot holds the tune the radio is on NOW, never the closed one"
        );
        drop(obs);
        drop(log);
        assert!(
            store.open_dwells().is_empty(),
            "the segment ended: the last tune closed into a record and the slot is empty"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
