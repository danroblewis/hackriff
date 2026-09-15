//! Trust-test verdicts from the scheduler's verification groups (T-037b).
//!
//! The control thread's `Verification::evaluate` hands paired captures to [`TrustEval`], which
//! runs `hk_detect::trust::{gain_step, retune, rate_change}` and turns every classified emitter
//! (or a skipped comparison) into a verdict row. Rows wait in the [`VerdictOutbox`]
//! (`Counters::verdicts`, shared with every stage) until the detection writer thread stores them
//! as `hk_model::TrustVerdict`s with the run's Survey. Nothing here touches the repository, so
//! evaluation never waits on SQLite.
//!
//! Labels are kebab-case verdict names (`linear`, `suspect-imd`, `stays`, `moves-with-lo`,
//! `clock-harmonic`, …); a comparison that was not run is one row `skipped-<reason>` over the
//! capture's usable span. A retune on a replay (the centre could not move) is
//! `skipped-virtual-retune`.

use std::sync::{Mutex, PoisonError};

use hk_core::scheduler::{CaptureTrust, PoiKey, TrustEvaluator};
use hk_detect::trust::{
    CaptureResult, CaptureSide, GainStepConfig, GainStepSkip, GainStepVerdict, RateChangeConfig,
    RateChangeLabel, RateChangeSkip, RetuneConfig, RetuneLabel, gain_step, rate_change, retune,
};
use hk_model::{FreqRange, SurveyId, Timestamp, TrackId, TrustTest, TrustVerdict};
use serde_json::{Value, json};

use crate::stats::{SchedulerCounters, inc};

/// Pending rows kept when the writer falls behind; older rows beyond it are dropped.
const OUTBOX_CAP: usize = 65_536;

/// A verification capture.
pub(crate) struct Cap(pub CaptureResult);

impl CaptureTrust for Cap {
    fn clipped(&self) -> bool {
        self.0.clipped
    }
}

/// A verdict without its Survey (the writer adds it).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PendingVerdict {
    pub track: Option<TrackId>,
    pub test: TrustTest,
    pub label: String,
    pub freq: FreqRange,
    pub t: Timestamp,
    pub detail: Value,
}

impl PendingVerdict {
    pub fn into_verdict(self, survey_id: SurveyId) -> TrustVerdict {
        TrustVerdict {
            survey_id,
            track: self.track,
            test: self.test,
            label: self.label,
            freq: self.freq,
            t: self.t,
            detail: self.detail,
        }
    }
}

/// Verdicts from the control thread awaiting the detection writer.
#[derive(Debug, Default)]
pub struct VerdictOutbox(Mutex<Vec<PendingVerdict>>);

impl VerdictOutbox {
    pub(crate) fn push(&self, rows: impl IntoIterator<Item = PendingVerdict>) {
        let mut q = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        q.extend(rows);
        if q.len() > OUTBOX_CAP {
            let excess = q.len() - OUTBOX_CAP;
            q.drain(..excess);
        }
    }

    /// Takes every pending row.
    pub(crate) fn take(&self) -> Vec<PendingVerdict> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Puts rows back in front (a failed write).
    pub(crate) fn requeue(&self, mut rows: Vec<PendingVerdict>) {
        let mut q = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        rows.append(&mut q);
        *q = rows;
        if q.len() > OUTBOX_CAP {
            q.truncate(OUTBOX_CAP);
        }
    }
}

fn gain_label(v: GainStepVerdict) -> &'static str {
    match v {
        GainStepVerdict::Linear => "linear",
        GainStepVerdict::Compressed => "compressed",
        GainStepVerdict::SuspectImd => "suspect-imd",
        GainStepVerdict::InconclusiveBursty => "inconclusive-bursty",
        GainStepVerdict::InconclusiveWeak => "inconclusive-weak",
    }
}

fn gain_skip(s: GainStepSkip) -> &'static str {
    match s {
        GainStepSkip::LowerQuantisationLimited => "skipped-lower-quantisation-limited",
        GainStepSkip::Clipped => "skipped-clipped",
        GainStepSkip::GeometryMismatch => "skipped-geometry-mismatch",
    }
}

fn retune_label(l: RetuneLabel) -> &'static str {
    match l {
        RetuneLabel::Stays => "stays",
        RetuneLabel::MovesWithLo => "moves-with-lo",
        RetuneLabel::MovesAgainstLo => "moves-against-lo",
        RetuneLabel::ImageMoves => "image-moves",
        RetuneLabel::NotReproduced => "not-reproduced",
    }
}

fn rate_label(l: RateChangeLabel) -> &'static str {
    match l {
        RateChangeLabel::Stays => "stays",
        RateChangeLabel::ScalesWithRate => "scales-with-rate",
        RateChangeLabel::ClockHarmonic => "clock-harmonic",
        RateChangeLabel::NotReproduced => "not-reproduced",
    }
}

fn rate_skip(s: RateChangeSkip) -> &'static str {
    match s {
        RateChangeSkip::CentreMismatch => "skipped-centre-mismatch",
        RateChangeSkip::SameRate => "skipped-same-rate",
    }
}

