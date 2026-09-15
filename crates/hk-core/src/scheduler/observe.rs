//! Observation records from applied steps (T-115, ADR-0012 §1): maps a [`super::ScheduleStep`]
//! plus what was actually applied and analysed into `hk_model::attention::observation` records,
//! using [`super::Purpose::reason`]. Pure mapping; the pipeline owns the sink.
//!
//! - **Where** ([`WindowRule`]): the observed extent of a tune is the analysed spectrum frame's
//!   extent, the same span the history ingest folds into level-0 tiles (bin 0's lower edge to the
//!   last bin's upper edge, `hk_dsp::Spectrum::f_lo_hz`/`f_hi_hz`, T-116's coverage mask),
//!   clipped to the sampled band, less a DC notch of `±dc_half_hz` (the detector's DC rule
//!   tolerance). Both the log and history derive coverage from this one geometry, so they
//!   describe the same cells.
//! - **When** ([`StepObservation`]): the caller reports the settled start (first analysed data at
//!   the step's tuning) and the actual end; the recorder clips both into the planned interval and
//!   flags a step whose end came before its planned end as preempted.
//! - **Sweep hops** aggregate into one [`SweepRecord`] per pass or 60 s, whichever ends first,
//!   referencing a [`SweepGeometry`] emitted once per geometry change. Every other purpose is one
//!   [`DwellRecord`].
//!
//! **Allocation:** [`ObservationRecorder::observe`] does not allocate for a dwell step or for a
//! hop that does not close a sweep record (visits go into a preallocated buffer); closing a
//! record allocates its replacement buffer once. All times are the device/sample clock the steps
//! carry (ADR-0012 §0).

use hk_model::attention::ATTENTION_SCHEMA_VERSION;
use hk_model::attention::baseline::SiteKey;
use hk_model::attention::observation::{
    DwellRecord, HopVisit, ObservationRecord, ObservedWindow, SweepGeometry, SweepRecord,
};
use hk_model::ids::SurveyId;
use hk_model::{FreqRange, TimeRange, Timestamp};

use super::{CompiledPlan, Purpose, ScheduleStep};

/// Hop visits a sweep record buffer holds before it needs to grow (60 s at 20 hops/s, plus
/// slack).
pub const SWEEP_VISITS_CAPACITY: usize = 1_536;

/// Frequency tolerance when comparing an applied tuning with a geometry hop, Hz.
const TUNE_TOLERANCE_HZ: f64 = 1.0;

/// The observed-extent rule (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowRule {
    /// FFT bins of the analysed spectrum frames (the pipeline's detection/history `fft_len`).
    pub fft_bins: usize,
    /// Half-width of the excluded DC notch around the tuned centre, Hz (0: none).
    pub dc_half_hz: f64,
    /// Equivalent noise bandwidth of the analysis window, in bins (Hann: 1.5).
    pub enbw_bins: f64,
}

impl WindowRule {
    /// The window a tune at `center_hz` / `rate_hz` analyses.
    pub fn window(&self, center_hz: f64, rate_hz: f64) -> ObservedWindow {
        let n = self.fft_bins.max(2);
        let df = rate_hz / n as f64;
        let center_bin = (n / 2) as f64;
        // Exactly `hk_dsp::Spectrum::{f_lo_hz, f_hi_hz}` of a DC-centred frame.
        let f_lo = center_hz + (0.0 - center_bin) * df - df / 2.0;
        let f_hi = center_hz + ((n - 1) as f64 - center_bin) * df + df / 2.0;
        let half = rate_hz / 2.0;
        let usable = FreqRange::new(f_lo.max(center_hz - half), f_hi.min(center_hz + half));
        let dc_excluded = (self.dc_half_hz > 0.0).then(|| {
            FreqRange::new(
                (center_hz - self.dc_half_hz).max(usable.lo_hz),
                (center_hz + self.dc_half_hz).min(usable.hi_hz),
            )
        });
        ObservedWindow {
            center_hz,
            sample_rate_hz: rate_hz,
            usable,
            dc_excluded: dc_excluded.filter(|d| d.hi_hz > d.lo_hz),
            rbw_hz: (df * self.enbw_bins.max(1.0)).min(rate_hz),
        }
    }
}

