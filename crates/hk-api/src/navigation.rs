//! `GET /api/navigation` (T-341): the **achievable `(centre, span)` grid**, and which tier would
//! answer for a state the view is about to move to.
//!
//! # The rule this serves
//!
//! The user's invariant (CLAUDE.md, "Time, the waterfall, and the live view"):
//!
//! > Navigation is discretized to achievable capture states, and the UI never implies detail the
//! > front end can't deliver. Zoom/pan and region-select resolve only to **realizable**
//! > configurations and **snap to the nearest one**: in frequency, centre and span are bounded by
//! > the instantaneous bandwidth (sample rate) and the tuning step — wider than the live window is
//! > **survey-history overview**, not live IQ; in time, by the retained window bounds and the
//! > history pyramid's discrete resolution tiers.
//!
//! This is **absent-means-not-measured** (T-297) applied to the navigation surface. T-297 stopped a
//! field being written for a region that was never swept, so nothing writes a zero rate. The same
//! rule here: *an interpolated pixel that looks like a measurement is a lie with a picture
//! attached*. A view drawn 40 MHz wide from a 20 Msps front end is not a 40 MHz observation; it is
//! stitched survey coverage, and it must say so.
//!
//! # The split
//!
//! **The backend owns which states are realizable**; the client does the gesture, the snap
//! arithmetic against the grid it was handed, and the styling. So this module reports the grid as
//! data — never a rendered axis — and also answers the one question a client must not decide for
//! itself: *for this requested state, which tier answers, and is it live IQ or overview?*
//!
//! # The three axes, and where each comes from
//!
//! | Axis | Source | Missing before T-341 |
//! |---|---|---|
//! | centre bounds | [`SourceCapabilities::frequency_ranges`] | no |
//! | span | [`SourceCapabilities::sample_rates`] — a live window's span **is** its sample rate | no |
//! | centre granularity | [`SourceCapabilities::tuning_step`] | **yes** — added by T-341 |
//! | time cells | the spectrum-history pyramid's level ladder | no |
//!
//! A source that cannot state a tuning step reports `center_step: "unknown"` with
//! `center_step_hz: null`, and **nothing snaps**: an unknown grid has no nearest point, and
//! answering the request unchanged would claim the device can sit exactly there.
//!
//! # `source`: the detail claim, not the file it came from
//!
//! [`DetailSource`] extends T-334's `resolution.source` — the declared home for "which tier
//! answered" — from one value to three. They are ordered by how much detail they claim, and
//! **nothing may claim more than it can show**: with no live capabilities known, `live-iq` is
//! unreachable, because "we could not check" is not evidence of live IQ.

use hk_core::SourceCapabilities;
use hk_core::source::SampleRates;
use hk_store::Pyramid;
use serde_json::{Value, json};

use crate::http::ApiState;
use crate::query::{ApiError, Params};

/// Which tier backs the detail on screen (T-334's `resolution.source`, extended by T-341).
///
/// Ordered by the strength of the claim: `LiveIq` > `SpectrumHistory` > `SurveyOverview`. A
/// surface that cannot establish the stronger claim reports the weaker one — never the reverse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetailSource {
    /// Live IQ from the front end, at the resolution drawn: the request fits inside one capture
    /// window (span ≤ the widest instantaneous bandwidth) and the front end can be there.
    LiveIq,
    /// The tiered spectrum-history pyramid: measured, but at a tier's cell size rather than at
    /// live-IQ resolution. Reduced from real frames — never interpolated.
    SpectrumHistory,
    /// Wider than any single capture window: no live window ever covered this span whole, so the
    /// picture is stitched from separate dwells. **Survey overview, not live detail.**
    SurveyOverview,
}

impl DetailSource {
    /// The wire value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LiveIq => "live-iq",
            Self::SpectrumHistory => "spectrum-history",
            Self::SurveyOverview => "survey-overview",
        }
    }

    /// The claim in words, for a client that shows it rather than styling it.
    pub const fn statement(self) -> &'static str {
        match self {
            Self::LiveIq => "live IQ from the front end at the resolution shown",
            Self::SpectrumHistory => {
                "spectrum history: measured, reduced to this tier's cells, not live IQ"
            }
            Self::SurveyOverview => {
                "survey overview: wider than one capture window, stitched from separate dwells, \
                 not live IQ"
            }
        }
    }

    /// `true` when the detail on screen is backed by live IQ.
    pub const fn is_live(self) -> bool {
        matches!(self, Self::LiveIq)
    }
}

