//! The detector: one call per [`SpectrumFrame`] + [`FloorFrame`], emitting [`DetectorEvent`]s.
//! See the [crate docs](crate) for the pipeline.

use std::sync::Arc;

use hk_core::ProvenanceHandle;
use hk_dsp::SpectrumFrame;
use hk_dsp::floor::FloorFrame;
use hk_dsp::window::WindowKind;
use hk_model::detection::SpurReason;
use hk_model::{Detection, DetectionFlags, DetectionId, TimeRange};

use crate::alpha::db;
use crate::cfar::{CELL_NONE, CfarEngine, ClassifyStats, Thresholds};
use crate::clip::ClipCount;
use crate::comb::CombFinder;
use crate::components::{Component, Cumulative, FrameView, LabelParams, Labeler, Mark, Ready};
use crate::config::{Branches, ConfigError, DetectorConfig, FloorReference, RunContext};
use crate::integrated::{IntegratedEvaluation, IntegratedSnapshot, IntegratedSpectrum};
use crate::record::{
    Candidate, CloseReason, ConfirmReason, Confirmation, DetectionRecord, DetectorEvent,
    ImageEvidence,
};
use crate::rules::{Geometry, Pearson, edge_hit, spur_decision};
use crate::step::{GuardFrame, ShapeView, StepGuard, WideView};
use crate::trust::{CaptureEmitter, CaptureResult, GainState};

/// Nominal HackRF One RX amp gain used for `total_gain_db` (S4: not verified on this unit).
pub const AMP_NOMINAL_DB: f64 = 11.0;

/// State fixed between transitions.
#[derive(Clone, Debug, PartialEq)]
pub struct SegmentInfo {
    /// Detector segment counter.
    pub segment: u64,
    /// The floor tracker's segment.
    pub floor_segment: u64,
    /// Provenance of every frame in the segment.
    pub provenance: ProvenanceHandle,
    /// Bin geometry and edge zone.
    pub geometry: Geometry,
    /// Thresholds.
    pub thresholds: Thresholds,
    /// Profile index ([`DetectorConfig::profile_at`]).
    pub profile_index: usize,
    /// Branches of the profile.
    pub branches: Branches,
    /// Seconds between frames.
    pub frame_period_s: f64,
    /// Frames before a split.
    pub max_frames: u64,
    /// LNA + VGA + nominal amp, dB (for gain-specific spur-map rules).
    pub total_gain_db: f64,
    /// `Detection::detector_version` of the segment (interned per configuration and resolution).
    pub detector_version: Arc<str>,
    n_avg_bits: u64,
    cum_at_start: Cumulative,
}

/// Counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DetectorStats {
    /// Frames processed.
    pub frames: u64,
    /// Segments started.
    pub segments: u64,
    /// Detection events emitted (including impulsive events).
    pub detections: u64,
    /// Impulsive events emitted.
    pub impulsive_events: u64,
    /// Confirmation events emitted.
    pub confirmations: u64,
    /// Integrated evaluations.
    pub evaluations: u64,
    /// Frames with no valid floor (nothing detected).
    pub invalid_floor_frames: u64,
    /// Frames with more runs than `max_runs_per_frame` (not labelled).
    pub dense_frames: u64,
    /// Runs dropped at `max_live_components`.
    pub dropped_runs: u64,
    /// Frames in which the floor-step guard moved the floor branch off the per-frame reference
    /// somewhere (to the wide reference or off).
    pub guarded_frames: u64,
    /// Frames in which some guarded bins ran the floor branch on the wide reference.
    pub wide_guarded_frames: u64,
    /// Frames whose block floors did not match the step guard's layout (guard inactive).
    pub step_guard_unavailable: u64,
}

#[derive(Clone, Copy, Debug)]
struct Recent {
    id: DetectionId,
    segment: u64,
    f_center: f64,
    obw: f64,
    f_lo: f64,
    f_hi: f64,
    t_end: i64,
    confirmed: bool,
}

/// What a record needs beyond the row to merge into an impulsive event.
#[derive(Clone, Copy, Debug)]
struct Sums {
    pixels: u64,
    ratio_sum: f64,
    sk_sum: f64,
    sk_count: u64,
    cum_before: Cumulative,
    cum_after: Cumulative,
    run: u64,
}