/// What one applied step actually observed, as the caller measured it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StepObservation {
    /// The step as emitted.
    pub step: ScheduleStep,
    /// Centre the analysed data carried, Hz (a replay that cannot retune keeps its own).
    pub center_hz: f64,
    /// Sample rate the analysed data carried, Hz.
    pub rate_hz: f64,
    /// First analysed instant at the step's tuning (after retune settle); `None` when the tuning
    /// never took effect before the step ended.
    pub settled: Option<Timestamp>,
    /// When the step actually ended (the next step started, or the run stopped).
    pub end: Timestamp,
    /// Samples dropped (source or ring) during the step.
    pub dropped_samples: u64,
    /// Front end judged overloaded during the step.
    pub overload: bool,
}

impl StepObservation {
    /// The observed interval, inside the planned one, and whether a preemption cut it.
    pub fn observed(&self) -> (TimeRange, bool) {
        let start = self.step.t_start;
        let planned_end = self.step.t_end();
        let end = self.end.clamp(start, planned_end);
        let settled = self.settled.map_or(end, |s| s.clamp(start, end));
        (TimeRange::new(settled, end), self.end < planned_end)
    }
}

struct OpenSweep {
    plan_version: u32,
    geometry: u64,
    start: Timestamp,
    end: Timestamp,
    last_hop: u32,
    preempted_hops: u32,
    dropped_samples: u64,
    overload_hops: u32,
}

/// Turns applied steps into observation records (see the module docs).
pub struct ObservationRecorder {
    rule: WindowRule,
    site: SiteKey,
    survey_id: Option<SurveyId>,
    /// The geometry hops are recorded against now.
    geometry: SweepGeometry,
    geometry_pending: bool,
    /// T-173: the other pass parity's geometry (DC-dithered hops), when it differs.
    alt: Option<SweepGeometry>,
    alt_pending: bool,
    /// `geometry` is the dithered passes' one (T-181).
    dithered: bool,
    open: Option<OpenSweep>,
    visits: Vec<HopVisit>,
}

impl ObservationRecorder {
    /// A recorder for `plan`'s hops. `fixed_tuning` is the `(centre, rate)` of a source that
    /// cannot retune (every hop then observes that window); `None` for a controllable source.
    pub fn new(
        plan: &CompiledPlan,
        rule: WindowRule,
        fixed_tuning: Option<(f64, f64)>,
        survey_id: Option<SurveyId>,
    ) -> Self {
        let mut r = Self {
            rule,
            site: SiteKey::Unassigned,
            survey_id,
            geometry: SweepGeometry {
                schema: ATTENTION_SCHEMA_VERSION,
                id: 0,
                plan_version: plan.plan_version,
                hops: Vec::new(),
            },
            geometry_pending: true,
            alt: None,
            alt_pending: false,
            dithered: false,
            open: None,
            visits: Vec::with_capacity(SWEEP_VISITS_CAPACITY),
        };
        r.set_plan(plan, fixed_tuning);
        r
    }

    /// The site records are keyed by (T-119 sets it; `Unassigned` until then).
    pub fn set_site(&mut self, site: SiteKey) {
        self.site = site;
    }

    /// The hop geometry recorded against now (the last hop's pass parity; even passes before any
    /// hop).
    pub fn geometry(&self) -> &SweepGeometry {
        &self.geometry
    }

