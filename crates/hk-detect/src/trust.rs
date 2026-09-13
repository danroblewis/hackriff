//! Cross-capture trust tests (S4 rules 6–7) as pure functions over two capture results, for the
//! scheduler (C04) to run when it interleaves gain states or retunes.
//!
//! - [`gain_step`] — SNR invariance. With an analog-noise-limited floor a real signal keeps its
//!   SNR across a gain step, while an `i`-th order product gains `(i − 1)` dB of SNR per dB. The
//!   allowed real-signal ΔSNR is `bound = max(0, G_lin − Δfloor)`, `G_lin` the median level change
//!   of ≥ 3 strong (≥ 10 dB both), wide (≥ 50 kHz), clean anchors (never nominal unless there are
//!   no anchors). Verdicts: `suspect_imd` if ΔSNR > bound + 6 dB; `compressed` if the level grew
//!   < `G_lin` − 6 dB; `inconclusive_bursty` if either block SNR spread > 6 dB;
//!   `inconclusive_weak` if the lower SNR is below the measurability limit (0.5 dB excess ≈
//!   −9.1 dB SNR; the lower bound is used). Skipped entirely when the lower state is
//!   quantisation-limited or either capture clipped (reduce gain first; S4's a = 3 control).
//! - [`retune`] — same gain, centres Δ apart: an emitter found at the same absolute frequency
//!   `stays`; at ±Δ with similar level (±4 dB) and bandwidth (×2) it is LO-relative; at 2Δ an
//!   image; otherwise `not_reproduced` (→ `marginal`). Reference harmonics and RF IMD stay at
//!   absolute frequency, so they need rule 1 / the gain step instead.

use hk_model::detection::SpurReason;
use hk_model::{DetectionFlags, Tune};

use crate::alpha::db;
use crate::detector::AMP_NOMINAL_DB;
use crate::integrated::{IntegratedEmitter, IntegratedSnapshot};

/// Front-end gain settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GainState {
    /// LNA, dB.
    pub lna_db: f64,
    /// VGA, dB.
    pub vga_db: f64,
    /// RF amp on.
    pub amp_on: bool,
}

impl GainState {
    /// From a provenance tune record.
    pub fn of(tune: &Tune) -> Self {
        Self {
            lna_db: tune.lna_db,
            vga_db: tune.vga_db,
            amp_on: tune.amp_on,
        }
    }

    /// LNA + VGA + `amp_db` when on.
    pub fn nominal_db(&self, amp_db: f64) -> f64 {
        self.lna_db + self.vga_db + if self.amp_on { amp_db } else { 0.0 }
    }
}

/// An emitter of one capture, with the flags that disqualify it as an anchor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CaptureEmitter {
    /// Lower edge, Hz.
    pub f_lo_hz: f64,
    /// Upper edge, Hz.
    pub f_hi_hz: f64,
    /// Centre, Hz.
    pub f_center_hz: f64,
    /// Bandwidth, Hz.
    pub bandwidth_hz: f64,
    /// Peak-bin excess, dBFS.
    pub peak_excess_dbfs: f64,
    /// Spur candidate (any reason).
    pub spur: bool,
    /// DC.
    pub dc: bool,
    /// Image candidate.
    pub image: bool,
    /// Edge zone.
    pub edge: bool,
}

impl From<&IntegratedEmitter> for CaptureEmitter {
    fn from(e: &IntegratedEmitter) -> Self {
        Self {
            f_lo_hz: e.f_lo_hz,
            f_hi_hz: e.f_hi_hz,
            f_center_hz: e.f_center_hz,
            bandwidth_hz: e.bandwidth_hz,
            peak_excess_dbfs: e.peak_excess_dbfs,
            spur: e.flags.spur_candidate && e.flags.spur_reason != Some(SpurReason::Dc),
            dc: e.flags.spur_reason == Some(SpurReason::Dc),
            image: e.flags.image_candidate,
            edge: e.flags.edge,
        }
    }
}

/// One capture: integrated spectra and emitters under one gain state and tune.
#[derive(Clone, Debug, PartialEq)]
pub struct CaptureResult {
    /// Tuned centre, Hz.
    pub center_hz: f64,
    /// Gains.
    pub gain: GainState,
    /// The floor was quantisation-limited.
    pub quantisation_limited: bool,
    /// The ADC clipped (or the provenance is overloaded).
    pub clipped: bool,
    /// Integrated spectra.
    pub spectrum: IntegratedSnapshot,
    /// Emitters.
    pub emitters: Vec<CaptureEmitter>,
}

