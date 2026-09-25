//! **Where the radio has been**, as a traced path (T-898, docs/23 §10.6 rule 2): `GET
//! /api/tune-history`.
//!
//! The device's own movement through frequency has (time × frequency) coordinates, so by the
//! user's second map-UI principle it belongs **on** the map — a directions line, one per front end,
//! each retune a vertex and each dwell a vertical run. This route serves that line; the client lays
//! its vertices out through the pane's own capture-time mapping on every render frame and derives
//! nothing (the thin-client rule, and the reason there is a route at all rather than a client-side
//! reconstruction of the tune history).
//!
//! # Endpoint
//!
//! | Method | Path | Query | Answers |
//! |---|---|---|---|
//! | GET | `/api/tune-history` | `f_lo`, `f_hi` (Hz), `t0`, `t1` (Unix s) — all four; `device`? (a device id, `unknown`); `limit`? (default 64, max 512) | `{window, context, paths, devices, total, limit, truncated, horizon, method}` |
//!
//! # It reads the records that already exist
//!
//! The evidence is exactly the coverage map's: [`crate::coverage::Evidence`] over the IQ-ring
//! journal, the sealed observation-log dwells and sweep visits, and the dwell in flight — the
//! "coverage map derived from the SDR configuration/tune history" this project already keeps
//! (CLAUDE.md). [`hk_store::tunepath::tune_paths`] turns those spans into one timeline per front
//! end. Nothing new is recorded, and a gap no record covers **breaks** the line rather than being
//! drawn across: not knowing where the radio was is the same honesty as the coverage grey.
//!
//! # Read over every band, served for this viewport
//!
//! The spans are read over the **whole** frequency axis, because a retune's line enters the pane
//! from wherever the radio was: a route whose centres are all off screen still crosses the viewport
//! as a jump, and clipping the evidence to the band would cut the line at the pane's edge and imply
//! the radio stopped there. The time window is widened by its own duration (clamped to
//! [`MIN_CONTEXT_S`]..=[`MAX_CONTEXT_S`]) for the same reason at the other axis, and a path is
//! served when its own extent overlaps the viewport in both axes.

use hk_model::{FreqRange, Region, TimeRange, Timestamp};
use hk_store::coverage::Device;
use hk_store::tunepath::{TUNE_PATH_METHOD, TunePath, TunePathConfig, tune_paths};
use serde_json::{Value, json};

use crate::control::{CtlRequest, CtlResponse, Fail, dispatch, refuse_route};
use crate::coverage::Evidence;
use crate::http::ApiState;
use crate::query::ts_s;

/// Default number of routes served.
pub const DEFAULT_TUNE_LIMIT: usize = 64;
/// Largest `limit`.
pub const MAX_TUNE_LIMIT: usize = 512;
/// Longest time margin read either side of the window, s.
pub const MAX_CONTEXT_S: f64 = 600.0;
/// Shortest time margin read either side of the window, s: a leg that began before the window must
/// be read whole, or the line would start at the pane's edge instead of where the radio arrived.
pub const MIN_CONTEXT_S: f64 = 60.0;

/// The whole frequency axis: the route is read over every band (see the module header).
const ALL_FREQ: FreqRange = FreqRange::new(f64::NEG_INFINITY, f64::INFINITY);

/// One validated request.
#[derive(Clone, Debug, PartialEq)]
pub struct TuneHistoryQuery {
    /// The viewport.
    pub window: Region,
    /// Only this front end, if given.
    pub device: Option<Device>,
    /// Most routes served.
    pub limit: usize,
}

fn get<'a>(q: &'a [(String, String)], k: &str) -> Option<&'a str> {
    q.iter()
        .find(|(a, _)| a == k)
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty())
}

fn num(q: &[(String, String)], k: &str) -> Result<f64, Fail> {
    let v =
        get(q, k).ok_or_else(|| Fail::invalid(format!("{k} is required (f_lo, f_hi, t0, t1)")))?;
    v.parse::<f64>()
        .ok()
        .filter(|x| x.is_finite())
        .ok_or_else(|| Fail::invalid(format!("{k} must be a finite number")))
}

fn ts(s: f64) -> Timestamp {
    Timestamp::from_unix_nanos((s * 1e9).round() as i64)
}