    /// Rebuilds the geometries for a new or updated plan: one for undithered passes and, when the
    /// plan DC-dithers its hops (T-173), one for dithered passes. Emits nothing; the next hop
    /// closes any open sweep record and emits its geometry first (each geometry once, until it
    /// changes).
    pub fn set_plan(&mut self, plan: &CompiledPlan, fixed_tuning: Option<(f64, f64)>) {
        let build = |dithered: bool| {
            let hops = plan
                .hops
                .iter()
                .map(|h| {
                    let c = if dithered {
                        h.center_hz + h.dither_hz
                    } else {
                        h.center_hz
                    };
                    let (c, r) = fixed_tuning.unwrap_or((c, h.rate_hz));
                    self.rule.window(c, r)
                })
                .collect();
            let g = SweepGeometry {
                schema: ATTENTION_SCHEMA_VERSION,
                id: 0,
                plan_version: plan.plan_version,
                hops,
            };
            SweepGeometry {
                id: geometry_id(&g),
                ..g
            }
        };
        let (even, odd) = (build(false), build(true));
        // A geometry already known keeps its pending flag (it was, or is yet to be, emitted).
        let pending = |g: &SweepGeometry| {
            if g.id == self.geometry.id {
                self.geometry_pending
            } else if let Some(a) = self.alt.as_ref().filter(|a| a.id == g.id) {
                debug_assert_eq!(a.plan_version, g.plan_version);
                self.alt_pending
            } else {
                true
            }
        };
        let (even_pending, odd_pending) = (pending(&even), pending(&odd));
        if odd.id == even.id {
            self.alt = None;
            self.alt_pending = false;
        } else {
            self.alt = Some(odd);
            self.alt_pending = odd_pending;
        }
        self.geometry = even;
        self.geometry_pending = even_pending;
        self.dithered = false;
    }

    /// A step is starting (call before it runs). A non-sweep step that would carry the open sweep
    /// record past [`SweepRecord::MAX_SPAN_NS`] closes it now, so a long preemption (a user intent,
    /// a lease) never holds a sweep's coverage back until the next hop, hours later and filed in
    /// a later hour than its own.
    pub fn begin(&mut self, step: &ScheduleStep, out: &mut impl FnMut(ObservationRecord)) {
        if matches!(step.purpose, Purpose::Sweep { .. }) {
            return;
        }
        let long = self.open.as_ref().is_some_and(|s| {
            step.t_end().as_unix_nanos() - s.start.as_unix_nanos() >= SweepRecord::MAX_SPAN_NS
        });
        if long {
            self.flush(out);
        }
    }

    /// Records one applied step, passing finished records to `out` in emission order.
    pub fn observe(&mut self, o: &StepObservation, out: &mut impl FnMut(ObservationRecord)) {
        match o.step.purpose {
            Purpose::Sweep { hop } => self.hop(o, hop, out),
            _ => {
                // A caller that did not call `begin` still closes the sweep before this record.
                self.begin(&o.step, out);
                let (observed, preempted) = o.observed();
                let reason = o.step.purpose.reason();
                out(ObservationRecord::Dwell(DwellRecord {
                    schema: ATTENTION_SCHEMA_VERSION,
                    survey_id: self.survey_id,
                    seq: o.step.seq,
                    plan_version: o.step.plan_version,
                    site: self.site,
                    reason,
                    tier: reason.tier(),
                    window: self.rule.window(o.center_hz, o.rate_hz),
                    rf_path: o.step.rf_path,
                    planned: TimeRange::new(o.step.t_start, o.step.t_end()),
                    observed,
                    preempted,
                    dropped_samples: o.dropped_samples,
                    overload: o.overload,
                    provenance_ref: None,
                }));
            }
        }
    }

    /// Closes the open sweep record (end of run, plan change, checkpoint).
    pub fn flush(&mut self, out: &mut impl FnMut(ObservationRecord)) {
        let Some(s) = self.open.take() else { return };
        let visits = std::mem::replace(&mut self.visits, Vec::with_capacity(SWEEP_VISITS_CAPACITY));
        out(ObservationRecord::Sweep(SweepRecord {
            schema: ATTENTION_SCHEMA_VERSION,
            survey_id: self.survey_id,
            plan_version: s.plan_version,
            site: self.site,
            geometry: s.geometry,
            span: TimeRange::new(s.start, s.end),
            visits,
            preempted_hops: s.preempted_hops,
            dropped_samples: s.dropped_samples,
            overload_hops: s.overload_hops,
        }));
    }

