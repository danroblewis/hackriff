//! Spectrum-history pyramid (T-017; C26; docs/07 §2.5, §3.2, §4; ADR-0006).
//!
//! Frames fold into **tiles**; tiles roll up into coarser levels; a rolling byte budget evicts
//! the finest covered tiles first; [`Pyramid::query`] answers "what has this region looked like
//! over time?".
//!
//! # Tiles and addressing
//!
//! A scheme ([`PyramidConfig::scheme`], carried in every [`hk_model::TileKey`] and tile header)
//! fixes a canonical grid per level: level-0 cells of `f_cell_hz × t_cell`, epoch-aligned, and a
//! ladder where each level widens frequency cells by `f_factor` and **uses one whole tile of the
//! level below as its time cell**. A tile is `f_cells_per_block × t_cells_per_block` cells
//! addressed `(scheme, level, f_block, t_block)`, with `f_block = ⌊f / (nf·w)⌋` and
//! `t_block = ⌊t / (nt·d)⌋`. That address is the index: queries and eviction compute the keys they
//! need and open `…/s<scheme>/L<level>/f<f_block>/t<t_block>.tile` directly; nothing scans
//! unrelated tiles. See [`PyramidConfig::default`] for the default ladder.
//!
//! # Per-cell statistics
//!
//! Frames regrid onto level 0 as described in [`frame`] (max-preserving peak, power-averaging
//! value). Each cell keeps:
//!
//! - **max** (dB) of the peak, **mean** as a linear power mean, **frames**;
//! - **low/high percentile** (default p10/p90). Level 0: exact order statistics of the cell's
//!   frame values, computed when its 1-column closes. Higher levels: from the merged histogram;
//! - **occupancy**: fraction of observed time with value > threshold, threshold = floor + margin.
//!   The floor is the caller's per-bin floor ([`FrameInput::floor_db`], e.g. T-005) or, by
//!   default, the minimum of the cell's low percentile over the last `floor_memory_tiles` level-0
//!   tiles (cold start: the 20th percentile of the column across frequency);
//! - **max-occupancy**: the highest level-0 cell occupancy folded in, so a short busy period is
//!   not averaged away by rollup;
//! - **coverage**: observed fraction of the cell's duration (gaps read as < 1, unobserved as
//!   `frames == 0`, never as quiet).
//!
//! Every tile also keeps, per frequency cell, a fixed-bin dB histogram of all frame values over the
//! **tile's whole duration** — exactly the histogram of the parent cell it becomes — plus a
//! [`ProvenanceSummary`] (gain states, suspect/clip fraction, dropped samples, calibration id; T-116:
//! gain table, filter, spur-mask version, the cell noise shape, and every front-end
//! [`ProvenanceStep`]). The grid is fixed, so tiles are not split at a front-end change: single-valued
//! tags carry a `*_mixed` flag and the step is listed with its time, before and after state, and it
//! survives rollup — different provenance never merges silently. The pyramid scheme/version id is in
//! every [`hk_model::TileKey`], tile header and query result.
//!
//! # Coverage mask and floor (T-116)
//!
//! Each tile's observed-cell bitmap is its coverage mask, and every cell keeps the observed fraction
//! of its duration; queries return `coverage` per cell and [`RegionHistory::coverage_summary`] (observed
//! cells, mean coverage, fully unobserved time runs). A gap in the input is unobserved at level 0 and
//! lowers coverage (or stays unobserved) at every coarser level. The raw low percentile of averaged
//! noise reads below the noise mean (≈ −0.5 dB for T-017's STFT geometry, −2.5 dB for 8-look noise);
//! [`CellStats::floor_db`] corrects it with the Gamma model when the frames carry a
//! [`NoiseShape`].
//!
//! # Rollup
//!
//! When a tile seals it is written and folded into its parent (`tile::Tile::fold_child`): max-of-max;
//! power mean of means weighted by frames; histograms summed (so a parent percentile is within one
//! histogram step of the pooled order statistic over every contributing frame; see
//! [`stats::hist_percentile`] for the exact bound);
//! occupancy time-weighted in time and max across frequency; max-occupancy max-of-max. Parents live
//! in memory until their block ends, then seal in turn. Because a child is evictable only once its
//! parent is sealed on disk, open parents need no checkpoint: they are rebuilt from their sealed
//! children on [`Pyramid::open`].
//!
//! # Storage format (decision)
//!
//! A compact custom binary file per tile (module `codec`, format version [`FORMAT_VERSION`]): header with scheme and geometry,
//! bitmap-sparse cell statistics (dB as i16 at 0.01 dB, fractions as u16, varint frames), varint
//! sparse histograms, CRC-32, written temp → fsync → rename. Chosen over Parquet because the access
//! pattern is "whole tile by key", not columnar scans; the arrow/parquet crates are large
//! (compile time and binary size on the Jetson) for no query benefit; and one file per key needs no
//! index service, compaction or schema tooling (one developer, low ops). Format 2 (T-116) stores the
//! statistics column-wise and compresses the payload with zstd ([`PyramidConfig::compression_level`],
//! default level 3; raw when zstd does not shrink it); format-1 tiles remain readable, so existing
//! history needs no migration. [`PyramidStats::raw_bytes_written`] / `bytes_written` is the
//! measured compression ratio.
//!
//! # hackrf_sweep CSV and PNG (T-116)
//!
//! [`import_sweep_csv`] folds the user's `hackrf_sweep` CSV into history (`hk history
//! import-sweep-csv`); [`write_sweep_csv`] and [`waterfall_png`] export a query result (module
//! `export`).
//!
//! # Floor for sweep and CSV frames (T-126)
//!
//! Sweep rows rarely state how many FFTs were averaged into a bin, so their shape is estimated
//! from the rows: a noise bin's dB variance over time is `(10/ln 10)²·ψ′(k)` whatever the floor
//! level, so [`NoiseShapeEstimator`] pools the variance of floor-level, steady bins and inverts it
//! (method and measured accuracy in [`shape`]). Frames carry it as [`NoiseShape::BinShape`]; the
//! CSV importer estimates it from the first sweeps. Measured on synthetic 1- and 4-look sweeps:
//! `k` within 5 %, `floor_db` of hour cells within 0.5 dB of the injected floor.
//!
//! # Provenance step state (T-126)
//!
//! Steps compare a frame with the last frame **of its source** ([`FrameInput::source`]), and that
//! state (with its cell shape) is persisted per source in `front_end.state` at every checkpoint
//! and seal, so a restart neither hides a real change nor invents one, and sources sharing a store
//! do not show alternating false steps.
//!
//! # Source and site (T-133)
//!
//! Every tile records the frames it folded per **origin**: the [`FrameInput::source`] key and the
//! site the device was at ([`FrameInput::site`]: a site id, `unassigned` or `mobile`; ADR-0012
//! §3.5), at most [`MAX_ORIGINS`] per tile ([`ProvenanceSummary::origins`], format 3). Tiles are
//! not split by origin. [`Pyramid::query_filtered`] answers for one source and/or site: cells
//! other origins' frames were folded into read as unobserved, except that a coarse cell of a mixed
//! tile is kept when the one finer tile it rolls up passes whole. Tiles written before format 3
//! (and frames without a site) are of **unknown** origin: only an unfiltered query or an explicit
//! [`OriginField::Unknown`] filter matches them.
//!
//! # Retention and crash safety
//!
//! After every seal retention runs over sealed tiles (T-116): per-level `max_age`, per-level
//! `byte_quota`, per-region [`RetentionOverride`]s ("keep 433 MHz at level 0 for 90 days"), then the
//! global [`PyramidConfig::byte_budget`]: evict the oldest sealed unprotected tile of the finest
//! level whose parent is sealed on disk; only when no level below the top has one, expire the
//! oldest top-level tile; protected tiles last. A tile is never evicted while a finer tile inside it
//! remains (children first) or before a coarser level covers it. Overrides are cell-precise
//! (T-126): a protected tile's unrelated cells are trimmed once past their own age. Age work runs
//! from a deadline index and quota/budget from an unprotected-tile index, so a retention pass does
//! not scan the protected tiles it keeps. The clock is the data watermark (see
//! `Pyramid::enforce_budget`). Level-0 open tiles are checkpointed every
//! `checkpoint_interval`. On open, temp files and files failing the
//! length/CRC checks are ignored and removed.

