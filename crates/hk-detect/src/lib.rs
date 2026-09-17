//! hackriff detection (C09, T-006): OS-CFAR plus floor-branch detection over spectrum frames,
//! hysteresis and minimum duration, time–frequency components, spur/ghost/image/clip flags, and
//! immutable docs/07 §2.9 [`Detection`](hk_model::Detection) records. Design input: spike S4
//! (REPORT §3.2–3.5, §5). Burst tracking (C10, T-007) consumes the event stream in [`track`].
//!
//! # Pipeline (one [`Detector::process`] call per frame)
//!
//! 1. **Classify** every cell ([`cfar`]): `seed = (P > guard·F ∧ P > α_on·Z) ∨ P > T_on·F`,
//!    `region = (P > guard·F ∧ P > α_off·Z) ∨ P > T_off·F ∨ seed`. OS-CFAR across frequency
//!    (N 32 = 16/side, G 4/side, k 24); `α` numeric for `Gamma(n)` order statistics ([`alpha`]);
//!    `T = Q⁻¹(n, pfa)/n`; 3 dB guard on the OS branch only. The off thresholds come from their
//!    own Pfa (1e-3), never a fixed −3 dB. `F` is the floor reference
//!    ([`FloorReference`]: the wide-signal reference by default since T-033; per-frame FCME
//!    optionally). `n` is [`FloorFrame::n_avg_effective`](hk_dsp::floor::FloorFrame). Branches are
//!    per profile ([`DetectionProfile::branches`]), so a band can run OS-only.
//! 2. **Floor-step guard** ([`step`]): within a block of a persistent > 5 dB block-floor jump that
//!    bounds no signal-like plateau (a notch, filter edge, filter-bank passband or staircase, where
//!    block FCME is biased; plateaus are told apart by their power statistics over time) or a
//!    configured response edge, the floor branch leaves the configured reference: it runs on the
//!    shape-normalised wide reference where the learned shape explains the step, and is off (OS
//!    branch alone, guarded against the local upper block floor) otherwise. On the wide
//!    reference, floor features it reads as signals use the per-frame floor. The integrated
//!    spectrum averages the reference the floor branch used and includes the bins where it ran.
//!    **Narrow floor features** (T-316) are the sub-block case: a span of raised noise wider than
//!    the OS guard band and narrower than its reference span is estimated by neither the block
//!    floor nor the OS reference cells, so a floor-like one takes its own per-bin running mean as
//!    the floor and runs the floor branch alone there.
//! 3. **Components** ([`components`]): 4-connected time–frequency components of the raw region
//!    that contain a seed and span ≥ 3 frames are kept; kept components then merge across ≤ 2-frame
//!    gaps. No frequency merge. Streamed, so a box is emitted `gap + 1` frames after it ends.
//!    Connectivity is bounded: a split at `max_duration_s` cuts it, impulsive frames never merge
//!    components, and dense frames (> `max_runs_per_frame` runs) and the live-component cap bound
//!    memory and time.
//! 4. **Impulsive frames** ([`FloorFrame::impulsive`](hk_dsp::floor::FloorFrame)): boxes with most
//!    cells in impulsive frames merge per impulsive run into one broadband `impulsive` Detection.
//! 5. **Flags** ([`rules`], [`comb`]): ref-harmonic (n × 10 MHz, ≤ 25 kHz, max(10 kHz, 25 ppm)),
//!    DC (≤ 40 kHz within 15 kHz of fc), clock-harmonic (n × fs within max(2 bins, 2 kHz); flags
//!    only), spur map, comb (judged on the integrated spectrum), IQ
//!    image (≥ 20 dB stronger mirror, shape correlation > 0.5), clip (frame clip fraction > 1e-4;
//!    overloaded provenance), edge zone, and `marginal` (peak SNR < 10 dB, quantisation-limited,
//!    edge, or an inconclusive test). Gain-step and retune (rules 6–7) are pure functions in
//!    [`trust`].
//! 6. **Emitter candidates** (S4 "emitter confirmation"): a detection is confirmed by a repeat at
//!    a consistent frequency or by the ≥ 1 s integrated spectrum ([`integrated`]). The status is
//!    on the record ([`Candidate`]) and later confirmations are [`DetectorEvent::Confirmed`]
//!    events; nothing is filtered.
//! 7. **Records** ([`record`]): t/f box, OBW (99 %) and x-dB bandwidth from the burst-gated
//!    spectrum, peak/mean SNR, SK, peak level (dBFS), clip count, provenance, and a
//!    `detector_version` (settings hash, profile, n_eff, FFT size/overlap/window and the floor
//!    tracker tag, interned per segment configuration). [`DetectionWriter`] batches repository
//!    writes.
//!
//! # Transitions
//!
//! A new floor segment, a provenance change (gain, retune), a frame discontinuity in
//! [`DetectorConfig::reset_on`], or a resolution change closes every open box at its last frame
//! (no merge across the change) and restarts thresholds, components and integration.
//!
//! **By design**, a component that has a seed but has not yet reached `min_frames` when a
//! transition arrives (a seed only in the last 1–2 frames before a gain change) is dropped, not
//! emitted: it never passed the duration test, and the frames after the change belong to a
//! different provenance.
//!
//! # Real-time path
//!
//! [`Detector::process`] allocates nothing in steady state except the emitted detections (the
//! `detector_version` string); tested under a counting allocator. Cost at 4096 bins:
//! `benches/detect_throughput.rs`.

pub mod alpha;
pub mod burst;
pub mod cfar;
pub mod clip;
pub mod comb;
pub mod components;
pub mod config;
pub mod detector;
pub mod integrated;
pub mod record;
pub mod rules;
pub mod step;
pub mod track;
pub mod trunk;
pub mod trust;
pub mod writer;

pub use burst::{BURST_DETECTOR, BurstConfig, BurstDetector, BurstStats};
pub use cfar::{
    BranchMasks, CELL_NONE, CELL_REGION, CELL_SEED, CfarEngine, ClassifyStats, Thresholds,
};
pub use clip::{ClipCount, count_clipped_ci8};
pub use comb::{Comb, CombFinder};
pub use components::FrameOutcome;
pub use config::{
    BandProfile, Branches, CfarWindow, ClockHarmonicRule, CombRule, ConfigError, ConfirmConfig,
    DETECT_RESET_ON, DcRule, DetectionProfile, DetectorConfig, EdgeRule, FloorReference,
    Hysteresis, ImageRule, IntegrationConfig, RefHarmonicRule, Rules, RunContext,
};
pub use detector::{Detector, DetectorStats, SegmentInfo};
pub use integrated::{IntegratedEmitter, IntegratedEvaluation, IntegratedSnapshot, SpanMeasure};
pub use record::{
    Candidate, CloseReason, ConfirmReason, Confirmation, DetectionRecord, DetectorEvent,
    ImageEvidence,
};
pub use rules::Geometry;
pub use step::{GuardFrame, ShapeView, StepGuard, StepGuardConfig, WideView};
pub use track::{
    BoundaryKind, CloseCause, LiveExtent, TrackBatch, TrackEvent, TrackSummary, Tracker,
    TrackerConfig,
};
pub use trust::{
    CaptureEmitter, CaptureResult, CaptureSide, GainState, GainStepConfig, GainStepResult,
    GainStepRow, GainStepSkip, GainStepVerdict, RateChangeConfig, RateChangeLabel,
    RateChangeResult, RateChangeRow, RateChangeSkip, RetuneConfig, RetuneLabel, RetuneResult,
    RetuneRow, gain_step, rate_change, retune,
};
pub use writer::DetectionWriter;