#[derive(Debug)]
struct ImpulsiveEntry {
    record: DetectionRecord,
    sums: Sums,
    last_merge: u64,
    version: Arc<str>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct VersionKey {
    profile: usize,
    n_bits: u64,
    fft_len: usize,
    overlap: usize,
    window: WindowKind,
}

/// The CFAR detector (C09). See the [crate docs](crate).
pub struct Detector {
    config: DetectorConfig,
    versions: Vec<(VersionKey, Arc<str>)>,
    cfar: CfarEngine,
    codes: Vec<u8>,
    labeler: Labeler,
    step: Option<StepGuard>,
    /// The last frame's floor reference as the floor branch used it (wide in guarded zones).
    eff_floor: Vec<f32>,
    /// The last frame's floor-branch mask (per-frame or wide reference).
    floor_ok: Vec<bool>,
    guard_active: bool,
    integrated: IntegratedSpectrum,
    comb: CombFinder,
    seg: Option<SegmentInfo>,
    frame_index: u64,
    cum: Cumulative,
    impulsive_run: u64,
    in_impulsive: bool,
    gap_frames: u32,
    aggregator: Vec<ImpulsiveEntry>,
    recent: Vec<Option<Recent>>,
    recent_head: usize,
    final_reason: CloseReason,
    stats: DetectorStats,
    last_classify: ClassifyStats,
    last_capture: Option<CaptureResult>,
    retain_capture: bool,
}

impl Detector {
    /// A detector with `config`.
    pub fn new(config: DetectorConfig) -> Result<Self, ConfigError> {
        config.validate()?;
        let cap = config.confirm.recent_capacity;
        Ok(Self {
            versions: Vec::with_capacity(8),
            cfar: CfarEngine::new(config.window, 0),
            codes: Vec::new(),
            labeler: Labeler::new(0, config.profile.gap_frames),
            step: config.step_guard.map(StepGuard::new),
            eff_floor: Vec::new(),
            floor_ok: Vec::new(),
            guard_active: false,
            integrated: IntegratedSpectrum::new(config.integration, config.rules.comb.max_lines),
            comb: CombFinder::new(config.rules.comb),
            seg: None,
            frame_index: 0,
            cum: Cumulative::default(),
            impulsive_run: 0,
            in_impulsive: false,
            gap_frames: config.profile.gap_frames,
            aggregator: Vec::with_capacity(16),
            recent: vec![None; cap],
            recent_head: 0,
            final_reason: CloseReason::Ended,
            stats: DetectorStats::default(),
            last_classify: ClassifyStats::default(),
            last_capture: None,
            retain_capture: false,
            config,
        })
    }

    /// Keep a [`CaptureResult`] of each segment when it ends (allocates at segment end), for
    /// [`Detector::last_capture`] after [`Detector::finish`].
    pub fn retain_capture_results(&mut self, on: bool) {
        self.retain_capture = on;
    }

    /// Settings.
    pub fn config(&self) -> &DetectorConfig {
        &self.config
    }

    /// Counters.
    pub fn stats(&self) -> DetectorStats {
        self.stats
    }

    /// The current segment.
    pub fn segment(&self) -> Option<&SegmentInfo> {
        self.seg.as_ref()
    }

    /// Cell classes of the last frame ([`crate::CELL_SEED`], [`crate::CELL_REGION`], none).
    pub fn codes(&self) -> &[u8] {
        &self.codes
    }

    /// Where the floor branch ran in the last frame, on either reference (`None` when no step
    /// guard was active: everywhere).
    pub fn floor_branch_mask(&self) -> Option<&[bool]> {
        self.guard_active.then_some(self.floor_ok.as_slice())
    }

    /// The floor reference of the last frame as the floor branch and the OS guard used it: the
    /// configured [`FloorReference`], with the wide reference in guarded zones where the step
    /// guard allows it.
    pub fn floor_branch_reference(&self) -> &[f32] {
        &self.eff_floor
    }

    /// The floor-step guard (its per-frame and wide masks are those of the last frame).
    pub fn step_guard(&self) -> Option<&StepGuard> {
        self.step.as_ref()
    }

    /// Classification counts of the last frame.
    pub fn last_classify(&self) -> ClassifyStats {
        self.last_classify
    }

    /// Live components (open or waiting to be emitted).
    pub fn live_components(&self) -> usize {
        self.labeler.live_count()
    }

    /// Heap bytes held by the component labeller.
    pub fn labeler_memory_bytes(&self) -> usize {
        self.labeler.memory_bytes()
    }

    /// The latest integrated evaluation in the current segment.
    pub fn integrated_evaluation(&self) -> Option<&IntegratedEvaluation> {
        self.integrated.evaluation()
    }

    /// Integrated spectra + emitters of the current segment's latest evaluation, for the
    /// cross-capture trust tests (allocates).
    pub fn capture_result(&self) -> Option<CaptureResult> {
        let seg = self.seg.as_ref()?;
        let spectrum: IntegratedSnapshot = self.integrated.snapshot(&seg.geometry)?;
        let eval = self.integrated.evaluation()?;
        let tune = &seg.provenance.tune;
        Some(CaptureResult {
            center_hz: seg.geometry.center_hz,
            gain: GainState::of(tune),
            quantisation_limited: seg.provenance.quantisation_limited
                || self.cum.quantisation_frames > seg.cum_at_start.quantisation_frames,
            clipped: seg.provenance.overload || self.cum.clip_frames > seg.cum_at_start.clip_frames,
            spectrum,
            emitters: eval.emitters.iter().map(CaptureEmitter::from).collect(),
        })
    }

