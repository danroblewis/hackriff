//! `GET /api/coverage` — the coverage map: which front end actually sampled which frequency, and
//! therefore which cells are grey because nothing ever looked (T-368).
//!
//! # The rule this serves
//!
//! The user's invariant (CLAUDE.md, "Time, the waterfall, and the live view"):
//!
//! > **The waterfall shows the data that exists for the selected (time, frequency); grey means
//! > genuinely unobserved.** … This requires the backend to keep a **coverage map derived from the
//! > SDR configuration/tune history** — for each interval, which centre/span/rate (and which
//! > device) was active — so observed-vs-unobserved is computed from what was actually sampled, and
//! > the frequency navigator's survey view is built from that same coverage.
//!
//! T-341 stopped the view *claiming* detail the front end never captured. This is the other half:
//! the view may *show* what the front end did capture, and must grey only what it did not. Between
//! them sits the failure this route exists to prevent — **painting never-observed spectrum as
//! quiet**, which invents an absence-of-signal finding out of an absence of measurement.
//!
//! # Three states on the wire, and the third cannot be spelled as the second
//!
//! Each cell is one of:
//!
//! ```jsonc
//! { "state": "unobserved" }                                  // 3: nothing ever looked. Grey.
//! { "state": "observed", "duty": 0.5, "observed_s": 30.0, … } // 1 or 2: the radio was here.
//! ```
//!
//! An unobserved cell carries **no measurement keys at all** — not `null` ones. That is stronger
//! than a nullable number, because there is no field a client can read as zero: the absence is
//! structural. It comes straight from [`hk_store::Coverage`], whose only observed-constructor refuses to
//! mint an observation out of nothing, so state 3 is unrepresentable as state 2 in the type as well
//! as on the wire.
//!
//! Whether an observed cell was *quiet* is a different question with a different answer: `shade`,
//! the level the spectrum history holds there. `shade: null` on an **observed** cell means the
//! history kept no level for it — sampled, level not retained — and a client must draw that
//! differently from grey. **Grey is `state == "unobserved"` and nothing else.**
//!
//! # Where the map comes from: provenance already written
//!
//! Nothing new is journalled for this. Two records already say "for each interval, which
//! centre/span/rate was active", and one of them also says which device:
//!
//! | Source | Interval | Centre/span/rate | Device | Horizon |
//! |---|---|---|---|---|
//! | IQ ring journal (`/api/iqbuffer` segments, ADR-0014) | yes | yes | **yes** (`device_id`) | the ring's retention |
//! | observation log (`DwellRecord`/`SweepRecord`, ADR-0012 §1) | yes | yes (`ObservedWindow`) | **yes** (`device_id`, T-378) | 30 days |
//!
//! The ring journal opens a new segment on **every** provenance change, so retunes are segment
//! boundaries by construction — it is already a tune history. **T-378** put the same `device_id` on
//! the observation log's records, so the long horizon is device-local too and coverage over the
//! retention window answers "did *this* front end look here", not merely "did anything" — which,
//! with two SDRs, is the whole question. A record that names no device (every record written before
//! T-378, or a source that states no identity) stays [`Device::Unknown`]: evidence that *something*
//! looked, never evidence that a *particular* front end did, and never read as whichever radio is
//! running now. `sources` in the response reports, per record kind, how many spans actually named
//! one.
//!
//! | Method | Path | Query | Answers |
//! |---|---|---|---|
//! | GET | `/api/coverage` | `f_lo`&`f_hi` (Hz, required), `cells`? (1…4096, default 256), `t0`&`t1`? (Unix s; default the capture window) | `{region, window, grid, devices, any, sources, resolution}` |

use hk_model::attention::observation::{ObservationRecord, ObservedWindow};
use hk_model::{FreqRange, TimeRange, Timestamp};
use hk_store::coverage::{Coverage, CoverageGrid, CoverageSpan, Device, MAX_COVERAGE_CELLS};
use hk_store::observation::{MAX_RECORD_LIMIT, ObservationStore, RecordQuery};
use serde_json::{Value, json};

