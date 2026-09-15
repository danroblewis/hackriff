//! hackriff persistence outside the relational store. Covers the multi-resolution spectrum-history
//! pyramid of SpectrumTiles that answers "what has this region looked like over time" (C26),
//! SigMF recording with pre-trigger IQ and embedded annotations (C25), and the disk quota and
//! retention policy (ADR-0006).
//!
//! - [`history`]: the spectrum-history pyramid (T-017): frames fold into tiles, tiles roll up into
//!   coarser levels as they age, a rolling byte budget bounds the disk, and
//!   [`Pyramid::query`](history::Pyramid::query) answers the region-over-time question
//!   (docs/07 §4).
//! - [`radiometry`]: the calibrated noise-floor product (T-021, SPACE-050): calibrated frames
//!   folded into tiles and a floor-vs-time query with uncertainty and flags.
//! - [`outputs`]: output files (T-061): WAV writer, SigMF-style JSON sidecars, output disk usage.

pub mod decoded;
pub mod history;
pub mod outputs;
pub mod radiometry;

// ADR-0012 §9/§11 (pre-added by T-113; the owners fill them in).
pub mod baseline; // T-119
pub mod observation; // T-115
pub mod occupancy; // T-118

pub use history::{
    CellStats, ChannelSummary, FrameInput, GainState, HistogramConfig, IngestOutcome, LevelConfig,
    ProvenanceSummary, Pyramid, PyramidConfig, PyramidStats, RegionHistory, RegionQuery,
    Resolution, StoreError,
};
pub use radiometry::{
    FloorFlags, FloorIngest, FloorIngestQueue, FloorProduct, FloorProductConfig, FloorProductStats,
    FloorStep, FloorVsTime, IngestQueueStats, QueuedIngest,
};
