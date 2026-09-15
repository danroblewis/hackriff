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
use hk_model::attention::observation::ObservationRecord;
use hk_model::{ContentClass, SurveyId, Timestamp};
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

/// The run's observation log (see the module docs).
pub struct ObservationLog {
    writer: ObservationWriter,
}

impl ObservationLog {
    /// Opens the log under `<data_dir>/observations` and starts its writer; offers the
    /// `observations` stream through `sink`.
    pub(crate) fn open(data_dir: &Path, sink: Option<&StreamSink>) -> anyhow::Result<Self> {
        Self::open_with(
            ObservationLogConfig::new(data_dir.join("observations")),
            sink,
        )
    }

    /// [`ObservationLog::open`] with explicit log settings.
    pub(crate) fn open_with(
        config: ObservationLogConfig,
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
        let rule = WindowRule {
            fft_bins,
            dc_half_hz: DC_NOTCH_HALF_HZ,
            enbw_bins: HANN_ENBW_BINS,
        };
        Observer {
            recorder: ObservationRecorder::new(plan, rule, fixed_tuning, survey_id),
            queue: self.writer.queue(),
            fixed: fixed_tuning.is_some(),
            counters,
            current: None,
            last_now_ns: 0,
        }
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
    }

    fn tune(&self) -> (f64, f64) {
        (
            f64::from_bits(self.counters.tune_center_bits.load(Ordering::Relaxed)),
            f64::from_bits(self.counters.tune_rate_bits.load(Ordering::Relaxed)),
        )
    }

    fn dropped(&self) -> u64 {
        self.counters.source.source_dropped.load(Ordering::Relaxed)
            + self
                .counters
                .detect_reader
                .lost_samples
                .load(Ordering::Relaxed)
    }

    fn close(&mut self, end_ns: i64) {
        let Some(c) = self.current.take() else { return };
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

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use hk_core::SourceCapabilities;
    use hk_core::scheduler::{Scheduler, SchedulerConfig, SyntheticClock};

    use super::*;

    #[test]
    fn observe_stalled_writer_drops_and_counts_without_stalling_the_control_thread() {
        let dir = std::env::temp_dir().join(format!("hk-t115-stall-{}", std::process::id()));
        let mut cfg = ObservationLogConfig::new(dir.join("observations"));
        cfg.queue_len = 4;
        let log = ObservationLog::open_with(cfg, None).unwrap();
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
}