/// Gain-step settings (S4 defaults).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GainStepConfig {
    /// Nominal amp gain when no anchors exist, dB (11).
    pub amp_nominal_db: f64,
    /// IMD / compression margin, dB (6).
    pub imd_margin_db: f64,
    /// Block SNR spread above which a row is bursty, dB (6).
    pub burst_spread_db: f64,
    /// Measurability limit, dB SNR (0.5 dB excess ≈ −9.14 dB).
    pub measurability_limit_db: f64,
    /// Anchor SNR in both captures, dB (10).
    pub anchor_snr_db: f64,
    /// Anchor bandwidth, Hz (50 kHz).
    pub anchor_min_bandwidth_hz: f64,
    /// Fallback anchor SNR in the lower capture, dB (3).
    pub fallback_anchor_snr_db: f64,
    /// Anchors needed for a measured `G_lin` (3).
    pub min_anchors: usize,
}

impl Default for GainStepConfig {
    fn default() -> Self {
        Self {
            amp_nominal_db: AMP_NOMINAL_DB,
            imd_margin_db: 6.0,
            burst_spread_db: 6.0,
            measurability_limit_db: db(10f64.powf(0.05) - 1.0),
            anchor_snr_db: 10.0,
            anchor_min_bandwidth_hz: 50e3,
            fallback_anchor_snr_db: 3.0,
            min_anchors: 3,
        }
    }
}

/// Gain-step verdict for one emitter of the higher-gain capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GainStepVerdict {
    /// SNR kept: consistent with a real signal.
    Linear,
    /// Level grew less than `G_lin − margin`.
    Compressed,
    /// ΔSNR above the real-signal bound + margin.
    SuspectImd,
    /// Block SNR spread too large to compare.
    InconclusiveBursty,
    /// Lower SNR below the measurability limit and ΔSNR within the bound.
    InconclusiveWeak,
}

impl GainStepVerdict {
    /// Applies the verdict to detection flags (`suspect_imd`, `compressed`, or `marginal`).
    pub fn apply(self, flags: &mut DetectionFlags) {
        match self {
            GainStepVerdict::Linear => {}
            GainStepVerdict::Compressed => flags.compressed = true,
            GainStepVerdict::SuspectImd => flags.suspect_imd = true,
            GainStepVerdict::InconclusiveBursty | GainStepVerdict::InconclusiveWeak => {
                flags.marginal = true;
            }
        }
    }
}

/// Why a gain-step comparison was not run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GainStepSkip {
    /// The lower-gain floor is quantisation-limited: the bound is too wide to infer anything.
    LowerQuantisationLimited,
    /// A capture clipped: compression corrupts `G_lin` and Δfloor. Reduce gain first.
    Clipped,
    /// Centres or bin geometry differ.
    GeometryMismatch,
}

/// One compared emitter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GainStepRow {
    /// Index into the higher-gain capture's emitters.
    pub emitter: usize,
    /// SNR at lower gain, dB.
    pub snr_low_db: f64,
    /// SNR at higher gain, dB.
    pub snr_high_db: f64,
    /// ΔSNR (a lower bound when not certain), dB.
    pub delta_snr_db: f64,
    /// ΔSNR measured (the lower SNR was above the measurability limit).
    pub delta_snr_certain: bool,
    /// Level change, dB.
    pub delta_level_db: f64,
    /// Detected in the lower capture too.
    pub detected_low: bool,
    /// Verdict.
    pub verdict: GainStepVerdict,
}

/// Gain-step result.
#[derive(Clone, Debug, PartialEq)]
pub struct GainStepResult {
    /// Nominal gain step, dB.
    pub nominal_db: f64,
    /// Measured linear gain step, dB (nominal without anchors).
    pub g_lin_db: f64,
    /// Anchors used.
    pub anchors: usize,
    /// Median floor change, dB.
    pub delta_floor_db: f64,
    /// Real-signal ΔSNR bound, dB.
    pub bound_db: f64,
    /// Set when the test was not run (rows empty).
    pub skipped: Option<GainStepSkip>,
    /// Rows.
    pub rows: Vec<GainStepRow>,
}

fn overlaps(a: &CaptureEmitter, e: &CaptureEmitter) -> bool {
    (a.f_lo_hz <= e.f_center_hz && e.f_center_hz <= a.f_hi_hz)
        || (e.f_lo_hz <= a.f_center_hz && a.f_center_hz <= e.f_hi_hz)
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        0.5 * (v[n / 2 - 1] + v[n / 2])
    }
}

