//! T-981: the front end's clip state, **measured per spectrum row**, and the front-end events it
//! adds up to — what `/api/status`'s `frontend` block and `GET /api/frontend/events` serve.
//!
//! Before this the only overload record was the sticky tune-state flag
//! ([`hk_model::Provenance::overload`]): set by the first block over the device's clip fraction and
//! held until the next gain change, so it could say "this gain state has clipped" but never *which
//! rows*. The explorer's one-row stripes (full-span energy while a pager keyed up with the amp on)
//! were therefore drawn as signal with nothing anywhere to say otherwise.
//!
//! - **Measured, per row.** The spectrum reader (the canvas's finest tier, T-484) counts the clipped
//!   samples and the ADC peak of each row's own span ([`hk_detect::ClipLedger`]) and judges it with
//!   [`hk_detect::FrontEndMonitor`]: `clipped` (provenance on every such row) and `event` (clipped
//!   *and* a whole-span energy step — the front end's energy, not a signal's). Both go on the row's
//!   frame metadata ([`hk_stream::RecordFlags::CLIPPED`] / [`hk_stream::RecordFlags::FRONTEND_EVENT`])
//!   and are counted here.
//! - **Events are time–frequency regions.** Consecutive event rows under the same tuning coalesce
//!   into one [`FrontEndEvent`] — `[t0, t1)` on the capture clock, over the tuned window — so the
//!   canvas can mark it at its own place on the time axis, distinct from any signal box.
//! - **Bounded.** The log keeps the newest [`EVENT_LOG_CAPACITY`] events in memory and counts what
//!   it evicts; it is not (yet) persisted, and `oldest_s` says how far back it reaches.
//! - **Detection.** The detection reader judges its own frames by the same rule and does not store a
//!   whole-span (or impulsive) record over an event; it counts it in `suppressed_detections`.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;

use hk_detect::{FrameVerdict, FrontEndConfig, SpanClip};
use hk_model::Timestamp;
use serde_json::{Value, json};

use crate::stats::{add, get, inc};

/// Events the in-memory log keeps (the newest; older ones are counted in `evicted_events`).
pub const EVENT_LOG_CAPACITY: usize = 1024;

/// One front-end event: consecutive event rows under one tuning.
#[derive(Clone, Debug, PartialEq)]
pub struct FrontEndEvent {
    /// Start of the first row, capture clock.
    pub t0: Timestamp,
    /// End of the last row, capture clock.
    pub t1: Timestamp,
    /// The front end.
    pub device_id: String,
    /// Tuned centre, Hz.
    pub center_hz: f64,
    /// Sample rate (the tuned window's width), Hz.
    pub sample_rate_hz: f64,
    /// Event rows.
    pub rows: u64,
    /// Clipped samples over those rows.
    pub clipped_samples: u64,
    /// Samples over those rows.
    pub samples: u64,
    /// Largest per-row clipped fraction.
    pub clip_fraction_max: f64,
    /// Largest per-row ADC peak, fraction of full scale.
    pub adc_peak_max: f64,
    /// Largest per-row median step over the reference, dB (`None` while no row had a reference).
    pub step_db_max: Option<f32>,
}

impl FrontEndEvent {
    /// The wire shape (`docs/api.md` "GET /api/frontend/events").
    pub fn to_json(&self) -> Value {
        json!({
            "kind": "clip",
            "t0": self.t0.secs(),
            "t1": self.t1.secs(),
            "t0_ns": self.t0.as_unix_nanos(),
            "t1_ns": self.t1.as_unix_nanos(),
            "device_id": self.device_id,
            "center_hz": self.center_hz,
            "sample_rate_hz": self.sample_rate_hz,
            "f_lo_hz": self.center_hz - self.sample_rate_hz / 2.0,
            "f_hi_hz": self.center_hz + self.sample_rate_hz / 2.0,
            "rows": self.rows,
            "clipped_samples": self.clipped_samples,
            "samples": self.samples,
            "clip_fraction_max": self.clip_fraction_max,
            "adc_peak_max": self.adc_peak_max,
            "step_db_max": self.step_db_max.map(f64::from),
        })
    }
}