mod codec;
mod config;
mod export;
pub mod frame;
mod query;
pub mod shape;
pub mod stats;
mod store;
mod tile;

#[cfg(test)]
mod tests;

use std::path::PathBuf;

pub use codec::FORMAT_VERSION;
pub use config::{
    Geometry, HistogramConfig, LevelConfig, LevelGeometry, MAX_LEVELS, PyramidConfig,
    RetentionOverride,
};
pub use export::{
    HistoryStat, PNG_UNOBSERVED_RGB, SweepCsvImport, SweepCsvOptions, import_sweep_csv,
    waterfall_index, waterfall_png, waterfall_range, write_sweep_csv,
};
pub use frame::{
    DbScratch, FrameInput, FrameOrigin, FrontEnd, GainState, NoiseShape, PortTag, source_key,
};
pub use query::{
    CellStats, ChannelSummary, CoverageSummary, FULL_CELL_OCCUPANCY, FilterSummary,
    MAX_COVERAGE_GAPS, MAX_QUERY_CELLS, RegionHistory, RegionQuery, Resolution, burst_histogram,
};
pub use shape::NoiseShapeEstimator;
pub use store::{IngestOutcome, Pyramid, PyramidStats};
pub use tile::{
    FrontEndState, MAX_GAIN_STATES, MAX_ORIGINS, MAX_PROVENANCE_STEPS, Origin, OriginField,
    OriginFilter, OriginMatch, ProvenanceStep, ProvenanceSummary, SHAPE_TOLERANCE,
};

/// Errors from the history store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Invalid configuration.
    #[error("invalid pyramid config: {0}")]
    Config(String),
    /// Filesystem error.
    #[error("{path}: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A tile of this scheme id was written with different geometry: change the scheme id.
    #[error("{path}: tile does not match the scheme config: {detail}")]
    SchemeMismatch {
        /// Offending file.
        path: PathBuf,
        /// What differs.
        detail: String,
    },
    /// The frame is malformed or in the wrong unit.
    #[error("frame rejected: {0}")]
    BadFrame(&'static str),
    /// The query is malformed or too large.
    #[error("query rejected: {0}")]
    BadQuery(String),
}