fn side(s: CaptureSide) -> &'static str {
    match s {
        CaptureSide::A => "a",
        CaptureSide::B => "b",
    }
}

fn usable_span(c: &CaptureResult) -> FreqRange {
    FreqRange::centered(c.center_hz, 2.0 * c.spectrum.geometry.usable_half_hz)
}

fn emitter_span(c: &CaptureResult, i: usize) -> FreqRange {
    c.emitters
        .get(i)
        .map_or_else(|| usable_span(c), |e| FreqRange::new(e.f_lo_hz, e.f_hi_hz))
}

/// Runs the trust tests for one verification group and queues their verdicts.
pub(crate) struct TrustEval<'a> {
    counters: &'a SchedulerCounters,
    outbox: &'a VerdictOutbox,
    track: Option<TrackId>,
    t: Timestamp,
    /// T-127: the group's overall verdict so far (see [`TrustEval::verdict`]).
    verdict: std::cell::Cell<Option<bool>>,
}

impl<'a> TrustEval<'a> {
    /// An evaluator for the POI of `track` (if still known), evaluated at stream time `t`.
    pub fn new(
        counters: &'a SchedulerCounters,
        outbox: &'a VerdictOutbox,
        track: Option<TrackId>,
        t: Timestamp,
    ) -> Self {
        Self {
            counters,
            outbox,
            track,
            t,
            verdict: std::cell::Cell::new(None),
        }
    }

    /// T-127: the verification group's verdict for the bandit (`report_verification`): `false`
    /// when any test labelled the emitter a front-end artefact (IMD, compression, LO-following or
    /// image motion, clock harmonic), else `true` when a test confirmed it (linear, stays), else
    /// `None` (only inconclusive or skipped rows).
    pub fn verdict(&self) -> Option<bool> {
        self.verdict.get()
    }

    fn row(&self, test: TrustTest, label: &str, freq: FreqRange, detail: Value) -> PendingVerdict {
        match label {
            "suspect-imd" | "compressed" | "moves-with-lo" | "moves-against-lo" | "image-moves"
            | "clock-harmonic" | "scales-with-rate" => self.verdict.set(Some(false)),
            "linear" | "stays" if self.verdict.get().is_none() => self.verdict.set(Some(true)),
            _ => {}
        }
        PendingVerdict {
            track: self.track,
            test,
            label: label.to_owned(),
            freq,
            t: self.t,
            detail,
        }
    }
}