/// One measured row, as the spectrum reader hands it over.
#[derive(Clone, Copy, Debug)]
pub struct RowMeasure<'a> {
    /// Row start, capture clock.
    pub t0: Timestamp,
    /// Row end, capture clock.
    pub t1: Timestamp,
    /// The front end.
    pub device_id: &'a str,
    /// Tuned centre, Hz.
    pub center_hz: f64,
    /// Sample rate, Hz.
    pub sample_rate_hz: f64,
    /// The row's clip count and ADC peak.
    pub span: SpanClip,
    /// The row's judgement.
    pub verdict: FrameVerdict,
}

#[derive(Debug, Default)]
struct State {
    log: VecDeque<FrontEndEvent>,
    last_row: Option<Value>,
    adc_peak_max: f64,
}

/// The run's front-end clip counters and event log (`Counters::frontend`).
#[derive(Debug, Default)]
pub struct FrontEndReport {
    /// Rows measured.
    pub rows: AtomicU64,
    /// Rows whose clipped fraction exceeded the threshold.
    pub clipped_rows: AtomicU64,
    /// Rows judged front-end events.
    pub event_rows: AtomicU64,
    /// Events (coalesced runs of event rows).
    pub events: AtomicU64,
    /// Clipped samples over every measured row.
    pub clipped_samples: AtomicU64,
    /// Samples over every measured row.
    pub samples: AtomicU64,
    /// Detections not stored because they were a front-end event's energy.
    pub suppressed_detections: AtomicU64,
    /// Events dropped from the front of the log by [`EVENT_LOG_CAPACITY`].
    pub evicted_events: AtomicU64,
    state: Mutex<State>,
}

impl FrontEndReport {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Counts one measured row, and logs it when it is an event.
    pub fn row(&self, m: &RowMeasure<'_>) {
        let v = &m.verdict;
        inc(&self.rows);
        add(&self.samples, v.clip.samples);
        if v.clipped {
            inc(&self.clipped_rows);
            add(&self.clipped_samples, v.clip.clipped);
        }
        let adc_peak = m.span.adc_peak();
        let mut st = self.state();
        st.adc_peak_max = st.adc_peak_max.max(adc_peak);
        st.last_row = Some(json!({
            "t": m.t0.secs(),
            "clipped_samples": v.clip.clipped,
            "samples": v.clip.samples,
            "clip_fraction": v.clip.fraction(),
            "adc_peak": adc_peak,
            "level_db": finite(v.level_db),
            "step_db": v.step_db.and_then(finite),
            "clipped": v.clipped,
            "event": v.event,
        }));
        if !v.event {
            return;
        }
        inc(&self.event_rows);
        // One row's length of slack joins an event across a row the judgement missed.
        let slack = m.t1.as_unix_nanos() - m.t0.as_unix_nanos();
        let joins = st.log.back().is_some_and(|e| {
            e.device_id == m.device_id
                && e.center_hz == m.center_hz
                && e.sample_rate_hz == m.sample_rate_hz
                && m.t0.as_unix_nanos() <= e.t1.as_unix_nanos() + slack
        });
        if joins {
            let e = st.log.back_mut().expect("checked");
            e.t1 = e.t1.max(m.t1);
            e.rows += 1;
            e.clipped_samples += v.clip.clipped;
            e.samples += v.clip.samples;
            e.clip_fraction_max = e.clip_fraction_max.max(v.clip.fraction());
            e.adc_peak_max = e.adc_peak_max.max(adc_peak);
            e.step_db_max = max_opt(e.step_db_max, v.step_db);
            return;
        }
        inc(&self.events);
        if st.log.len() >= EVENT_LOG_CAPACITY {
            st.log.pop_front();
            inc(&self.evicted_events);
        }
        st.log.push_back(FrontEndEvent {
            t0: m.t0,
            t1: m.t1,
            device_id: m.device_id.to_owned(),
            center_hz: m.center_hz,
            sample_rate_hz: m.sample_rate_hz,
            rows: 1,
            clipped_samples: v.clip.clipped,
            samples: v.clip.samples,
            clip_fraction_max: v.clip.fraction(),
            adc_peak_max: adc_peak,
            step_db_max: v.step_db,
        });
    }

    /// The logged events overlapping `[t0_s, t1_s)` (Unix s), oldest first.
    pub fn events_between(&self, t0_s: f64, t1_s: f64) -> Vec<FrontEndEvent> {
        self.state()
            .log
            .iter()
            .filter(|e| e.t0.secs() < t1_s && e.t1.secs() > t0_s)
            .cloned()
            .collect()
    }

    /// Start of the oldest logged event (Unix s), `None` with an empty log.
    pub fn oldest_s(&self) -> Option<f64> {
        self.state().log.front().map(|e| e.t0.secs())
    }