/// Parses the query: the whole window, always (a viewport route answers about a viewport).
pub(crate) fn parse(q: &[(String, String)]) -> Result<TuneHistoryQuery, Fail> {
    let (f_lo, f_hi, t0, t1) = (
        num(q, "f_lo")?,
        num(q, "f_hi")?,
        num(q, "t0")?,
        num(q, "t1")?,
    );
    if !(f_hi > f_lo && f_lo >= 0.0) {
        return Err(Fail::invalid("need 0 <= f_lo < f_hi (Hz)"));
    }
    if !(t1 > t0 && t0 > -4e9 && t1 < 9e9) {
        return Err(Fail::invalid("need t0 < t1 (Unix seconds)"));
    }
    // `any` is the label of a union, never of one radio (`/api/coverage`'s rule), so it is not a
    // selector here: asking for every front end is asking for no filter at all.
    let device = match get(q, "device") {
        None => None,
        Some("any") => {
            return Err(Fail::invalid(
                "device names one front end (or `unknown`); omit it for every front end",
            ));
        }
        Some("unknown") => Some(Device::Unknown),
        Some(d) => Some(Device::Id(d.to_owned())),
    };
    let limit = match get(q, "limit") {
        None => DEFAULT_TUNE_LIMIT,
        Some(v) => v
            .parse::<usize>()
            .ok()
            .filter(|&n| (1..=MAX_TUNE_LIMIT).contains(&n))
            .ok_or_else(|| Fail::invalid(format!("limit is 1..={MAX_TUNE_LIMIT}")))?,
    };
    Ok(TuneHistoryQuery {
        window: Region::new(FreqRange::new(f_lo, f_hi), TimeRange::new(ts(t0), ts(t1))),
        device,
        limit,
    })
}

/// The time window read for a viewport: widened by its own duration, clamped.
pub fn context_of(window: &Region) -> TimeRange {
    let (t0, t1) = (ts_s(window.time.start), ts_s(window.time.end));
    let margin = (t1 - t0).clamp(MIN_CONTEXT_S, MAX_CONTEXT_S);
    TimeRange::new(ts(t0 - margin), ts(t1 + margin))
}

fn path_json(p: &TunePath) -> Value {
    let (f_lo, f_hi) = p.freq_extent().unwrap_or((0.0, 0.0));
    json!({
        "id": p.id(),
        // The route is labelled by the front end that drove it (the several-SDRs rule). `unknown`
        // is a record that named no radio, never a named one's route.
        "device": p.device.as_str(),
        "device_named": p.device.is_named(),
        "t0_s": ts_s(p.time().start),
        "t1_s": ts_s(p.time().end),
        "f_lo_hz": f_lo,
        "f_hi_hz": f_hi,
        "legs": p.legs.len(),
        "retunes": p.retunes(),
        "vertices": p.vertices().iter().map(|v| json!({
            "t_s": ts_s(v.t),
            "f_hz": v.f_hz,
            "sample_rate_hz": v.sample_rate_hz,
            "at": v.at.as_str(),
        })).collect::<Vec<_>>(),
        "vertices_truncated": p.truncated,
    })
}

fn region_json(freq: &FreqRange, time: &TimeRange) -> Value {
    json!({
        "f_lo_hz": freq.lo_hz,
        "f_hi_hz": freq.hi_hz,
        "t0_s": ts_s(time.start),
        "t1_s": ts_s(time.end),
    })
}

fn secs(t: Option<Timestamp>) -> Option<f64> {
    t.map(|t| t.as_unix_nanos() as f64 * 1e-9)
}

/// **How far the evidence reaches**, both ways, so a missing line is never read as "the radio was
/// nowhere". The same two sentences `/api/coverage`'s `horizon` states about a grey cell, said
/// about a route (see [`crate::coverage::Evidence`] for why each end needs one).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TuneHorizon {
    /// Unix s: the earliest instant any surviving tune record reaches; `None` when this server
    /// holds no tune record at all.
    pub oldest_record_s: Option<f64>,
    /// Unix s: when this server's memory of recording begins.
    pub recording_began_s: Option<f64>,
    /// Why this server cannot bound what it forgot, or `None` when it can.
    pub forgotten: Option<&'static str>,
    /// Unix s: how far forward the evidence reaches. A route ends here because the record does.
    pub as_of_s: Option<f64>,
}

impl TuneHorizon {
    pub(crate) fn of(ev: &Evidence) -> Self {
        Self {
            oldest_record_s: secs(ev.oldest_record),
            recording_began_s: secs(ev.recording_began),
            forgotten: ev.forgotten,
            as_of_s: secs(ev.newest_record),
        }
    }
}