/// Rule 6: compares `lower` (lower gain) with `higher` at the same centre.
pub fn gain_step(
    lower: &CaptureResult,
    higher: &CaptureResult,
    cfg: &GainStepConfig,
) -> GainStepResult {
    let nominal_db =
        higher.gain.nominal_db(cfg.amp_nominal_db) - lower.gain.nominal_db(cfg.amp_nominal_db);
    let mut result = GainStepResult {
        nominal_db,
        g_lin_db: nominal_db,
        anchors: 0,
        delta_floor_db: f64::NAN,
        bound_db: f64::NAN,
        skipped: None,
        rows: Vec::new(),
    };
    let (ga, gb) = (&lower.spectrum.geometry, &higher.spectrum.geometry);
    if ga.bins != gb.bins || (lower.center_hz - higher.center_hz).abs() > 0.5 * ga.bin_width_hz {
        result.skipped = Some(GainStepSkip::GeometryMismatch);
        return result;
    }
    if lower.clipped || higher.clipped {
        result.skipped = Some(GainStepSkip::Clipped);
        return result;
    }
    if lower.quantisation_limited {
        result.skipped = Some(GainStepSkip::LowerQuantisationLimited);
        return result;
    }
    let Some(delta_floor_db) = lower.spectrum.floor_change_db(&higher.spectrum) else {
        result.skipped = Some(GainStepSkip::GeometryMismatch);
        return result;
    };
    struct Meas {
        idx: usize,
        snr_a: f64,
        snr_b: f64,
        lvl_a: f64,
        lvl_b: f64,
        spread_a: f64,
        spread_b: f64,
        det_a: bool,
        clean: bool,
        bw: f64,
    }
    let mut rows = Vec::new();
    for (idx, e) in higher.emitters.iter().enumerate() {
        if e.edge {
            continue;
        }
        let (Some(a), Some(b)) = (
            lower.spectrum.measure(e.f_lo_hz, e.f_hi_hz),
            higher.spectrum.measure(e.f_lo_hz, e.f_hi_hz),
        ) else {
            continue;
        };
        rows.push(Meas {
            idx,
            snr_a: a.snr_db,
            snr_b: b.snr_db,
            lvl_a: a.level_dbfs,
            lvl_b: b.level_dbfs,
            spread_a: a.spread_db,
            spread_b: b.spread_db,
            det_a: lower.emitters.iter().any(|x| overlaps(x, e)),
            clean: !(e.spur || e.dc || e.image),
            bw: e.bandwidth_hz,
        });
    }
    let mut anchors: Vec<f64> = rows
        .iter()
        .filter(|r| {
            r.det_a
                && r.clean
                && r.snr_a >= cfg.anchor_snr_db
                && r.snr_b >= cfg.anchor_snr_db
                && r.bw >= cfg.anchor_min_bandwidth_hz
        })
        .map(|r| r.lvl_b - r.lvl_a)
        .collect();
    if anchors.len() < cfg.min_anchors {
        anchors = rows
            .iter()
            .filter(|r| r.det_a && r.clean && r.snr_a >= cfg.fallback_anchor_snr_db)
            .map(|r| r.lvl_b - r.lvl_a)
            .collect();
    }
    let g_lin_db = if anchors.len() >= cfg.min_anchors {
        median(&mut anchors)
    } else {
        nominal_db
    };
    let bound_db = (g_lin_db - delta_floor_db).max(0.0);
    result.g_lin_db = g_lin_db;
    result.anchors = if anchors.len() >= cfg.min_anchors {
        anchors.len()
    } else {
        0
    };
    result.delta_floor_db = delta_floor_db;
    result.bound_db = bound_db;
    for r in rows {
        let bursty = (r.spread_a > cfg.burst_spread_db && r.snr_a >= 0.0)
            || (r.spread_b > cfg.burst_spread_db && r.snr_b >= 0.0);
        let (delta_snr_db, certain) = if r.snr_a >= cfg.measurability_limit_db {
            (r.snr_b - r.snr_a, true)
        } else {
            (r.snr_b - cfg.measurability_limit_db, false)
        };
        let delta_level_db = r.lvl_b - r.lvl_a;
        let verdict = if bursty {
            GainStepVerdict::InconclusiveBursty
        } else if delta_snr_db > bound_db + cfg.imd_margin_db {
            GainStepVerdict::SuspectImd
        } else if !certain {
            GainStepVerdict::InconclusiveWeak
        } else if r.snr_a >= cfg.anchor_snr_db && delta_level_db < g_lin_db - cfg.imd_margin_db {
            GainStepVerdict::Compressed
        } else {
            GainStepVerdict::Linear
        };
        result.rows.push(GainStepRow {
            emitter: r.idx,
            snr_low_db: r.snr_a,
            snr_high_db: r.snr_b,
            delta_snr_db,
            delta_snr_certain: certain,
            delta_level_db,
            detected_low: r.det_a,
            verdict,
        });
    }
    result
}

/// Retune verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetuneLabel {
    /// Same absolute frequency in both captures.
    Stays,
    /// Moved by +Δ with the LO (LO-relative spur, DC, baseband product).
    MovesWithLo,
    /// Moved by −Δ.
    MovesAgainstLo,
    /// Moved by 2Δ: an IQ image.
    ImageMoves,
    /// Not found in the other capture.
    NotReproduced,
}