/// Whether a region of `span_hz` could have been seen inside **one** live window.
///
/// `max_live_span_hz` is the widest instantaneous bandwidth any front end here can produce
/// ([`SourceCapabilities::max_live_span_hz`]). `None` means no front end reported one, and then
/// the answer is [`DetailSource::SurveyOverview`]: not knowing the window is not evidence that the
/// span fits inside it.
///
/// The comparison is `span_hz <= max_live_span_hz`, with the boundary counted as **inside**: a
/// view exactly as wide as the sample rate is one window's worth. Beyond it, every extra hertz had
/// to come from a different dwell, so the error direction is the honest one — a span one hertz
/// over the window is called overview rather than nearly-live.
pub fn live_window_verdict(span_hz: f64, max_live_span_hz: Option<f64>) -> DetailSource {
    match max_live_span_hz {
        Some(max) if span_hz.is_finite() && span_hz > 0.0 && span_hz <= max => DetailSource::LiveIq,
        _ => DetailSource::SurveyOverview,
    }
}

/// The `spans_hz` block: the span axis of the grid, which **is** the sample-rate axis.
fn spans_json(rates: &SampleRates) -> Value {
    match rates {
        SampleRates::Continuous { min_hz, max_hz } => json!({ "min": min_hz, "max": max_hz }),
        SampleRates::Discrete(v) => json!({ "values": v }),
    }
}

/// The `frequency` block: the achievable `(centre, span)` grid of one front end.
fn frequency_json(
    caps: &SourceCapabilities,
    device_id: Option<&str>,
    current: Option<(f64, f64)>,
) -> Value {
    json!({
        "device_id": device_id,
        "driver": caps.driver,
        "controllable": caps.controllable,
        "ranges_hz": caps.frequency_ranges.iter().map(|r| [r.min_hz, r.max_hz]).collect::<Vec<_>>(),
        // T-341: three-valued, like the bias tee. `"unknown"` with a null step is "the source
        // cannot say", and a client must not read it as 1 Hz or as continuous — it snaps nothing.
        "center_step": caps.tuning_step.as_str(),
        "center_step_hz": caps.tuning_step.step_hz(),
        "spans_hz": spans_json(&caps.sample_rates),
        // The widest span that is still one capture window. Wider is survey overview by
        // definition, whatever the pyramid can draw there.
        "max_live_span_hz": caps.max_live_span_hz(),
        "current": current.map(|(c, s)| json!({ "center_hz": c, "span_hz": s })),
    })
}

/// One **currently-active capture window**: a front end that is tuned and producing IQ right now.
///
/// T-340: the frequency navigator lights a segment per active window, so this is the fact it
/// draws from. `f_lo_hz`/`f_hi_hz` are the window's edges, derived here rather than in the client,
/// because "the span of a live window *is* its sample rate" is a statement about the front end.
fn window_json(l: &dyn crate::live_control::LiveControl) -> Value {
    let t = l.tuning();
    let span = t.sample_rate_hz;
    json!({
        // T-343's identity, so a lit segment can name the radio it belongs to. `null` is not an
        // identity: a source that reports none leaves the field null rather than taking a
        // placeholder that a second front end could collide with.
        "device_id": l.device_id(),
        "driver": l.capabilities().driver,
        "center_hz": t.center_hz,
        "span_hz": span,
        "f_lo_hz": t.center_hz - span / 2.0,
        "f_hi_hz": t.center_hz + span / 2.0,
    })
}

/// Every currently-active capture window, as a **list**.
///
/// # Why a list when this server runs one front end
///
/// The number of front ends is a fact about the run, not a constant of the design. The source
/// layer is already N-shaped (T-259's audit; T-302/T-303/T-304/T-305 keyed artifacts, baselines,
/// history and the source-layer rule on the front end that produced each frame), and the user's
/// multi-SDR direction is explicit: several simultaneous windows, or a wider one stitched from
/// contiguous dwells. A singleton here would force the navigator to assume one window and would
/// have to be re-shaped — and every consumer with it — the day a second chain exists.
///
/// So the length of this array is **measured, never assumed**: it is however many live front ends
/// this run holds. Today [`ApiState::live_control`] is one optional handle, so the array is empty
/// on a replay and holds one entry on a live run. **What would have to change to report N:**
/// `ApiState::live_control` becomes a collection of handles rather than an `Option`, built one per
/// `ReceiveChain` where the pipeline composes the run; nothing in this function or in its clients
/// changes, because both already speak in lists.
fn windows_json(state: &ApiState) -> Vec<Value> {
    state
        .live_control
        .iter()
        .map(|l| window_json(l.as_ref()))
        .collect()
}

