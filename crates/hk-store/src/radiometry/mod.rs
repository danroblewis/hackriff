//! The calibrated noise-floor product (T-021; C33 radiometry, C08 floor; SPACE-050; docs/07
//! §5.1): calibrated spectra folded into T-017 tiles, and a floor-vs-time query over them.
//!
//! # Design
//!
//! - **Calibrate before folding.** Each frame is calibrated with the version its provenance pins
//!   ([`hk_dsp::radiometry::PowerCalibrations`]), per bin (`PSD · 10^(K(f)/10)`), and folded into
//!   a **dBm/Hz pyramid**. Frames with no applicable calibration go, unchanged, into a separate
//!   **dBFS/Hz pyramid**: the two never mix and dBm is never faked. Calibrating at query time
//!   instead would be wrong for cells that mix gain states (the tile digest is per tile, not per
//!   cell, and a p10 over mixed dBFS values is meaningless).
//! - **Both pyramids go through the existing ingest path** ([`crate::history::Pyramid::ingest`]),
//!   so the tile digest keeps the calibration id (from the frame's provenance) and gain states.
//! - **State log** (`runs.tsv`, module `runs`): tiles keep no per-frame flags, so the
//!   product logs frame flags, gain setting and calibration per level-0 time cell, run-length
//!   encoded. Flags come from the T-005 [`FloorFrame`](hk_dsp::floor::FloorFrame) of the same
//!   frame; gain-segment boundaries come from its segment reset (GainKey), not from episodes.
//! - **Cell shape** `n_c` ([`hk_dsp::radiometry::cell_value_shape`]) is fixed per product by
//!   the first frame's spectral geometry and persisted in `product.txt`; frames of another
//!   geometry are rejected (use a separate product per STFT configuration).
//!
//! # Estimator: bias-corrected p10, not the power mean
//!
//! The value of a step is the **median over the region's cells of the bias-corrected low
//! percentile** (p10). The power mean is kept only as a cross-check (`mean_db_per_hz`): it
//! includes every burst and carrier in the cell, so it reads high whenever the cell is occupied
//! at all; the p10 of a cell's frames ignores activity present in fewer than ~90 % of them, and
//! the median across cells ignores carriers occupying a minority of cells. The p10 of averaged
//! noise reads below the floor (T-017: ~0.5 dB); the correction is derived from the Gamma model
//! of a cell value (`hk_dsp::radiometry::bias`):
//!
//! `floor_dB = p10_dB − 10·log10(P⁻¹(n_c, p)/n_c)`, `p = (1 + 0.1·(n − 1))/(n + 1)` for level-0
//! cells of `n` frames (exact order statistic), `p = 0.1` for rolled-up cells (histogram).
//!
//! # Uncertainty (standard, combined in quadrature)
//!
//! `σ = √(σ_model² + σ_cal² + σ_hist² + σ_stat²)`:
//! - `σ_model` = [`FloorProductConfig::model_uncertainty_db`] (0.5 dB, as T-005);
//! - `σ_cal` = the largest point uncertainty of the calibrations applied in the step (0 when
//!   uncalibrated; per-bin application removes `BandCal`'s band-spread term);
//! - `σ_hist` = `step/√12` when any cell came from a rolled-up histogram (uniform in-bin error);
//! - `σ_stat` = `√(π/2)·1.4826·MAD/√N` over the `N` cells' corrected values (the standard error
//!   of a median, with a robust spread), or `4.343/√n_c` for a single cell.
//!
//! # Mixed-gain cells
//!
//! A cell whose frames span a gain change pools dBm values from both states. With correct
//! calibration they share one distribution and the corrected p10 is the floor (no step, tested).
//! If the two calibrations disagree by `δ`, the pooled p10 lies between the two readings, closer
//! to the lower one. Such steps carry [`FloorFlags::MIXED_GAIN`] (and `SEGMENT_START` from the
//! tracker reset); `σ_cal` is the larger of the two. A quantisation-limited low-gain state reads
//! higher in dBm, so the pooled p10 favours the high-gain frames (the safe direction).
//!
//! # Flags and gaps
//!
//! Steps OR the state flags of the runs in the step whose calibration status matches the value
//! (`QUANTISATION_LIMITED`, `IMPULSIVE_PAUSED`, `GATE_RELEASED`, `SEGMENT_START`, `NOT_READY`,
//! `INVALID`, `EPISODE`, `CAL_EDGE_HELD`), plus `MIXED_GAIN`, `MIXED_CALIBRATION`,
//! `UNCALIBRATED` (value in dBFS/Hz) or `PARTLY_UNCALIBRATED` (calibrated value; uncalibrated
//! frames also observed and excluded). A step nothing observed is a **gap**: no value, NaN
//! uncertainty, [`FloorFlags::GAP`]; neighbours are never interpolated across it. The slow-floor
//! flags describe the tracker, not the tile value: the p10 product does not use the slow floor,
//! so T-005 slow-floor biases under bursts do not reach it.
//!
//! # Limits
//!
//! Temperature dependence of `K` and antenna/accessory keys are not modelled (C05 must issue a new
//! version). The state log is not pruned with the pyramid budget. The region should exclude the
//! baseband roll-off at the span edges.

mod ingest_queue;
mod product;
pub mod runs;

pub use ingest_queue::{FloorIngestQueue, IngestQueueStats, QueuedIngest};

pub use hk_dsp::radiometry::FloorFlags;
pub use product::{
    FloorIngest, FloorProduct, FloorProductConfig, FloorProductStats, FloorStep, FloorVsTime,
};
pub use runs::FloorRun;