    fn hop(&mut self, o: &StepObservation, hop: u32, out: &mut impl FnMut(ObservationRecord)) {
        let idx = hop as usize;
        let fits = |g: &SweepGeometry| {
            g.hops.get(idx).is_some_and(|w| {
                (w.center_hz - o.center_hz).abs() <= TUNE_TOLERANCE_HZ
                    && (w.sample_rate_hz - o.rate_hz).abs() <= TUNE_TOLERANCE_HZ
            })
        };
        // T-173/T-181: the step's scheduled pass picks the geometry, not its tuning, so a hop that
        // could not dither (same centre on both) never splits a pass's record.
        if o.step.dither_pass != self.dithered
            && let Some(alt) = self.alt.as_mut()
        {
            std::mem::swap(&mut self.geometry, alt);
            std::mem::swap(&mut self.geometry_pending, &mut self.alt_pending);
            self.dithered = o.step.dither_pass;
        }
        // A hop whose data came from elsewhere than the geometry says: the geometry changed.
        let matches = fits(&self.geometry);
        if !matches {
            let w = self.rule.window(o.center_hz, o.rate_hz);
            if idx >= self.geometry.hops.len() {
                self.geometry.hops.resize(idx + 1, w);
            }
            self.geometry.hops[idx] = w;
            self.geometry.id = geometry_id(&self.geometry);
            self.geometry_pending = true;
        }
        if self.geometry.plan_version != o.step.plan_version {
            self.geometry.plan_version = o.step.plan_version;
            self.geometry.id = geometry_id(&self.geometry);
            self.geometry_pending = true;
        }
        let (observed, preempted) = o.observed();
        let close = self.open.as_ref().is_some_and(|s| {
            hop <= s.last_hop
                || s.plan_version != o.step.plan_version
                || s.geometry != self.geometry.id
                || observed.end.as_unix_nanos() - s.start.as_unix_nanos() > SweepRecord::MAX_SPAN_NS
        });
        if close {
            self.flush(out);
        }
        if self.geometry_pending {
            self.geometry_pending = false;
            out(ObservationRecord::Geometry(self.geometry.clone()));
        }
        let s = self.open.get_or_insert(OpenSweep {
            plan_version: o.step.plan_version,
            geometry: self.geometry.id,
            start: o.step.t_start,
            end: o.step.t_start,
            last_hop: hop,
            preempted_hops: 0,
            dropped_samples: 0,
            overload_hops: 0,
        });
        let ms = |ns: i64| u32::try_from(ns.max(0) / 1_000_000).unwrap_or(u32::MAX);
        let s0 = s.start.as_unix_nanos();
        self.visits.push(HopVisit {
            hop,
            start_ms: ms(observed.start.as_unix_nanos() - s0),
            observed_ms: ms(observed.duration_ns()),
        });
        s.end = observed.end;
        s.last_hop = hop;
        s.preempted_hops += u32::from(preempted);
        s.dropped_samples += o.dropped_samples;
        s.overload_hops += u32::from(o.overload);
    }
}

/// FNV-1a over the canonical hop windows and plan version: stable across runs and builds.
pub fn geometry_id(g: &SweepGeometry) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |v: u64| {
        for b in v.to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    eat(u64::from(g.plan_version));
    for w in &g.hops {
        eat(w.center_hz.to_bits());
        eat(w.sample_rate_hz.to_bits());
        eat(w.usable.lo_hz.to_bits());
        eat(w.usable.hi_hz.to_bits());
        let dc = w.dc_excluded.unwrap_or(FreqRange::new(0.0, 0.0));
        eat(dc.lo_hz.to_bits());
        eat(dc.hi_hz.to_bits());
        eat(w.rbw_hz.to_bits());
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule() -> WindowRule {
        WindowRule {
            fft_bins: 1024,
            dc_half_hz: 15e3,
            enbw_bins: 1.5,
        }
    }

    #[test]
    fn window_matches_the_spectrum_frame_extent_less_the_dc_notch() {
        let w = rule().window(100e6, 2.4e6);
        let df = 2.4e6 / 1024.0;
        // bin 0 lower edge is −fs/2 − df/2: clipped to the sampled band.
        assert_eq!(w.usable.lo_hz, 100e6 - 1.2e6);
        assert_eq!(w.usable.hi_hz, 100e6 + (511.0 * df) + df / 2.0);
        assert_eq!(
            w.dc_excluded,
            Some(FreqRange::new(100e6 - 15e3, 100e6 + 15e3))
        );
        assert!((w.rbw_hz - 1.5 * df).abs() < 1e-9);
        w.validate().unwrap();
        let c = w.covered();
        assert_eq!(c.len(), 2);
    }
}