/// The `time` block: the retained window and the pyramid's **discrete** resolution tiers.
///
/// The ladder is the whole point — time resolution is not a slider. A view asking for a finer cell
/// than `min_t_cell_s` cannot be served one, and T-334's rule applies: it is answered **coarser**,
/// never finer, because a coarse cell repeated across pixels shows a measured value while a fine
/// grid reduced in the client invents one.
fn time_json(p: &Pyramid) -> Value {
    let geom = p.geometry();
    let cfg = p.config();
    let tiers: Vec<Value> = geom
        .levels
        .iter()
        .enumerate()
        .map(|(i, l)| {
            json!({
                "level": i,
                "t_cell_s": l.t_cell_ns as f64 / 1e9,
                "f_cell_hz": l.f_cell_hz,
                // How far back this tier is kept, when the level sets an age. `null` = no age
                // limit of its own (the byte budget still bounds it) — not "kept forever".
                "max_age_s": cfg.levels.get(i).and_then(|c| c.max_age).map(|d| d.as_secs_f64()),
            })
        })
        .collect();
    let finest = geom.levels.first().map(|l| l.t_cell_ns as f64 / 1e9);
    let coarsest = geom.levels.last().map(|l| l.t_cell_ns as f64 / 1e9);
    json!({
        "tiers": tiers,
        "min_t_cell_s": finest,
        "max_t_cell_s": coarsest,
        // The newest capture time the history has reached, on the stream's own clock (a replay or
        // a time-compressed scene runs on its own, T-125). The *capture-ring* window that sizes
        // the scrubber is T-338's; this is the history horizon, and they are different things.
        "latest_s": p.latest_frame_end().map(|t| t.as_unix_nanos() as f64 / 1e9),
    })
}

/// The pyramid level that answers a requested time cell: the **finest tier no finer than
/// `want_t_cell_s`**, i.e. err coarser (T-334).
///
/// Returns `(level, t_cell_s)`. A request finer than the finest tier is answered by the finest
/// tier, which is coarser than asked — reported as a snap, never silently.
fn level_for_t_cell(p: &Pyramid, want_t_cell_s: f64) -> Option<(usize, f64)> {
    let levels = &p.geometry().levels;
    let cell = |l: &hk_store::history::LevelGeometry| l.t_cell_ns as f64 / 1e9;
    // Levels run finest first. Take the coarsest level still at or below the request; if none is
    // (the request is finer than every tier) the finest tier answers.
    let mut best: Option<(usize, f64)> = None;
    for (i, l) in levels.iter().enumerate() {
        let c = cell(l);
        if c <= want_t_cell_s {
            best = Some((i, c));
        }
    }
    best.or_else(|| levels.first().map(|l| (0, cell(l))))
}

/// The requested state parsed from the query, when one was asked for.
struct Requested {
    center_hz: f64,
    span_hz: f64,
    t_cell_s: Option<f64>,
}

fn finite(q: &Params, key: &str) -> Result<Option<f64>, ApiError> {
    match q.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()) {
        None => Ok(None),
        Some(raw) => raw
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .map(Some)
            .ok_or_else(|| ApiError::new(400, format!("{key} must be a finite number"))),
    }
}

fn requested(q: &Params) -> Result<Option<Requested>, ApiError> {
    let center = finite(q, "center_hz")?;
    let span = finite(q, "span_hz")?;
    let t_cell = finite(q, "t_cell_s")?;
    match (center, span) {
        (None, None) if t_cell.is_none() => Ok(None),
        (Some(center_hz), Some(span_hz)) if span_hz > 0.0 => Ok(Some(Requested {
            center_hz,
            span_hz,
            t_cell_s: t_cell,
        })),
        _ => Err(ApiError::new(
            400,
            "resolving a state needs center_hz and span_hz together, span_hz > 0",
        )),
    }
}

