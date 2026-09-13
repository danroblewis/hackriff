//! hackriff detection (C09, T-006): OS-CFAR plus floor-branch detection over spectrum frames,
//! hysteresis and minimum duration, time–frequency components, spur/ghost/image/clip flags, and
//! immutable docs/07 §2.9 [`Detection`](hk_model::Detection) records. Design input: spike S4
//! (REPORT §3.2–3.5, §5). Burst tracking (C10) is T-007 and consumes the event stream.
//!
//! # Pipeline (one [`Detector::process`] call per frame)
//!
//! 1. **Classify** every cell ([`cfar`]): `seed = (P > guard·F ∧ P > α_on·Z) ∨ P > T_on·F`,
//!    `region = (P > guard·F ∧ P > α_off·Z) ∨ P > T_off·F ∨ seed`. OS-CFAR across frequency
//!    (N 32 = 16/side, G 4/side, k 24); `α` numeric for `Gamma(n)` order statistics ([`alpha`]);
//!    `T = Q⁻¹(n, pfa)/n`; 3 dB guard on the OS branch only. The off thresholds come from their
//!    own Pfa (1e-3), never a fixed −3 dB. `F` is the floor reference
//!    ([`FloorReference`]: per-frame FCME by default; the wide-signal reference once T-005's fix
//!    lands). `n` is [`FloorFrame::n_avg_effective`](hk_dsp::floor::FloorFrame).
//! 2. **Components** ([`components`]): 4-connected time–frequency components of the raw region
//!    that contain a seed and span ≥ 3 frames are kept; kept components then merge across ≤ 2-frame
//!    gaps. No frequency merge. Streamed, so a box is emitted `gap + 1` frames after it ends.
//! 3. **Impulsive frames** ([`FloorFrame::impulsive`](hk_dsp::floor::FloorFrame)): boxes with most
//!    cells in impulsive frames merge per impulsive run into one broadband `impulsive` Detection.
//! 4. **Flags** ([`rules`], [`comb`]): ref-harmonic (n × 10 MHz, ≤ 25 kHz, max(10 kHz, 25 ppm)),
//!    DC (≤ 40 kHz within 15 kHz of fc), spur map, comb (judged on the integrated spectrum), IQ
//!    image (≥ 20 dB stronger mirror, shape correlation > 0.5), clip (frame clip fraction > 1e-4;
//!    overloaded provenance), edge zone, and `marginal` (peak SNR < 10 dB, quantisation-limited,
//!    edge, or an inconclusive test). Gain-step and retune (rules 6–7) are pure functions in
//!    [`trust`].
//! 5. **Emitter candidates** (S4 "emitter confirmation"): a detection is confirmed by a repeat at
//!    a consistent frequency or by the ≥ 1 s integrated spectrum ([`integrated`]). The status is
//!    on the record ([`Candidate`]) and later confirmations are [`DetectorEvent::Confirmed`]
//!    events; nothing is filtered.
//! 6. **Records** ([`record`]): t/f box, OBW (99 %) and x-dB bandwidth from the burst-gated
//!    spectrum, peak/mean SNR, SK, peak level (dBFS), clip count, provenance, and a
//!    `detector_version` with a settings hash. [`DetectionWriter`] batches repository writes.
//!
//! # Transitions
//!
//! A new floor segment, a provenance change (gain, retune), a frame discontinuity in
//! [`DetectorConfig::reset_on`], or a resolution change closes every open box at its last frame
//! (no merge across the change) and restarts thresholds, components and integration.
//!
//! # Real-time path
//!
//! [`Detector::process`] allocates nothing in steady state except the emitted detections (the
//! `detector_version` string); tested under a counting allocator. Cost at 4096 bins:
//! `benches/detect_throughput.rs`.

pub mod alpha;
pub mod cfar;
pub mod clip;
pub mod comb;
pub mod components;
pub mod config;
pub mod detector;
pub mod integrated;
pub mod record;
pub mod rules;
pub mod trust;
pub mod writer;

pub use cfar::{CELL_NONE, CELL_REGION, CELL_SEED, CfarEngine, ClassifyStats, Thresholds};
pub use clip::{ClipCount, count_clipped_ci8};
pub use comb::{Comb, CombFinder};
pub use config::{
    BandProfile, Branches, CfarWindow, CombRule, ConfigError, ConfirmConfig, DETECT_RESET_ON,
    DcRule, DetectionProfile, DetectorConfig, EdgeRule, FloorReference, Hysteresis, ImageRule,
    IntegrationConfig, RefHarmonicRule, Rules,
};
pub use detector::{Detector, DetectorStats, SegmentInfo};
pub use integrated::{IntegratedEmitter, IntegratedEvaluation, IntegratedSnapshot, SpanMeasure};
pub use record::{
    Candidate, CloseReason, ConfirmReason, Confirmation, DetectionRecord, DetectorEvent,
    ImageEvidence,
};
pub use rules::Geometry;
pub use trust::{
    CaptureEmitter, CaptureResult, CaptureSide, GainState, GainStepConfig, GainStepResult,
    GainStepRow, GainStepSkip, GainStepVerdict, RetuneConfig, RetuneLabel, RetuneResult, RetuneRow,
    gain_step, retune,
};
pub use writer::DetectionWriter;