/// The answer for `q` over already-read `paths`. Separated from the read so a test can assert on
/// exactly what the route serves for a given tune history, without a server.
pub fn answer_over(
    all: Vec<TunePath>,
    q: &TuneHistoryQuery,
    context: TimeRange,
    horizon: &TuneHorizon,
) -> Value {
    let served: Vec<TunePath> = all
        .into_iter()
        .filter(|p| q.device.as_ref().is_none_or(|d| &p.device == d))
        .filter(|p| p.time().overlaps(&q.window.time))
        .filter(|p| {
            p.freq_extent().is_some_and(|(lo, hi)| {
                FreqRange::new(lo, hi.max(lo + 1.0)).overlaps(&q.window.freq)
            })
        })
        .collect();
    let total = served.len();
    let mut devices: Vec<&str> = served.iter().map(|p| p.device.as_str()).collect();
    devices.dedup();
    json!({
        "window": region_json(&q.window.freq, &q.window.time),
        // The frequency axis is not narrowed: a route is read wherever the radio went, so the
        // context's band bounds are explicitly `null` rather than a number a reader could clip to.
        "context": {
            "f_lo_hz": Value::Null,
            "f_hi_hz": Value::Null,
            "t0_s": ts_s(context.start),
            "t1_s": ts_s(context.end),
            "freq_rule": "read over every band: a retune's line enters the pane from wherever the \
                radio was, so clipping the evidence to the window would cut the line at the edge \
                and imply the radio stopped there.",
        },
        "paths": served.iter().take(q.limit).map(path_json).collect::<Vec<_>>(),
        "devices": devices,
        "total": total,
        "limit": q.limit,
        "truncated": total > q.limit,
        "horizon": {
            // Unix s, or null when nothing on this server holds a tune record at all. Before it
            // there is no route to draw - not because the radio was nowhere, but because no record
            // survives to say where. The same sentence `/api/coverage` states for a grey cell.
            "oldest_record_s": horizon.oldest_record_s,
            "recording_began_s": horizon.recording_began_s,
            "forgotten": horizon.forgotten,
            // Unix s: how far forward the evidence reaches. A route ends here because the record
            // does, never because the radio stopped.
            "as_of_s": horizon.as_of_s,
            "rule": "a route is drawn only where a tune record covers it. Before \
                `oldest_record_s` and after `as_of_s` no line is drawn, and the absence of one \
                means \"no record reaches here\" - never \"the radio was not tuned anywhere\". A \
                gap no record covers BREAKS a path into two rather than being drawn across.",
        },
        "method": TUNE_PATH_METHOD,
    })
}

/// The answer for `q` over this server's tune records. Public so a pipeline test can assert on
/// exactly what the route serves for a run of scripted retunes, without a server.
pub fn answer(state: &ApiState, q: &TuneHistoryQuery) -> Value {
    read_evidence(state, q).0
}

/// The answer, and whether this server holds any tune history at all. One read of the records.
fn read_evidence(state: &ApiState, q: &TuneHistoryQuery) -> (Value, bool) {
    let context = context_of(&q.window);
    let ev = Evidence::collect(state, ALL_FREQ, context);
    let all = tune_paths(&ev.spans, &TunePathConfig::default());
    (
        answer_over(all, q, context, &TuneHorizon::of(&ev)),
        ev.has_source(),
    )
}

fn read(state: &ApiState, q: &[(String, String)]) -> Result<Value, Fail> {
    let q = parse(q)?;
    let (answer, has_source) = read_evidence(state, &q);
    // With no tune history at all, "no route here" would be a claim nothing supports - the same
    // reason a coverage row with no record reads `"unknown"` rather than grey.
    if !has_source {
        return Err(Fail::new(
            503,
            "unavailable",
            "no tune history on this server: neither an IQ ring journal nor an observation log",
        ));
    }
    Ok(answer)
}