use crate::http::ApiState;
use crate::query::{ApiError, Params, count, parse_freq_only};

/// Frequency cells the survey strip is drawn in when the caller names none.
pub const DEFAULT_CELLS: usize = 256;

/// Segments read from the IQ ring journal for one answer.
const RING_SEGMENTS: usize = 10_000;

fn f64_of(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64).filter(|x| x.is_finite())
}

/// The tune history the IQ ring journal already holds: one segment per provenance, so one per
/// retune, each naming the device that produced it (ADR-0014).
///
/// A segment whose tuned window misses the region is dropped here rather than folded: coverage of
/// somewhere else is not coverage of here.
fn ring_spans(state: &ApiState, freq: FreqRange, window: TimeRange) -> Vec<CoverageSpan> {
    let Some(c) = state.iq_buffer.as_deref() else {
        return Vec::new();
    };
    let status = c.status(&crate::iqbuffer::IqBufferQuery {
        t0: Some(window.start.as_unix_nanos() as f64 * 1e-9),
        t1: Some(window.end.as_unix_nanos() as f64 * 1e-9),
        limit: RING_SEGMENTS,
    });
    let Some(segments) = status.get("segments").and_then(Value::as_array) else {
        return Vec::new();
    };
    segments
        .iter()
        .filter_map(|s| {
            let center_hz = f64_of(s, "center_hz")?;
            let rate = f64_of(s, "sample_rate_hz").filter(|r| *r > 0.0)?;
            let (t0, t1) = (
                s.get("t0_ns").and_then(Value::as_i64)?,
                s.get("t1_ns").and_then(Value::as_i64)?,
            );
            let half = rate / 2.0;
            let f = FreqRange::new(center_hz - half, center_hz + half);
            if !f.overlaps(&freq) {
                return None;
            }
            Some(CoverageSpan {
                // The journal names the radio, so this span is device-local. A segment without one
                // is `Unknown` rather than borrowing whichever device happens to be running.
                device: s
                    .get("device_id")
                    .and_then(Value::as_str)
                    .filter(|d| !d.is_empty())
                    .map_or(Device::Unknown, |d| Device::Id(d.to_string())),
                time: TimeRange::new(
                    Timestamp::from_unix_nanos(t0),
                    Timestamp::from_unix_nanos(t1),
                ),
                freq: f,
                center_hz,
                sample_rate_hz: rate,
            })
        })
        .collect()
}

/// Which front end an observation record says looked (T-378).
///
/// A record that names one is device-local evidence, exactly like an IQ-ring segment. A record
/// that names none — every record written before T-378, and every record of a source that states
/// no identity — is [`Device::Unknown`]: evidence that *something* looked, and nothing more. It is
/// never read as the device that happens to be running now, which would invent provenance for data
/// that has none (`BiasTee::Unknown` ≠ `off`).
fn record_device(device_id: Option<&String>) -> Device {
    device_id
        .filter(|d| !d.is_empty())
        .map_or(Device::Unknown, |d| Device::Id(d.clone()))
}