impl TrustEvaluator<Cap> for TrustEval<'_> {
    fn gain_step(&mut self, _poi: PoiKey, pair: u8, lower: &Cap, higher: &Cap) {
        let r = gain_step(&lower.0, &higher.0, &GainStepConfig::default());
        inc(&self.counters.gain_pairs_run);
        let common = json!({
            "pair": pair,
            "nominal_db": r.nominal_db,
            "g_lin_db": r.g_lin_db,
            "anchors": r.anchors,
            "delta_floor_db": r.delta_floor_db,
            "bound_db": r.bound_db,
        });
        let rows: Vec<PendingVerdict> = match r.skipped {
            Some(s) => vec![self.row(
                TrustTest::GainStep,
                gain_skip(s),
                usable_span(&higher.0),
                common,
            )],
            None => r
                .rows
                .iter()
                .map(|row| {
                    let mut detail = common.clone();
                    detail["snr_low_db"] = json!(row.snr_low_db);
                    detail["snr_high_db"] = json!(row.snr_high_db);
                    detail["delta_snr_db"] = json!(row.delta_snr_db);
                    detail["delta_snr_certain"] = json!(row.delta_snr_certain);
                    detail["delta_level_db"] = json!(row.delta_level_db);
                    detail["detected_low"] = json!(row.detected_low);
                    self.row(
                        TrustTest::GainStep,
                        gain_label(row.verdict),
                        emitter_span(&higher.0, row.emitter),
                        detail,
                    )
                })
                .collect(),
        };
        self.outbox.push(rows);
    }

    fn retune(&mut self, _poi: PoiKey, base: &Cap, moved: &Cap, delta_hz: f64) {
        if (moved.0.center_hz - base.0.center_hz).abs() < 1.0 {
            inc(&self.counters.retunes_skipped_virtual);
            let row = self.row(
                TrustTest::Retune,
                "skipped-virtual-retune",
                usable_span(&base.0),
                json!({ "requested_delta_hz": delta_hz }),
            );
            self.outbox.push([row]);
            return;
        }
        let r = retune(&base.0, &moved.0, &RetuneConfig::default());
        inc(&self.counters.retunes_run);
        let rows: Vec<PendingVerdict> = r
            .rows
            .iter()
            .map(|row| {
                let capture = match row.capture {
                    CaptureSide::A => &base.0,
                    CaptureSide::B => &moved.0,
                };
                self.row(
                    TrustTest::Retune,
                    retune_label(row.label),
                    emitter_span(capture, row.emitter),
                    json!({ "delta_hz": r.delta_hz, "capture": side(row.capture) }),
                )
            })
            .collect();
        self.outbox.push(rows);
    }

    fn rate_change(
        &mut self,
        _poi: PoiKey,
        base: &Cap,
        changed: &Cap,
        base_rate_hz: f64,
        rate_hz: f64,
    ) {
        let r = rate_change(
            &base.0,
            &changed.0,
            base_rate_hz,
            rate_hz,
            &RateChangeConfig::default(),
        );
        let common = json!({ "base_rate_hz": r.base_rate_hz, "rate_hz": r.rate_hz });
        let rows: Vec<PendingVerdict> = match r.skipped {
            Some(s) => {
                inc(&self.counters.rate_changes_skipped);
                vec![self.row(
                    TrustTest::RateChange,
                    rate_skip(s),
                    usable_span(&base.0),
                    common,
                )]
            }
            None => {
                inc(&self.counters.rate_changes_run);
                r.rows
                    .iter()
                    .map(|row| {
                        let capture = match row.capture {
                            CaptureSide::A => &base.0,
                            CaptureSide::B => &changed.0,
                        };
                        let mut detail = common.clone();
                        detail["capture"] = json!(side(row.capture));
                        self.row(
                            TrustTest::RateChange,
                            rate_label(row.label),
                            emitter_span(capture, row.emitter),
                            detail,
                        )
                    })
                    .collect()
            }
        };
        self.outbox.push(rows);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use hk_detect::trust::{CaptureEmitter, GainState};
    use hk_detect::{EdgeRule, Geometry, IntegratedSnapshot};

    use super::*;

    const BINS: usize = 4096;

    fn capture(center_hz: f64, rate_hz: f64, emitters: &[(f64, f64)]) -> Cap {
        let geometry = Geometry::new(center_hz, rate_hz, BINS, 0.0, &EdgeRule::default());
        Cap(CaptureResult {
            center_hz,
            gain: GainState {
                lna_db: 24.0,
                vga_db: 20.0,
                amp_on: false,
            },
            quantisation_limited: false,
            clipped: false,
            spectrum: IntegratedSnapshot {
                geometry,
                span_s: 0.5,
                mean_psd: vec![1e-12; BINS],
                mean_floor: vec![1e-12; BINS],
                block_psd: vec![vec![1e-12; BINS]; 4],
            },
            emitters: emitters
                .iter()
                .map(|&(f, w)| CaptureEmitter {
                    f_lo_hz: f - w / 2.0,
                    f_hi_hz: f + w / 2.0,
                    f_center_hz: f,
                    bandwidth_hz: w,
                    peak_excess_dbfs: -60.0,
                    spur: false,
                    dc: false,
                    image: false,
                    edge: false,
                })
                .collect(),
        })
    }

    #[test]
    fn rate_change_is_wired_and_every_verdict_is_queued() {
        let counters = SchedulerCounters::default();
        let outbox = VerdictOutbox::default();
        let track = TrackId::new();
        let t = Timestamp::from_unix_nanos(1_789_300_800_000_000_000);
        let mut eval = TrustEval::new(&counters, &outbox, Some(track), t);
        // 910 MHz = 91 × 10 Msps is gone at 8 Msps; 914.3 MHz is a real emitter at both rates.
        let base = capture(913e6, 10e6, &[(910.0e6, 5e3), (914.3e6, 100e3)]);
        let changed = capture(913e6, 8e6, &[(914.3e6, 100e3)]);
        eval.rate_change(1, &base, &changed, 10e6, 8e6);
        eval.rate_change(1, &base, &base, 10e6, 10e6);
        eval.retune(1, &base, &base, 1e6);
        assert_eq!(counters.rate_changes_run.load(Ordering::Relaxed), 1);
        assert_eq!(counters.rate_changes_skipped.load(Ordering::Relaxed), 1);
        assert_eq!(counters.retunes_skipped_virtual.load(Ordering::Relaxed), 1);
        let rows = outbox.take();
        assert!(outbox.take().is_empty());
        let label = |f: f64, test: TrustTest| {
            rows.iter()
                .find(|r| r.test == test && (0.5 * (r.freq.lo_hz + r.freq.hi_hz) - f).abs() < 1.0)
                .map(|r| r.label.as_str())
        };
        assert_eq!(
            label(910.0e6, TrustTest::RateChange),
            Some("clock-harmonic")
        );
        assert_eq!(label(914.3e6, TrustTest::RateChange), Some("stays"));
        assert!(rows.iter().any(|r| r.label == "skipped-same-rate"));
        assert!(rows.iter().any(|r| r.label == "skipped-virtual-retune"));
        assert!(rows.iter().all(|r| r.track == Some(track) && r.t == t));
        let v = rows[0].clone().into_verdict(SurveyId::new());
        assert_eq!(v.label, rows[0].label);
        outbox.push(rows.clone());
        outbox.requeue(vec![rows[1].clone()]);
        assert_eq!(outbox.take().len(), rows.len() + 1);
    }
}