/// Routes `/api/tune-history`; `None` for any other path.
pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    if req.path != "/api/tune-history" {
        return None;
    }
    if req.method != "GET" {
        return Some(refuse_route(state, req, Some("GET")));
    }
    Some(dispatch(
        state,
        req,
        "tune-history",
        false,
        |s| read(s, req.query),
        |_, _| Err(Fail::new(500, "failed", "not a mutating action")),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::ROUTES;
    use hk_store::coverage::{Analysis, CoverageSpan};

    fn q(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect()
    }

    const OK: [(&str, &str); 4] = [
        ("f_lo", "1e8"),
        ("f_hi", "1.002e8"),
        ("t0", "100"),
        ("t1", "110"),
    ];

    fn span(device: &str, t0: f64, t1: f64, center_hz: f64) -> CoverageSpan {
        CoverageSpan {
            device: Device::Id(device.to_owned()),
            time: TimeRange::new(ts(t0), ts(t1)),
            freq: FreqRange::new(center_hz - 1e6, center_hz + 1e6),
            analysis: Analysis::Analysed,
            center_hz,
            sample_rate_hz: 2e6,
        }
    }

    #[test]
    fn the_route_is_in_the_table() {
        assert!(
            ROUTES
                .iter()
                .any(|(m, p)| *m == "GET" && *p == "/api/tune-history")
        );
    }

    #[test]
    fn the_window_is_required_whole_and_well_formed() {
        let p = parse(&q(&OK)).ok().expect("valid");
        assert_eq!(p.limit, DEFAULT_TUNE_LIMIT);
        assert_eq!(p.device, None);
        for (key, _) in OK {
            let mut v = OK.to_vec();
            v.retain(|(a, _)| *a != key);
            assert!(parse(&q(&v)).is_err(), "missing {key}");
        }
        let bad = |k: &str, val: &str| {
            let mut v = OK.to_vec();
            v.retain(|(a, _)| *a != k);
            v.push((k, val));
            parse(&q(&v)).is_err()
        };
        assert!(bad("f_hi", "1e8"), "an empty band");
        assert!(bad("t1", "100"), "an empty window");
        assert!(bad("t0", "nan"));
        assert!(bad("f_lo", "-5"));
        let mut v = OK.to_vec();
        v.push(("limit", "0"));
        assert!(parse(&q(&v)).is_err());
        v.pop();
        // `any` is the label of a union, never a selector for one radio.
        v.push(("device", "any"));
        assert!(parse(&q(&v)).is_err());
        v.pop();
        v.push(("device", "hackrf-1"));
        v.push(("limit", "5"));
        let p = parse(&q(&v)).ok().expect("valid");
        assert_eq!(
            (p.device, p.limit),
            (Some(Device::Id("hackrf-1".into())), 5)
        );
    }

    #[test]
    fn the_context_widens_the_window_in_time_and_never_narrows_the_frequency_axis() {
        let p = parse(&q(&OK)).ok().expect("valid");
        let c = context_of(&p.window);
        assert_eq!((ts_s(c.start), ts_s(c.end)), (40.0, 170.0));
        let long = parse(&q(&[
            ("f_lo", "0"),
            ("f_hi", "1e6"),
            ("t0", "0"),
            ("t1", "7200"),
        ]))
        .ok()
        .expect("valid");
        let c = context_of(&long.window);
        assert_eq!(
            (ts_s(c.start), ts_s(c.end)),
            (-MAX_CONTEXT_S, 7200.0 + MAX_CONTEXT_S)
        );
    }

    #[test]
    fn a_route_that_only_crosses_the_band_in_transit_is_still_served() {
        // The radio dwelt at 88 MHz, retuned to 900 MHz, and the viewport is the 433 MHz band it
        // never stopped in: the jump crosses the pane, so the line must be there to draw.
        let spans = [
            span("a", 100.0, 110.0, 88e6),
            span("a", 110.0, 120.0, 900e6),
        ];
        let all = tune_paths(&spans, &TunePathConfig::default());
        let win = parse(&q(&[
            ("f_lo", "433e6"),
            ("f_hi", "434e6"),
            ("t0", "100"),
            ("t1", "120"),
        ]))
        .ok()
        .expect("valid");
        let ev = TuneHorizon::default();
        let v = answer_over(all.clone(), &win, context_of(&win.window), &ev);
        assert_eq!(v["total"], json!(1), "{v}");
        assert_eq!(v["devices"], json!(["a"]), "{v}");
        assert_eq!(v["paths"][0]["retunes"], json!(1), "{v}");
        assert_eq!(v["paths"][0]["vertices"].as_array().unwrap().len(), 4);
        assert_eq!(v["method"], json!(TUNE_PATH_METHOD));
        // A band the route neither visited nor crossed gets nothing.
        let far = parse(&q(&[
            ("f_lo", "2e9"),
            ("f_hi", "2.1e9"),
            ("t0", "100"),
            ("t1", "120"),
        ]))
        .ok()
        .expect("valid");
        let v = answer_over(all.clone(), &far, context_of(&far.window), &ev);
        assert_eq!(v["total"], json!(0), "{v}");
        // Nor does a window before the radio was ever recorded there.
        let before = parse(&q(&[
            ("f_lo", "87e6"),
            ("f_hi", "89e6"),
            ("t0", "10"),
            ("t1", "20"),
        ]))
        .ok()
        .expect("valid");
        let v = answer_over(all, &before, context_of(&before.window), &ev);
        assert_eq!(v["total"], json!(0), "{v}");
    }

    #[test]
    fn device_narrows_to_one_front_end() {
        let spans = [
            span("a", 100.0, 110.0, 100e6),
            span("b", 100.0, 110.0, 100e6),
        ];
        let all = tune_paths(&spans, &TunePathConfig::default());
        let one = parse(&q(&[
            ("f_lo", "99e6"),
            ("f_hi", "101e6"),
            ("t0", "100"),
            ("t1", "110"),
            ("device", "b"),
        ]))
        .ok()
        .expect("valid");
        let v = answer_over(all, &one, context_of(&one.window), &TuneHorizon::default());
        assert_eq!(v["total"], json!(1), "{v}");
        assert_eq!(v["paths"][0]["device"], json!("b"), "{v}");
        assert_eq!(v["paths"][0]["device_named"], json!(true), "{v}");
    }
}