/// The tune history the observation log holds: every dwell's and every sweep hop's analysed extent
/// over the interval it was analysed in (ADR-0012 §1).
///
/// Since T-378 these records name the front end that observed, so a span from the log is
/// device-local over the log's 30-day horizon — far beyond the IQ ring's retention — and the
/// coverage map can answer "did *this* radio look here" over the whole of it. A record without a
/// device stays [`Device::Unknown`] (see [`record_device`]). `ObservedWindow::covered()` already
/// removes the DC notch, so a notched window contributes two spans and the notch stays honestly
/// unobserved.
///
/// Returns the spans and how many of them named a device, so the answer's `sources` row can say
/// whether this record actually knew.
fn observation_spans(
    store: &ObservationStore,
    freq: FreqRange,
    window: TimeRange,
) -> (Vec<CoverageSpan>, usize) {
    let page = store.query(&RecordQuery {
        freq,
        span: window,
        tier: None,
        cursor: 0,
        limit: MAX_RECORD_LIMIT,
    });
    let mut out = Vec::new();
    let mut named = 0usize;
    let mut push = |device: &Device, w: &ObservedWindow, t: TimeRange| {
        for c in w.covered() {
            if c.overlaps(&freq) {
                named += usize::from(device.is_named());
                out.push(CoverageSpan {
                    device: device.clone(),
                    time: t,
                    freq: c,
                    center_hz: w.center_hz,
                    sample_rate_hz: w.sample_rate_hz,
                });
            }
        }
    };
    for r in &page.records {
        match r {
            ObservationRecord::Dwell(d) => {
                push(&record_device(d.device_id.as_ref()), &d.window, d.observed)
            }
            ObservationRecord::Sweep(s) => {
                let device = record_device(s.device_id.as_ref());
                let Some(g) = page.geometries.iter().find(|g| g.id == s.geometry) else {
                    continue;
                };
                for v in &s.visits {
                    let Some(w) = g.hops.get(v.hop as usize) else {
                        continue;
                    };
                    let start = s
                        .span
                        .start
                        .saturating_add_nanos(i64::from(v.start_ms) * 1_000_000);
                    let end = start.saturating_add_nanos(i64::from(v.observed_ms) * 1_000_000);
                    if end > start {
                        push(&device, w, TimeRange::new(start, end));
                    }
                }
            }
            ObservationRecord::Geometry(_) => {}
        }
    }
    (out, named)
}

/// One cell's JSON. An unobserved cell carries **no measurement keys**, so there is nothing a
/// client can read as a zero level or a zero occupancy.
fn cell_json(c: &Coverage, shade: Option<f32>) -> Value {
    match c.sampled() {
        None => json!({ "state": "unobserved" }),
        Some(s) => json!({
            "state": "observed",
            "spans": s.spans,
            "observed_s": s.observed_ns as f64 * 1e-9,
            "duty": s.duty,
            "last_s": s.last.as_unix_nanos() as f64 * 1e-9,
            "center_hz": s.center_hz,
            "sample_rate_hz": s.sample_rate_hz,
            // The level the spectrum history holds here, normalised to this answer's own observed
            // range so the client never picks a colour scale from the numbers it happens to hold.
            // `null` = sampled, level not retained — drawn differently from grey, never as grey and
            // never as the bottom of the ramp.
            "shade": shade.map(|v| json!(v)).unwrap_or(Value::Null),
        }),
    }
}

fn grid_json(g: &CoverageGrid, shades: Option<&[Option<f32>]>) -> Value {
    let cells: Vec<Value> = g
        .cells
        .iter()
        .enumerate()
        .map(|(i, c)| cell_json(c, shades.and_then(|s| s.get(i)).copied().flatten()))
        .collect();
    json!({
        "device": g.device.as_str(),
        // Whether `device` is a real front-end identity. `"unknown"` and `"any"` are labels, not
        // radios, and a client must not attribute their coverage to a device.
        "named": g.device.is_named(),
        "observed_cells": g.observed_cells(),
        "unobserved_cells": g.unobserved_cells(),
        "observed_fraction": g.observed_fraction(),
        "cells": cells,
    })
}

/// The level the spectrum history holds for each cell of the strip, normalised to the strip's own
/// observed range. `None` per cell where the history has nothing.
///
/// This is only the *shading*: it never decides observed-versus-unobserved. A cell the radio
/// demonstrably sampled but whose level the pyramid no longer keeps is observed with no shade, and
/// a cell the pyramid happens to hold a value for is still grey if no tune ever covered it.
fn shades(state: &ApiState, freq: FreqRange, window: TimeRange, cells: usize) -> Vec<Option<f32>> {
    let region = crate::query::Region {
        freq,
        t0_ns: window.start.as_unix_nanos(),
        t1_ns: window.end.as_unix_nanos(),
    };
    crate::http::with_history(state, |p| {
        Ok(crate::query::overview_read(p, &region, 1, cells)?.grid)
    })
    .map(|o| {
        let range = o.range_db;
        o.cells
            .iter()
            .map(|c| match (range, c.observed()) {
                (Some((lo, hi)), true) if c.max_db.is_finite() => {
                    let span = if hi > lo { hi - lo } else { 1.0 };
                    Some(((c.max_db - lo) / span).clamp(0.0, 1.0))
                }
                _ => None,
            })
            .collect()
    })
    .unwrap_or_else(|_| vec![None; cells])
}