    /// The capture result retained when the last segment ended ([`Self::retain_capture_results`]).
    pub fn last_capture(&self) -> Option<&CaptureResult> {
        self.last_capture.as_ref()
    }

    /// Processes one frame. `floor` must be the tracker's output for `frame`; `clip` the clipped
    /// samples in the frame's span ([`ClipCount::NONE`] when unknown).
    pub fn process<F>(
        &mut self,
        frame: &SpectrumFrame,
        floor: &FloorFrame,
        clip: ClipCount,
        out: &mut F,
    ) where
        F: FnMut(DetectorEvent<'_>),
    {
        let spec = &frame.spectrum;
        assert_eq!(
            floor.floor.len(),
            spec.bins(),
            "floor frame does not match the spectrum"
        );
        if self.needs_segment(frame, floor) {
            self.end_segment(CloseReason::Transition, out);
            self.begin_segment(frame, floor);
        }
        self.frame_index += 1;
        let t = self.frame_index;
        self.stats.frames += 1;
        let (thresholds, geometry, profile_index, max_frames, branches) = {
            let s = self.seg.as_ref().expect("segment started");
            (
                s.thresholds,
                s.geometry,
                s.profile_index,
                s.max_frames,
                s.branches,
            )
        };
        let fs = spec.sample_rate_hz;
        let dur_ns = (frame.sample_count as f64 * 1e9 / fs).round() as i64;
        let start = Mark {
            sample: frame.t.sample_index,
            time: frame.t.host_time,
        };
        let end = Mark {
            sample: frame.t.sample_index + frame.sample_count,
            time: frame.t.host_time.saturating_add_nanos(dur_ns),
        };
        let before = self.cum;
        self.cum.frames += 1;
        if clip.fraction() > self.config.rules.clip_fraction {
            self.cum.clip_frames += 1;
            self.cum.clip_samples += clip.clipped;
        }
        if floor.quantisation_limited {
            self.cum.quantisation_frames += 1;
        }
        if !floor.valid {
            self.cum.invalid_frames += 1;
            self.stats.invalid_floor_frames += 1;
        }
        let run = if floor.impulsive {
            self.cum.impulsive_frames += 1;
            if !self.in_impulsive {
                self.impulsive_run += 1;
                self.in_impulsive = true;
            }
            Some(self.impulsive_run)
        } else {
            self.in_impulsive = false;
            None
        };
        let after = self.cum;
        let reference: &[f32] = match self.config.floor_reference {
            FloorReference::PerFrame => &floor.floor,
            FloorReference::Wide => &floor.wide_floor,
        };
        self.eff_floor.copy_from_slice(reference);
        self.guard_active = false;
        if floor.valid {
            if let Some(g) = &mut self.step
                && branches != Branches::OsOnly
            {
                // The shape is trusted from the tracker's `wide_min_frames`-th segment frame.
                let shape =
                    (floor.frames_in_segment >= g.config().wide_min_frames).then_some(ShapeView {
                        shape: &floor.shape,
                        norm_block_floor: &floor.norm_block_floor,
                    });
                let wide =
                    (self.config.floor_reference == FloorReference::Wide).then_some(WideView {
                        floor: &floor.floor,
                        wide_floor: &floor.wide_floor,
                    });
                g.update(
                    GuardFrame {
                        psd: &spec.psd,
                        n_avg_effective: floor.n_avg_effective,
                        impulsive: floor.impulsive,
                        block_floor: &floor.block_floor,
                        shape,
                        wide,
                    },
                    &geometry,
                    &self.config.response_edges_hz,
                );
                self.stats.guarded_frames += u64::from(g.guarded_bins() > 0);
                self.stats.wide_guarded_frames += u64::from(g.wide_bins() > 0);
                self.stats.step_guard_unavailable += u64::from(!g.available());
                let with_frame_ref = g.frame_ref_bins() > 0;
                for (b, ((ok, eff), (&m, &w))) in self
                    .floor_ok
                    .iter_mut()
                    .zip(self.eff_floor.iter_mut())
                    .zip(g.mask().iter().zip(g.wide_mask()))
                    .enumerate()
                {
                    *ok = m || w;
                    if w {
                        *eff = floor.wide_floor[b];
                    } else if with_frame_ref && g.frame_ref_mask()[b] {
                        *eff = floor.floor[b];
                    }
                    if !m && !w {
                        // OS-only: the guard's upper block-floor envelope.
                        *eff = eff.max(g.os_floor()[b]);
                    }
                }
                self.guard_active = true;
            }
            self.last_classify = self.cfar.classify(
                &spec.psd,
                &self.eff_floor,
                &thresholds,
                branches,
                self.guard_active.then_some(self.floor_ok.as_slice()),
                &mut self.codes,
            );
            // The integrated spectrum averages the reference the floor branch used (T-033: the
            // wide reference in guarded zones it may use, the per-frame floor over floor features).
            if !floor.impulsive
                && self
                    .integrated
                    .push(&spec.psd, &self.eff_floor, start.time, end.time)
            {
                self.evaluate_integrated(false, out);
            }
        } else {
            self.codes.fill(CELL_NONE);
            self.last_classify = ClassifyStats::default();
        }
        let profile = self.config.profile_at(profile_index);
        let params = LabelParams {
            min_frames: profile.min_frames,
            gap_frames: profile.gap_frames,
            max_frames,
            max_hold_frames: self.config.max_hold_frames,
            max_runs: self.config.max_runs_per_frame,
            max_live: self.config.max_live_components,
        };
        let view = FrameView {
            index: t,
            start,
            end,
            psd: &spec.psd,
            floor: &self.eff_floor,
            sk: &spec.sk,
            codes: &self.codes,
            bin_width_hz: geometry.bin_width_hz,
            impulsive_run: run,
            cum_before: before,
            cum_after: after,
        };
        let outcome = self.labeler.process_frame(&view, &params);
        self.stats.dense_frames += u64::from(outcome.dense);
        self.stats.dropped_runs += outcome.dropped_runs as u64;
        self.drain_ready(out);
        self.poll_aggregator(false, out);
    }

    /// Ends the stream: evaluates the partial integration, closes every box and flushes the
    /// impulsive events.
    pub fn finish<F>(&mut self, out: &mut F)
    where
        F: FnMut(DetectorEvent<'_>),
    {
        self.end_segment(CloseReason::EndOfStream, out);
    }

    fn needs_segment(&self, frame: &SpectrumFrame, floor: &FloorFrame) -> bool {
        let Some(s) = &self.seg else {
            return true;
        };
        let spec = &frame.spectrum;
        floor.segment != s.floor_segment
            || frame.provenance.id() != s.provenance.id()
            || frame.discontinuity.bits() & self.config.reset_on.bits() != 0
            || spec.bins() != s.geometry.bins
            || spec.f_center_hz.to_bits() != s.geometry.center_hz.to_bits()
            || spec.sample_rate_hz.to_bits() != s.geometry.sample_rate_hz.to_bits()
            || floor.n_avg_effective.to_bits() != s.n_avg_bits
    }

    fn version_for(&mut self, key: VersionKey) -> Arc<str> {
        if let Some((_, v)) = self.versions.iter().find(|(k, _)| *k == key) {
            return v.clone();
        }
        let run = RunContext {
            n_eff: f64::from_bits(key.n_bits),
            fft_len: key.fft_len,
            overlap: key.overlap,
            window: key.window,
        };
        let v: Arc<str> = self.config.detector_version_for(key.profile, &run).into();
        self.versions.push((key, v.clone()));
        v
    }

    fn begin_segment(&mut self, frame: &SpectrumFrame, floor: &FloorFrame) {
        self.stats.segments += 1;
        let spec = &frame.spectrum;
        let prov = frame.provenance.clone();
        let bins = spec.bins();
        let fs = spec.sample_rate_hz;
        let fc = spec.f_center_hz;
        let geometry = Geometry::new(
            fc,
            fs,
            bins,
            prov.tune.bandwidth_hz,
            &self.config.rules.edge,
        );
        let profile_index = self.config.profile_index(fc);
        let profile = self.config.profile_at(profile_index);
        let n = floor.n_avg_effective;
        let thresholds = Thresholds::new(n, &self.config.window, profile, self.config.guard_db);
        let branches = profile.branches;
        let gap_frames = profile.gap_frames;
        let res = spec.resolution;
        let frame_period_s = f64::from(res.n_avg) * res.hop() as f64 / fs;
        let max_frames = ((self.config.max_duration_s / frame_period_s).ceil() as u64).max(1);
        let detector_version = self.version_for(VersionKey {
            profile: profile_index,
            n_bits: n.to_bits(),
            fft_len: res.fft_len,
            overlap: res.overlap,
            window: res.window,
        });
        self.gap_frames = gap_frames;
        self.cfar.resize(bins);
        self.codes.resize(bins, CELL_NONE);
        self.labeler.reset(bins, gap_frames);
        if let Some(g) = &mut self.step {
            g.reset(bins);
        }
        self.eff_floor.resize(bins, 0.0);
        self.floor_ok.resize(bins, true);
        self.floor_ok.fill(true);
        self.guard_active = false;
        self.integrated
            .configure(bins, frame_period_s, self.stats.segments);
        self.in_impulsive = false;
        let t = &prov.tune;
        let total_gain_db = t.lna_db + t.vga_db + if t.amp_on { AMP_NOMINAL_DB } else { 0.0 };
        self.seg = Some(SegmentInfo {
            segment: self.stats.segments,
            floor_segment: floor.segment,
            provenance: prov,
            geometry,
            thresholds,
            profile_index,
            branches,
            frame_period_s,
            max_frames,
            total_gain_db,
            detector_version,
            n_avg_bits: n.to_bits(),
            cum_at_start: self.cum,
        });
    }

    fn end_segment<F>(&mut self, reason: CloseReason, out: &mut F)
    where
        F: FnMut(DetectorEvent<'_>),
    {
        if self.seg.is_none() {
            return;
        }
        if self.integrated.has_unevaluated() {
            self.evaluate_integrated(true, out);
        }
        if self.retain_capture {
            self.last_capture = self.capture_result();
        }
        self.final_reason = reason;
        self.labeler.close_all();
        self.drain_ready(out);
        self.final_reason = CloseReason::Ended;
        self.poll_aggregator(true, out);
        self.in_impulsive = false;
        self.seg = None;
    }

    fn evaluate_integrated<F>(&mut self, include_partial: bool, out: &mut F)
    where
        F: FnMut(DetectorEvent<'_>),
    {
        let Some(seg) = &self.seg else {
            return;
        };
        let geometry = seg.geometry;
        let segment = seg.segment;
        // Guarded bins take part where the floor branch ran on the wide reference (T-033).
        let mask = (seg.branches != Branches::OsOnly && self.step.is_some())
            .then_some(self.floor_ok.as_slice());
        if !self.integrated.evaluate(
            include_partial,
            &self.config.rules,
            &geometry,
            mask.filter(|m| m.len() == geometry.bins),
            &mut self.comb,
        ) {
            return;
        }
        self.stats.evaluations += 1;
        let Some(eval) = self.integrated.evaluation() else {
            return;
        };
        if eval.confirming {
            let t0 = eval.t_start.as_unix_nanos();
            for r in self.recent.iter_mut().flatten() {
                if r.segment != segment || r.confirmed || r.t_end < t0 {
                    continue;
                }
                if let Some(em) = eval
                    .emitters
                    .iter()
                    .find(|em| em.f_lo_hz <= r.f_hi && r.f_lo <= em.f_hi_hz)
                {
                    r.confirmed = true;
                    self.stats.confirmations += 1;
                    out(DetectorEvent::Confirmed(Confirmation {
                        detection: r.id,
                        reason: ConfirmReason::Integrated {
                            f_lo_hz: em.f_lo_hz,
                            f_hi_hz: em.f_hi_hz,
                        },
                    }));
                }
            }
        }
        out(DetectorEvent::Integrated(eval));
    }

    fn drain_ready<F>(&mut self, out: &mut F)
    where
        F: FnMut(DetectorEvent<'_>),
    {
        let mut ready = std::mem::take(&mut self.labeler.ready);
        for &(id, kind) in &ready {
            let (pixels, impulsive) = {
                let s = &self.labeler.component(id).s;
                (
                    s.pixels,
                    s.impulsive_pixels as f64
                        > self.config.rules.impulsive_fraction * s.pixels as f64,
                )
            };
            let built = (pixels > 0).then(|| {
                let seg = self.seg.as_ref().expect("segment");
                let close = match kind {
                    Ready::Final => self.final_reason,
                    Ready::Split => CloseReason::MaxDuration,
                };
                build_record(
                    self.labeler.component(id),
                    seg,
                    &self.config,
                    close,
                    kind == Ready::Split,
                    self.integrated.evaluation(),
                )
            });
            match kind {
                Ready::Final => self.labeler.release(id),
                Ready::Split => self.labeler.split(id),
            }
            if let Some((record, sums)) = built {
                if impulsive {
                    self.aggregate(record, sums);
                } else {
                    self.emit(record, out);
                }
            }
        }
        ready.clear();
        self.labeler.ready = ready;
    }

    fn emit<F>(&mut self, mut rec: DetectionRecord, out: &mut F)
    where
        F: FnMut(DetectorEvent<'_>),
    {
        let c = self.config.confirm;
        let df = (rec.f_hi_hz - rec.f_lo_hz) / rec.bins.len().max(1) as f64;
        let tol_of = |obw: f64| (c.min_tolerance_bins * df).max(c.bandwidth_fraction * obw);
        let fc = rec.detection.f_center_hz;
        let t_start = rec.detection.time.start.as_unix_nanos();
        let window = (c.repeat_window_s * 1e9) as i64;
        let cap = self.recent.len();
        let mut found = None;
        for k in 0..cap {
            let idx = (self.recent_head + cap - 1 - k) % cap;
            if let Some(r) = &self.recent[idx] {
                let tol = tol_of(rec.detection.obw_hz).max(tol_of(r.obw));
                // Consistent frequency: within the C10 gate, or each centre inside the other's
                // occupied extent (payload-dependent centroids of wide bursts move by more than
                // 10 % of the bandwidth; for 1–2-bin boxes this is ±1 bin, S4's track rule).
                let df_c = (r.f_center - fc).abs();
                let consistent = df_c <= tol || df_c <= 0.5 * r.obw.min(rec.detection.obw_hz);
                if consistent && r.t_end >= t_start.saturating_sub(window) {
                    found = Some(idx);
                    break;
                }
            }
        }
        if let Some(idx) = found {
            let r = self.recent[idx].as_mut().expect("found");
            rec.candidate = Candidate::Repeat { with: r.id };
            if !r.confirmed {
                r.confirmed = true;
                self.stats.confirmations += 1;
                out(DetectorEvent::Confirmed(Confirmation {
                    detection: r.id,
                    reason: ConfirmReason::Repeat {
                        with: rec.detection.id,
                    },
                }));
            }
        } else if let Some(eval) = self.integrated.evaluation()
            && eval.confirming
            && eval.segment == rec.segment
            && let Some(em) = eval
                .emitters
                .iter()
                .find(|em| em.f_lo_hz <= rec.f_hi_hz && rec.f_lo_hz <= em.f_hi_hz)
        {
            rec.candidate = Candidate::Integrated {
                f_lo_hz: em.f_lo_hz,
                f_hi_hz: em.f_hi_hz,
            };
        }
        self.recent[self.recent_head] = Some(Recent {
            id: rec.detection.id,
            segment: rec.segment,
            f_center: fc,
            obw: rec.detection.obw_hz,
            f_lo: rec.f_lo_hz,
            f_hi: rec.f_hi_hz,
            t_end: rec.detection.time.end.as_unix_nanos(),
            confirmed: rec.candidate.is_confirmed(),
        });
        self.recent_head = (self.recent_head + 1) % cap;
        self.stats.detections += 1;
        if let Some(seg) = &self.seg {
            rec.detection.detector_version = String::from(&*seg.detector_version);
        }
        out(DetectorEvent::Detection(rec));
    }

    fn aggregate(&mut self, mut record: DetectionRecord, sums: Sums) {
        let t = self.frame_index;
        if let Some(e) = self.aggregator.iter_mut().find(|e| e.sums.run == sums.run) {
            let r = &mut e.record;
            let d = &record.detection;
            r.f_lo_hz = r.f_lo_hz.min(record.f_lo_hz);
            r.f_hi_hz = r.f_hi_hz.max(record.f_hi_hz);
            r.bins = r.bins.start.min(record.bins.start)..r.bins.end.max(record.bins.end);
            r.frames = r.frames.start.min(record.frames.start)..r.frames.end.max(record.frames.end);
            r.samples =
                r.samples.start.min(record.samples.start)..r.samples.end.max(record.samples.end);
            r.detection.time = TimeRange::new(
                r.detection.time.start.min(d.time.start),
                r.detection.time.end.max(d.time.end),
            );
            r.detection.snr_peak_db = r.detection.snr_peak_db.max(d.snr_peak_db);
            r.detection.peak_level_dbfs = r.detection.peak_level_dbfs.max(d.peak_level_dbfs);
            r.detection.flags.clipped |= d.flags.clipped;
            r.detection.flags.marginal |= d.flags.marginal;
            r.detection.flags.edge |= d.flags.edge;
            r.pixels += record.pixels;
            r.merged_boxes += record.merged_boxes;
            r.inconclusive |= record.inconclusive;
            let s = &mut e.sums;
            s.pixels += sums.pixels;
            s.ratio_sum += sums.ratio_sum;
            s.sk_sum += sums.sk_sum;
            s.sk_count += sums.sk_count;
            if sums.cum_before.frames < s.cum_before.frames {
                s.cum_before = sums.cum_before;
            }
            if sums.cum_after.frames > s.cum_after.frames {
                s.cum_after = sums.cum_after;
            }
            e.last_merge = t;
            finish_impulsive(&mut e.record, &e.sums);
        } else if let Some(seg) = &self.seg {
            finish_impulsive(&mut record, &sums);
            let version = seg.detector_version.clone();
            self.aggregator.push(ImpulsiveEntry {
                record,
                sums,
                last_merge: t,
                version,
            });
        }
    }

    fn poll_aggregator<F>(&mut self, force: bool, out: &mut F)
    where
        F: FnMut(DetectorEvent<'_>),
    {
        let t = self.frame_index;
        let gap = u64::from(self.gap_frames);
        let mut i = 0;
        while i < self.aggregator.len() {
            let e = &self.aggregator[i];
            let run_active = self.in_impulsive && self.impulsive_run == e.sums.run;
            if force || (t >= e.last_merge + gap + 3 && !run_active) {
                let mut e = self.aggregator.swap_remove(i);
                self.stats.impulsive_events += 1;
                self.stats.detections += 1;
                e.record.detection.detector_version = String::from(&*e.version);
                out(DetectorEvent::Detection(e.record));
            } else {
                i += 1;
            }
        }
    }
}

/// Normalises a (merged) impulsive record: broadband extent, no spur/image interpretation.
fn finish_impulsive(r: &mut DetectionRecord, s: &Sums) {
    let d = &mut r.detection;
    d.flags.impulsive = true;
    d.flags.spur_candidate = false;
    d.flags.spur_reason = None;
    d.flags.image_candidate = false;
    d.flags.image_retune_confirmed = false;
    r.image = None;
    r.spur_harmonic_hz = None;
    let width = (r.f_hi_hz - r.f_lo_hz).max(0.0);
    d.f_center_hz = 0.5 * (r.f_lo_hz + r.f_hi_hz);
    d.obw_hz = width;
    d.xdb_bandwidth_hz = Some(width);
    if s.pixels > 0 {
        d.snr_mean_db = db(s.ratio_sum / s.pixels as f64);
    }
    d.sk = (s.sk_count > 0).then_some(s.sk_sum / s.sk_count.max(1) as f64);
    let clips = s
        .cum_after
        .clip_samples
        .saturating_sub(s.cum_before.clip_samples);
    d.clip_count = clips.min(u64::from(u32::MAX)) as u32;
    d.flags.clipped |= d.clip_count > 0 || s.cum_after.clip_frames > s.cum_before.clip_frames;
}

/// Builds the record of a finished (or split) component.
fn build_record(
    comp: &Component,
    seg: &SegmentInfo,
    cfg: &DetectorConfig,
    close: CloseReason,
    continues: bool,
    eval: Option<&IntegratedEvaluation>,
) -> (DetectionRecord, Sums) {
    let s = &comp.s;
    let acc = &comp.acc;
    let g = &seg.geometry;
    let df = g.bin_width_hz;
    let rules = &cfg.rules;
    let (lo, hi) = (s.bin_lo, s.bin_hi);
    let (f_lo, f_hi) = g.extent_hz(lo, hi);

    // Burst-gated spectrum: per-bin excess summed over the component's cells.
    let (mut total, mut ex_f, mut max_sum) = (0.0f64, 0.0f64, 0.0f64);
    for (b, a) in acc.iter() {
        let ex = f64::from(a.excess);
        total += ex;
        ex_f += ex * g.bin_hz(b as f64);
        max_sum = max_sum.max(ex);
    }
    let f_center = if total > 0.0 {
        ex_f / total
    } else {
        0.5 * (f_lo + f_hi)
    };
    let range = acc.range();
    let obw = if total > 0.0 {
        let tail = 0.5 * (1.0 - cfg.obw_fraction) * total;
        let mut cum = 0.0;
        let mut b_lo = range.start;
        for (b, a) in acc.iter() {
            cum += f64::from(a.excess);
            if cum > tail {
                b_lo = b;
                break;
            }
        }
        cum = 0.0;
        let mut b_hi = range.end - 1;
        for (b, a) in acc.iter().rev() {
            cum += f64::from(a.excess);
            if cum > tail {
                b_hi = b;
                break;
            }
        }
        (b_hi.saturating_sub(b_lo) + 1) as f64 * df
    } else {
        (hi - lo) as f64 * df
    };
    let xdb = if max_sum > 0.0 {
        let thr = max_sum * 10f64.powf(-cfg.xdb_level_db / 10.0);
        let first = acc
            .iter()
            .find(|(_, a)| f64::from(a.excess) >= thr)
            .map(|(b, _)| b);
        let last = acc
            .iter()
            .rev()
            .find(|(_, a)| f64::from(a.excess) >= thr)
            .map(|(b, _)| b);
        match (first, last) {
            (Some(a), Some(b)) => (b - a + 1) as f64 * df,
            _ => df,
        }
    } else {
        (hi - lo) as f64 * df
    };

    let snr_peak_db = db(f64::from(s.ratio_peak).max(1e-30));
    let snr_mean_db = db((s.ratio_sum / s.pixels.max(1) as f64).max(1e-30));
    let peak_level_dbfs = db(s.peak_power.max(1e-30)) as f32;
    let sk = (s.sk_count > 0).then_some(s.sk_sum / s.sk_count.max(1) as f64);
    let clip_count = s
        .cum_after
        .clip_samples
        .saturating_sub(s.cum_before.clip_samples);
    let clip_count = clip_count.min(u64::from(u32::MAX)) as u32;
    let prov = &seg.provenance;
    let clipped =
        s.cum_after.clip_frames > s.cum_before.clip_frames || prov.overload || clip_count > 0;
    let quantisation = prov.quantisation_limited
        || s.cum_after.quantisation_frames > s.cum_before.quantisation_frames;
    let invalid = s.cum_after.invalid_frames > s.cum_before.invalid_frames;
    let edge = edge_hit(f_lo, f_hi, g);

    let spur = spur_decision(
        f_lo,
        f_hi,
        f_center,
        xdb,
        g,
        rules,
        cfg.spur_mask.as_ref(),
        seg.total_gain_db,
    );
    let comb = spur.reason.is_none()
        && xdb <= rules.comb.max_line_width_hz
        && eval.is_some_and(|e| {
            CombFinder::is_member(&e.comb, f_center, rules.comb.tolerance_hz + df)
        });
    let dc = matches!(spur.reason, Some(SpurReason::Dc));
    let (image, mirror_missing) = image_test(comp, seg, cfg, f_center, dc);

    let flags = DetectionFlags {
        clipped,
        spur_candidate: spur.reason.is_some() || comb,
        spur_reason: spur.reason.or(comb.then_some(SpurReason::Comb)),
        image_candidate: image.is_some(),
        image_retune_confirmed: false,
        marginal: snr_peak_db < rules.marginal_snr_db
            || quantisation
            || edge
            || invalid
            || mirror_missing,
        suspect_imd: false,
        compressed: false,
        impulsive: false,
        edge,
    };
    let start = s.start.time;
    let end = s.end.time.max(start);
    let record = DetectionRecord {
        detection: Detection {
            id: DetectionId::new(),
            survey_id: cfg.survey_id,
            time: TimeRange::new(start, end),
            f_center_hz: f_center,
            obw_hz: obw,
            xdb_bandwidth_hz: Some(xdb),
            xdb_level_db: Some(cfg.xdb_level_db),
            snr_peak_db,
            snr_mean_db,
            peak_level_dbfs,
            peak_level_dbm: None,
            sk,
            clip_count,
            // Filled from the segment's interned version at emission, so boxes merged into an
            // impulsive event allocate nothing.
            detector_version: String::new(),
            provenance_ref: prov.id(),
            flags,
        },
        provenance: prov.clone(),
        segment: seg.segment,
        bins: lo..hi,
        f_lo_hz: f_lo,
        f_hi_hz: f_hi,
        frames: s.first_frame..s.last_frame + 1,
        samples: s.start.sample..s.end.sample,
        pixels: s.pixels,
        close,
        continues,
        candidate: Candidate::Unconfirmed,
        image,
        spur_harmonic_hz: spur.harmonic_hz,
        merged_boxes: s.boxes,
        inconclusive: invalid || mirror_missing,
    };
    let sums = Sums {
        pixels: s.pixels,
        ratio_sum: s.ratio_sum,
        sk_sum: s.sk_sum,
        sk_count: s.sk_count,
        cum_before: s.cum_before,
        cum_after: s.cum_after,
        run: s.impulsive_run,
    };
    (record, sums)
}

/// Rule 5 on a component's accumulators. Returns the evidence when it is an image candidate,
/// and whether the test was impossible (a cell at bin 0 has no mirror).
fn image_test(
    comp: &Component,
    seg: &SegmentInfo,
    cfg: &DetectorConfig,
    f_center: f64,
    dc: bool,
) -> (Option<ImageEvidence>, bool) {
    let g = &seg.geometry;
    let rule = &cfg.rules.image;
    let mut missing = false;
    let mut pearson = Pearson::default();
    let (mut peak_ex, mut mirror_peak_ex, mut mirror_peak_ratio) = (0.0f64, 0.0f64, 0.0f64);
    for (b, a) in comp.acc.iter() {
        if a.count == 0 {
            continue;
        }
        if b == 0 {
            missing = true;
            continue;
        }
        let n = f64::from(a.count);
        let r = f64::from(a.ratio) / n;
        let mr = f64::from(a.mirror_ratio) / n;
        peak_ex = peak_ex.max(f64::from(a.excess) / n);
        mirror_peak_ex = mirror_peak_ex.max(f64::from(a.mirror_excess) / n);
        mirror_peak_ratio = mirror_peak_ratio.max(mr);
        pearson.add(db(r.max(1e-6)), db(mr.max(1e-6)));
    }
    if dc || (f_center - g.center_hz).abs() < rule.min_offset_hz || pearson.count() == 0 {
        return (None, missing);
    }
    if mirror_peak_ratio <= seg.thresholds.t_on {
        return (None, missing);
    }
    let rejection_db = db(mirror_peak_ex.max(1e-300) / peak_ex.max(1e-300));
    if rejection_db < rule.min_rejection_db {
        return (None, missing);
    }
    let corr = if pearson.count() >= 3 {
        pearson.value()
    } else {
        None
    };
    let ok = pearson.count() <= rule.max_bins_without_shape
        || corr.is_some_and(|c| c > rule.min_shape_correlation);
    let evidence = ok.then_some(ImageEvidence {
        source_hz: 2.0 * g.center_hz - f_center,
        rejection_db,
        shape_correlation: corr,
    });
    (evidence, missing)
}
