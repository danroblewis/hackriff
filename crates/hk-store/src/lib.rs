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
//! - [`iqbuffer`]: the rolling raw-IQ capture buffer behind the Capture timeline (T-157).
//! - [`recordings`]: the catalogue of persisted SigMF IQ recordings (T-469): what is on disk,
//!   over what span and at what tuning, so the IQ horizon is the ring **plus** recordings rather
//!   than the ring alone. A query over the `Recording` rows, checked against the files.
//! - [`coverage`]: the coverage map (T-368): which front end actually sampled which
//!   time–frequency cell, so a view greys only what was **never observed** — three states, with
//!   "never looked" unrepresentable as "looked and it was quiet".

pub mod coverage; // T-368
pub mod dataset; // T-205
pub mod decoded;
pub mod history;
pub mod iqbuffer; // T-157
pub mod ml; // T-844: the durable half of C38 shadow mode (ADR-0016 §6)
pub mod outputs;
pub mod radiometry;
pub mod recordings; // T-469: the persisted IQ recordings that extend the audio horizon

// ADR-0012 §9/§11 (pre-added by T-113; the owners fill them in).
pub mod baseline; // T-119
pub mod observation; // T-115
pub mod occupancy; // T-118

pub use coverage::{
    Coverage, CoverageGrid, CoverageSpan, Device, RecordSpans, Sampled, spans_from_records,
};
pub use history::{
    CellStats, ChannelSummary, FilterSummary, FrameInput, FrameOrigin, GainState, HistogramConfig,
    IngestOutcome, LastKnown, LastKnownCell, LastKnownSearch, LastKnownStage, LevelConfig,
    OriginField, OriginFilter, Overview, OverviewCell, PendingWrites, ProvenanceSummary, Pyramid,
    PyramidConfig, PyramidStats, RegionHistory, RegionQuery, ResidentBytes, Resolution, ShadowFill,
    ShadowRun, StoreError, StraddleGuard, ViewLattice, WrittenBatch,
};
pub use radiometry::{
    FloorFlags, FloorIngest, FloorIngestQueue, FloorProduct, FloorProductConfig, FloorProductStats,
    FloorStep, FloorVsTime, IngestQueueStats, QueuedIngest,
};
pub use recordings::{
    Availability, AvailableSpan, RecordingEntry, RecordingsCatalogue, RecordingsQuery,
};