/// `GET /api/coverage?f_lo&f_hi[&cells][&t0&t1]`.
pub fn coverage_json(state: &ApiState, q: &Params) -> Result<Value, ApiError> {
    const ALLOWED: [&str; 6] = ["f_lo", "f_hi", "cells", "t0", "t1", "token"];
    if let Some((k, _)) = q.iter().find(|(k, _)| !ALLOWED.contains(&k.as_str())) {
        return Err(ApiError::new(
            400,
            format!("unknown parameter {k:?} (allowed: f_lo, f_hi, cells, t0, t1)"),
        ));
    }
    let cells = count(q, "cells", DEFAULT_CELLS, MAX_COVERAGE_CELLS)?;
    let freq = parse_freq_only(q)?
        .ok_or_else(|| ApiError::new(400, "f_lo and f_hi are required, in Hz"))?;
    let (window, window_source) = window_of(state, q)?;

    let mut spans = ring_spans(state, freq, window);
    let ring = spans.len();
    let ring_named = spans.iter().filter(|s| s.device.is_named()).count();
    let mut log_named = 0;
    if let Some(store) = state.observations.as_ref() {
        let (log_spans, named) = observation_spans(store, freq, window);
        log_named = named;
        spans.extend(log_spans);
    }
    let log = spans.len() - ring;

    let shades = shades(state, freq, window, cells);
    let any = hk_store::coverage::union_grid(&spans, freq, window, cells);
    let devices: Vec<Value> = hk_store::coverage::by_device(&spans, freq, window, cells)
        .iter()
        .map(|g| grid_json(g, Some(&shades)))
        .collect();

    let source = crate::navigation::live_window_verdict(
        freq.width_hz(),
        crate::http::max_live_span_hz(state),
    );
    Ok(json!({
        "region": { "lo_hz": freq.lo_hz, "hi_hz": freq.hi_hz },
        "window": {
            "t0_s": window.start.as_unix_nanos() as f64 * 1e-9,
            "t1_s": window.end.as_unix_nanos() as f64 * 1e-9,
            "span_s": window.duration_ns() as f64 * 1e-9,
            // Where the window came from: the caller, or the capture window this server holds.
            "source": window_source,
        },
        "grid": { "cells": cells, "f_lo_hz": any.f_lo_hz, "f_cell_hz": any.f_cell_hz },
        // One entry per front end that actually sampled here, never merged. Empty means nothing on
        // this server can say what was sampled — which is **not** the same as "nothing was".
        "devices": devices,
        // The deliberate union, labelled `"any"` so it can never be mistaken for one radio's
        // coverage (T-259/T-305: device-local physics reads the device).
        "any": grid_json(&any, Some(&shades)),
        // Which tune histories answered, how many of each one's spans actually named the radio,
        // and whether every span it contributed did. `device_known` is **measured, not declared**
        // (T-378): a log still holding records written before devices were logged reports them as
        // the unattributed spans they are instead of claiming a device-local horizon it has not
        // got. A source with no spans still appears, so a client can tell "this record had nothing
        // here" from "this record was not consulted".
        "sources": [
            { "kind": "iq-ring", "spans": ring, "named_spans": ring_named,
              "device_known": ring_named == ring,
              "available": state.iq_buffer.is_some() },
            { "kind": "observation-log", "spans": log, "named_spans": log_named,
              "device_known": log_named == log,
              "available": state.observations.is_some() },
        ],
        "resolution": {
            "source": source.as_str(),
            "live": source.is_live(),
            "statement": source.statement(),
            "served_span_hz": freq.width_hz(),
            "max_live_span_hz": crate::http::max_live_span_hz(state),
            // Grey is decided here and nowhere else.
            "grey_rule": "grey a cell if and only if its state is \"unobserved\"",
        },
    }))
}