impl RetuneLabel {
    /// Applies the verdict: LO-relative → `spur_candidate` (`lo-relative`); image →
    /// `image_candidate` + `image_retune_confirmed`; not reproduced → `marginal`.
    pub fn apply(self, flags: &mut DetectionFlags) {
        match self {
            RetuneLabel::Stays => {}
            RetuneLabel::MovesWithLo | RetuneLabel::MovesAgainstLo => {
                flags.spur_candidate = true;
                if flags.spur_reason.is_none() {
                    flags.spur_reason = Some(SpurReason::LoRelative);
                }
            }
            RetuneLabel::ImageMoves => {
                flags.image_candidate = true;
                flags.image_retune_confirmed = true;
            }
            RetuneLabel::NotReproduced => flags.marginal = true,
        }
    }
}

/// Which capture a retune row belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureSide {
    /// The first argument.
    A,
    /// The second argument.
    B,
}

/// Retune settings (S4 defaults).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RetuneConfig {
    /// Coverage tolerance, Hz (10 kHz).
    pub frequency_tolerance_hz: f64,
    /// Similar peak excess, dB (4).
    pub level_tolerance_db: f64,
    /// Similar bandwidth ratio (2).
    pub bandwidth_ratio: f64,
}

impl Default for RetuneConfig {
    fn default() -> Self {
        Self {
            frequency_tolerance_hz: 10e3,
            level_tolerance_db: 4.0,
            bandwidth_ratio: 2.0,
        }
    }
}

/// One classified emitter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RetuneRow {
    /// Capture.
    pub capture: CaptureSide,
    /// Index into that capture's emitters.
    pub emitter: usize,
    /// Centre, Hz.
    pub f_center_hz: f64,
    /// Verdict.
    pub label: RetuneLabel,
}

/// Retune result.
#[derive(Clone, Debug, PartialEq)]
pub struct RetuneResult {
    /// `B.center − A.center`, Hz.
    pub delta_hz: f64,
    /// Common usable span, Hz.
    pub span_lo_hz: f64,
    /// See `span_lo_hz`.
    pub span_hi_hz: f64,
    /// Rows for emitters of both captures inside the common span.
    pub rows: Vec<RetuneRow>,
}

/// Rule 7: classifies emitters of `a` and `b` (same gain, centres Δ apart).
pub fn retune(a: &CaptureResult, b: &CaptureResult, cfg: &RetuneConfig) -> RetuneResult {
    let delta_hz = b.center_hz - a.center_hz;
    let (ua, ub) = (
        a.spectrum.geometry.usable_half_hz,
        b.spectrum.geometry.usable_half_hz,
    );
    let span_lo_hz = (a.center_hz - ua).max(b.center_hz - ub);
    let span_hi_hz = (a.center_hz + ua).min(b.center_hz + ub);
    let mut rows = Vec::new();
    for (x, y, side, sign) in [(a, b, CaptureSide::A, 1.0), (b, a, CaptureSide::B, -1.0)] {
        let d = sign * delta_hz;
        for (i, e) in x.emitters.iter().enumerate() {
            if !(span_lo_hz <= e.f_center_hz && e.f_center_hz <= span_hi_hz) {
                continue;
            }
            let tol = cfg.frequency_tolerance_hz;
            let covers = |f: f64| {
                y.emitters
                    .iter()
                    .filter(move |c| c.f_lo_hz - tol <= f && f <= c.f_hi_hz + tol)
            };
            let label = if covers(e.f_center_hz).next().is_some() {
                RetuneLabel::Stays
            } else {
                let similar = |c: &CaptureEmitter| {
                    (c.peak_excess_dbfs - e.peak_excess_dbfs).abs() <= cfg.level_tolerance_db
                        && c.bandwidth_hz / e.bandwidth_hz >= 1.0 / cfg.bandwidth_ratio
                        && c.bandwidth_hz / e.bandwidth_hz <= cfg.bandwidth_ratio
                };
                [
                    (d, RetuneLabel::MovesWithLo),
                    (-d, RetuneLabel::MovesAgainstLo),
                    (2.0 * d, RetuneLabel::ImageMoves),
                ]
                .into_iter()
                .find(|&(shift, _)| covers(e.f_center_hz + shift).any(&similar))
                .map_or(RetuneLabel::NotReproduced, |(_, l)| l)
            };
            rows.push(RetuneRow {
                capture: side,
                emitter: i,
                f_center_hz: e.f_center_hz,
                label,
            });
        }
    }
    RetuneResult {
        delta_hz,
        span_lo_hz,
        span_hi_hz,
        rows,
    }
}