    /// The `/api/status` `frontend` block.
    pub fn to_json(&self) -> Value {
        let st = self.state();
        let cfg = FrontEndConfig::default();
        json!({
            "rows": get(&self.rows),
            "clipped_rows": get(&self.clipped_rows),
            "event_rows": get(&self.event_rows),
            "events": get(&self.events),
            "clipped_samples": get(&self.clipped_samples),
            "samples": get(&self.samples),
            "suppressed_detections": get(&self.suppressed_detections),
            "evicted_events": get(&self.evicted_events),
            "adc_peak_max": st.adc_peak_max,
            "last_row": st.last_row.clone(),
            "last_event": st.log.back().map(FrontEndEvent::to_json),
            "log": {
                "capacity": EVENT_LOG_CAPACITY,
                "retained": st.log.len(),
                "oldest_s": st.log.front().map(|e| e.t0.secs()),
            },
            "rule": {
                "clip_fraction": cfg.clip_fraction,
                "step_db": cfg.step_db,
                "saturation_fraction": cfg.saturation_fraction,
            },
        })
    }
}

/// Unix seconds of a capture-clock instant.
trait Secs {
    fn secs(self) -> f64;
}

impl Secs for Timestamp {
    fn secs(self) -> f64 {
        self.as_unix_nanos() as f64 / 1e9
    }
}

fn finite(x: f32) -> Option<f64> {
    x.is_finite().then_some(f64::from(x))
}

fn max_opt(a: Option<f32>, b: Option<f32>) -> Option<f32> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, None) => a,
        (None, b) => b,
    }
}

#[cfg(test)]
mod tests {
    use hk_detect::ClipCount;

    use super::*;

    fn measure(t_ms: i64, event: bool, clipped: u64) -> RowMeasure<'static> {
        let t0 = Timestamp::from_unix_nanos(1_000_000_000_000 + t_ms * 1_000_000);
        RowMeasure {
            t0,
            t1: t0.saturating_add_nanos(40_000_000),
            device_id: "mock:a",
            center_hz: 915e6,
            sample_rate_hz: 2e6,
            span: SpanClip {
                clip: ClipCount::new(clipped, 80_000),
                peak_code: if clipped > 0 { 128 } else { 40 },
            },
            verdict: FrameVerdict {
                clip: ClipCount::new(clipped, 80_000),
                clipped: clipped > 8,
                level_db: -90.0,
                step_db: Some(if event { 30.0 } else { 0.0 }),
                event,
            },
        }
    }

    #[test]
    fn consecutive_event_rows_are_one_event_and_a_later_one_is_another() {
        let r = FrontEndReport::default();
        r.row(&measure(0, false, 0));
        r.row(&measure(40, true, 30_000));
        r.row(&measure(80, true, 20_000));
        r.row(&measure(120, false, 0));
        r.row(&measure(400, true, 30_000));
        let j = r.to_json();
        assert_eq!(j["rows"], 5);
        assert_eq!(j["clipped_rows"], 3);
        assert_eq!(j["event_rows"], 3);
        assert_eq!(j["events"], 2);
        assert_eq!(j["clipped_samples"], 80_000);
        let all = r.events_between(0.0, f64::MAX);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].rows, 2);
        assert_eq!(
            all[0].t1.as_unix_nanos() - all[0].t0.as_unix_nanos(),
            80_000_000
        );
        let e = all[0].to_json();
        assert_eq!(e["f_lo_hz"], 914e6);
        assert_eq!(e["f_hi_hz"], 916e6);
        assert_eq!(e["adc_peak_max"], 1.0);
        // The window query is by overlap.
        assert_eq!(r.events_between(1000.3, 1000.5).len(), 1);
        assert_eq!(j["last_event"]["t0"], all[1].t0.secs());
    }

    #[test]
    fn the_log_is_bounded_and_counts_what_it_drops() {
        let r = FrontEndReport::default();
        for k in 0..(EVENT_LOG_CAPACITY as i64 + 5) {
            r.row(&measure(k * 1000, true, 30_000));
        }
        assert_eq!(r.events_between(0.0, f64::MAX).len(), EVENT_LOG_CAPACITY);
        assert_eq!(get(&r.evicted_events), 5);
        assert_eq!(get(&r.events), EVENT_LOG_CAPACITY as u64 + 5);
    }
}