/// The window: the caller's `t0`/`t1` when both are given, else the capture window this server
/// holds. An answer with no window at all is refused rather than defaulted to a plausible span.
fn window_of(state: &ApiState, q: &Params) -> Result<(TimeRange, &'static str), ApiError> {
    let num = |k: &str| -> Result<Option<f64>, ApiError> {
        match q.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str()) {
            None => Ok(None),
            Some(raw) => raw
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
                .map(Some)
                .ok_or_else(|| ApiError::new(400, format!("{k} must be a finite number"))),
        }
    };
    match (num("t0")?, num("t1")?) {
        (Some(a), Some(b)) if b > a => Ok((
            TimeRange::new(
                Timestamp::from_unix_nanos((a * 1e9).round() as i64),
                Timestamp::from_unix_nanos((b * 1e9).round() as i64),
            ),
            "requested",
        )),
        (None, None) => {
            let (w, _) = crate::timeline::capture_window(state);
            let (t0, t1) = w.band().ok_or_else(|| {
                ApiError::new(
                    404,
                    "no capture window on this server; give t0 and t1 to ask for one",
                )
            })?;
            Ok((
                TimeRange::new(
                    Timestamp::from_unix_nanos((t0 * 1e9).round() as i64),
                    Timestamp::from_unix_nanos((t1 * 1e9).round() as i64),
                ),
                "capture-window",
            ))
        }
        _ => Err(ApiError::new(
            400,
            "t0 and t1 must be given together, t1 > t0",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: i64) -> Timestamp {
        Timestamp::from_unix_nanos(s * 1_000_000_000)
    }

    /// The front end this run is using, so the migration control has a device available to
    /// wrongly acquire.
    const RUNNING: &str = "hackrf:0000000000000000a06063c8234e925f";
    /// A second front end, covering somewhere else entirely.
    const OTHER: &str = "rtl-sdr:00000001";

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "hk-coverage-{tag}-{}-{:?}",
                std::process::id(),
                std::time::Instant::now()
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn band() -> FreqRange {
        // 100 MHz wide, so a 10-cell grid has 10 MHz cells.
        FreqRange::new(100e6, 200e6)
    }

    fn observation_window() -> TimeRange {
        TimeRange::new(t(1000), t(1060))
    }

    /// One dwell over `lo..hi` for the whole window, observed by `device` (or by nobody).
    fn dwell_over(device: Option<&str>, lo: f64, hi: f64) -> ObservationRecord {
        use hk_model::attention::baseline::SiteKey;
        use hk_model::attention::observation::{DwellRecord, Reason, Tier};
        let reason = Reason::RegionDwell { hop: 0 };
        ObservationRecord::Dwell(DwellRecord {
            schema: hk_model::attention::ATTENTION_SCHEMA_VERSION,
            survey_id: None,
            seq: 1,
            plan_version: 1,
            site: SiteKey::Unassigned,
            device_id: device.map(str::to_string),
            reason,
            tier: Tier::ScheduledPlan,
            window: ObservedWindow {
                center_hz: (lo + hi) / 2.0,
                sample_rate_hz: hi - lo,
                usable: FreqRange::new(lo, hi),
                dc_excluded: None,
                rbw_hz: 1e3,
            },
            rf_path: 0,
            planned: observation_window(),
            observed: observation_window(),
            preempted: false,
            dropped_samples: 0,
            overload: false,
            provenance_ref: None,
        })
    }

    fn store_of(dir: &TempDir, records: &[ObservationRecord]) -> ObservationStore {
        let s = ObservationStore::open(hk_store::observation::ObservationLogConfig::new(
            dir.0.join("observations"),
        ))
        .unwrap();
        for r in records {
            s.append(r);
        }
        s.flush();
        s
    }

    /// **The property.** Coverage over the observation log's horizon — the long one, far beyond the
    /// IQ ring's retention — answers *"did **this** front end look here"*, not merely "did
    /// anything". Two radios on disjoint ranges are two grids, each unobserved exactly where the
    /// other looked, and neither one's coverage is ever the union.
    ///
    /// (T-368's `two_devices_on_disjoint_ranges_do_not_union` shape, now driven through real
    /// observation-log records rather than hand-built spans — which is the whole of T-378: before
    /// it, both of these records produced `Device::Unknown` and this test could not be written.)
    #[test]
    fn long_horizon_coverage_names_the_front_end_that_looked_and_two_devices_do_not_union() {
        let dir = TempDir::new("two-devices");
        let store = store_of(
            &dir,
            &[
                dwell_over(Some(RUNNING), 100e6, 110e6),
                dwell_over(Some(OTHER), 190e6, 200e6),
            ],
        );
        let (spans, named) = observation_spans(&store, band(), observation_window());
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert_eq!(named, 2, "both records named their radio");

        let a = hk_store::coverage::grid(
            &spans,
            &Device::Id(RUNNING.into()),
            band(),
            observation_window(),
            10,
        );
        let b = hk_store::coverage::grid(
            &spans,
            &Device::Id(OTHER.into()),
            band(),
            observation_window(),
            10,
        );
        assert!(a.at(105e6).unwrap().is_observed(), "{a:?}");
        assert_eq!(*a.at(195e6).unwrap(), Coverage::Unobserved, "{a:?}");
        assert!(b.at(195e6).unwrap().is_observed(), "{b:?}");
        assert_eq!(*b.at(105e6).unwrap(), Coverage::Unobserved, "{b:?}");
        assert_eq!((a.observed_cells(), b.observed_cells()), (1, 1));

        // `by_device` keeps them apart and names both; the union is only what someone asked for.
        let per = hk_store::coverage::by_device(&spans, band(), observation_window(), 10);
        assert_eq!(per.len(), 2);
        assert!(per.iter().all(|g| g.device.is_named()));
        let u = hk_store::coverage::union_grid(&spans, band(), observation_window(), 10);
        assert_eq!(u.device, Device::Any);
        assert!(!u.device.is_named());
        assert_eq!(u.observed_cells(), 2);
    }

    /// **The migration control.** An observation log written *before* T-378 — the literal old
    /// bytes, hand-written below with no `device_id` key anywhere in them — still reads, and its
    /// coverage is `Device::Unknown`: evidence that *something* looked, never evidence that the
    /// radio running now did.
    ///
    /// **The mutation.** Default the missing field to the running device (`mutant` below) and the
    /// control breaks: the named grid then claims a band that front end demonstrably never tuned.
    /// That is what makes the honest assertion load-bearing rather than vacuous.
    #[test]
    fn a_pre_t378_log_reads_back_unknown_and_never_the_device_that_happens_to_be_running() {
        let dir = TempDir::new("pre-t378");
        let root = dir.0.join("observations");
        // The literal bytes of a pre-T-378 dwell over 140–160 MHz, CRC and all, written into the
        // log before this run opens it.
        let json = concat!(
            r#"{"record":"dwell","schema":1,"seq":11,"plan_version":1,"#,
            r#""site":{"kind":"unassigned"},"reason":{"code":"region-dwell","hop":0},"#,
            r#""tier":"scheduled-plan","window":{"center_hz":150000000.0,"#,
            r#""sample_rate_hz":20000000.0,"usable":{"lo_hz":140000000.0,"hi_hz":160000000.0},"#,
            r#""rbw_hz":1000.0},"rf_path":0,"#,
            r#""planned":{"start_ns":1000000000000,"end_ns":1060000000000},"#,
            r#""observed":{"start_ns":1000000000000,"end_ns":1060000000000},"#,
            r#""preempted":false,"dropped_samples":0,"overload":false}"#,
        );
        assert!(!json.contains("device_id"), "the old bytes name no device");
        let path = hk_store::observation::segment::segment_path(
            &root,
            hk_store::observation::segment::hour_of(t(1030)),
        );
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                "{:08x} {json}
",
                hk_store::observation::segment::crc32(json.as_bytes())
            ),
        )
        .unwrap();

        // The running device is in the same log, looking somewhere else — so there is a device
        // available for the old record to wrongly acquire.
        let store = store_of(&dir, &[dwell_over(Some(RUNNING), 100e6, 110e6)]);
        let (spans, named) = observation_spans(&store, band(), observation_window());
        assert_eq!(spans.len(), 2, "both records read: {spans:?}");
        assert_eq!(named, 1, "only one of them named a radio: {spans:?}");
        assert!(
            spans.iter().any(|s| s.device == Device::Unknown),
            "the pre-T-378 record must read back unknown: {spans:?}"
        );
        assert!(
            !spans
                .iter()
                .any(|s| s.device == Device::Id(RUNNING.into()) && s.freq.lo_hz == 140e6),
            "the old record must never acquire the running device: {spans:?}"
        );

        // The consequence: the running front end's own coverage says it never looked at 150 MHz.
        let honest = hk_store::coverage::grid(
            &spans,
            &Device::Id(RUNNING.into()),
            band(),
            observation_window(),
            10,
        );
        assert!(honest.at(105e6).unwrap().is_observed(), "{honest:?}");
        assert_eq!(
            *honest.at(150e6).unwrap(),
            Coverage::Unobserved,
            "an unattributed span is not this radio's coverage: {honest:?}"
        );
        // Unknown is its own device, and it did look there.
        let unknown =
            hk_store::coverage::grid(&spans, &Device::Unknown, band(), observation_window(), 10);
        assert!(unknown.at(150e6).unwrap().is_observed(), "{unknown:?}");
        assert!(!unknown.device.is_named());

        // The mutation: read a missing device as the one that happens to be running.
        let mutant: Vec<CoverageSpan> = spans
            .iter()
            .map(|s| CoverageSpan {
                device: Device::Id(RUNNING.into()),
                ..s.clone()
            })
            .collect();
        let m = hk_store::coverage::grid(
            &mutant,
            &Device::Id(RUNNING.into()),
            band(),
            observation_window(),
            10,
        );
        assert!(
            m.at(150e6).unwrap().is_observed(),
            "the mutant must claim the band this radio never tuned: {m:?}"
        );
        assert_ne!(
            honest.cells, m.cells,
            "so the honest assertion is doing work"
        );
    }

    #[test]
    fn an_unobserved_cell_carries_no_measurement_keys_at_all() {
        // The wire form of state 3: nothing a client can read as a zero level, a zero duty or a
        // zero occupancy. Not a null measurement — no measurement.
        let v = cell_json(&Coverage::Unobserved, Some(0.9));
        assert_eq!(v, json!({ "state": "unobserved" }));
        assert!(v.get("duty").is_none());
        assert!(v.get("observed_s").is_none());
        assert!(v.get("shade").is_none());
    }

    #[test]
    fn an_observed_cell_states_the_sampling_that_makes_it_observed() {
        let c =
            hk_store::coverage::Coverage::of(2, 30_000_000_000, 60_000_000_000, t(1030), 1e8, 2e6);
        let v = cell_json(&c, None);
        assert_eq!(v["state"], json!("observed"));
        assert_eq!(v["spans"], json!(2));
        // Floating seconds, so compare as a number rather than by JSON equality.
        assert!(
            (v["observed_s"].as_f64().unwrap() - 30.0).abs() < 1e-9,
            "{v}"
        );
        assert_eq!(v["duty"], json!(0.5));
        assert_eq!(v["last_s"], json!(1030.0));
        // Sampled, level not retained: an explicit null on an *observed* cell, which is a
        // different thing from grey and must be drawn differently.
        assert_eq!(v["shade"], Value::Null);
        assert_ne!(v["state"], cell_json(&Coverage::Unobserved, None)["state"]);
    }
}