/// The `resolved` block: the nearest realizable state to what was asked for, and the detail claim
/// that comes with it.
///
/// `snapped` names each axis that moved, and `matched` is true when none did — T-334's vocabulary,
/// because it is the same question asked of a different grid: *did you get what you asked for, and
/// if not, on which axis?*
fn resolved_json(
    r: &Requested,
    caps: Option<&SourceCapabilities>,
    pyramid: Option<&Pyramid>,
    max_live_span_hz: Option<f64>,
) -> Value {
    let mut snapped: Vec<&str> = Vec::new();

    let center = caps.and_then(|c| c.snap_center_hz(r.center_hz));
    if center.is_some_and(|v| v != r.center_hz) {
        snapped.push("center_hz");
    }
    let span = caps.and_then(|c| c.snap_span_hz(r.span_hz));
    if span.is_some_and(|v| v != r.span_hz) {
        snapped.push("span_hz");
    }

    // The detail claim is made about the span the view will actually draw, not the one it asked
    // for. A request wider than the window snaps nowhere in frequency — the span axis clamps to
    // the widest rate, but the *view* still shows the wide region, from history.
    let live = live_window_verdict(r.span_hz, max_live_span_hz);

    let (t_cell, level) = match (r.t_cell_s, pyramid) {
        (Some(want), Some(p)) => match level_for_t_cell(p, want) {
            Some((level, cell)) => {
                if cell != want {
                    snapped.push("t_cell_s");
                }
                (Some(cell), Some(level))
            }
            None => (None, None),
        },
        _ => (None, None),
    };

    // Which tier answers. Live IQ only when the span fits one window *and* the request did not ask
    // for a history cell; asking for a tier is asking the pyramid, which is not live IQ.
    let source = match (live, r.t_cell_s) {
        (DetailSource::LiveIq, None) => DetailSource::LiveIq,
        (DetailSource::LiveIq, Some(_)) => DetailSource::SpectrumHistory,
        (other, _) => other,
    };

    json!({
        "requested": {
            "center_hz": r.center_hz,
            "span_hz": r.span_hz,
            "t_cell_s": r.t_cell_s,
        },
        // Null when the source cannot state a tuning step: an unknown grid has no nearest point,
        // and echoing the request back would claim the device can sit exactly there.
        "center_hz": center,
        "span_hz": span,
        "t_cell_s": t_cell,
        "level": level,
        "source": source.as_str(),
        "live": source.is_live(),
        "statement": source.statement(),
        "matched": snapped.is_empty(),
        "snapped": snapped,
    })
}

/// `GET /api/navigation[?center_hz&span_hz[&t_cell_s]]`.
pub fn navigation_json(state: &ApiState, q: &Params) -> Result<Value, ApiError> {
    let req = requested(q)?;

    let live = state.live_control.as_deref();
    let caps = live.map(|l| l.capabilities().clone());
    let current = live.map(|l| {
        let t = l.tuning();
        (t.center_hz, t.sample_rate_hz)
    });

    // The same spectrum history `/api/history` would read (the history store, else the floor
    // product's uncalibrated pyramid), so the tiers reported here are the tiers that answer there.
    // `None` when this server has neither: then the time axis is null rather than invented.
    let time_and_resolved = crate::http::with_history(state, |p| {
        Ok((
            time_json(p),
            req.as_ref().map(|r| {
                resolved_json(
                    r,
                    caps.as_ref(),
                    Some(p),
                    crate::http::max_live_span_hz(state),
                )
            }),
        ))
    })
    .ok();

    let mut body = json!({
        // Null with no live front end (a replay run): there is no achievable grid to report, and
        // an invented one would be worse than none.
        "frequency": caps.as_ref().map(|c| {
            frequency_json(c, live.and_then(|l| l.device_id()), current)
        }),
        // Null with no spectrum history on this server.
        "time": time_and_resolved.as_ref().map(|(t, _)| t.clone()),
        // T-340: every currently-active capture window, one entry per live front end. A list
        // because the count is a property of the run — see `windows_json`.
        "windows": windows_json(state),
    });
    let resolved = match &time_and_resolved {
        Some((_, r)) => r.clone(),
        // No history here, so no tier can be named — but the frequency half of the answer still
        // stands, and the live-vs-overview claim does not need the pyramid to be made.
        None => req
            .as_ref()
            .map(|r| resolved_json(r, caps.as_ref(), None, crate::http::max_live_span_hz(state))),
    };
    if let (Some(r), Some(obj)) = (resolved, body.as_object_mut()) {
        obj.insert("resolved".into(), r);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_span_inside_the_window_is_live_and_wider_is_overview() {
        // The honesty test and its control, on the one function that decides it.
        assert_eq!(live_window_verdict(2.4e6, Some(20e6)), DetailSource::LiveIq);
        assert_eq!(live_window_verdict(20e6, Some(20e6)), DetailSource::LiveIq);
        assert_eq!(
            live_window_verdict(20e6 + 1.0, Some(20e6)),
            DetailSource::SurveyOverview
        );
        assert_eq!(
            live_window_verdict(100e6, Some(20e6)),
            DetailSource::SurveyOverview
        );
        // Not knowing the window is not evidence that the span fits inside it.
        assert_eq!(live_window_verdict(1e3, None), DetailSource::SurveyOverview);
    }

    #[test]
    fn detail_sources_are_the_three_wire_values() {
        assert_eq!(DetailSource::LiveIq.as_str(), "live-iq");
        assert_eq!(DetailSource::SpectrumHistory.as_str(), "spectrum-history");
        assert_eq!(DetailSource::SurveyOverview.as_str(), "survey-overview");
        assert!(DetailSource::LiveIq.is_live());
        assert!(!DetailSource::SpectrumHistory.is_live());
        assert!(!DetailSource::SurveyOverview.is_live());
    }
}
