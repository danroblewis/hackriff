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
//! [`ProvenanceSummary`] (gain states, suspect fraction, dropped samples, calibration id).
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
//! bitmap-sparse 15–19 B cell records (dB as i16 at 0.01 dB, fractions as u16, varint frames), varint
//! sparse histograms, CRC-32, written temp → fsync → rename. Chosen over Parquet because the access
//! pattern is "whole tile by key", not columnar scans; the arrow/parquet crates are large
//! (compile time and binary size on the Jetson) for no query benefit; and one file per key needs no
//! index service, compaction or schema tooling (one developer, low ops). No compression codec yet
//! (the header reserves the choice via the format version); the sparse encoding already skips
//! unobserved cells and empty histogram bins.
//!
//! # Budget and crash safety
//!
//! After every seal the budget is enforced ([`PyramidConfig::byte_budget`], optional per-level
//! `max_age`): evict the oldest sealed tile of the finest level whose parent is sealed on disk;
//! only when no level below the top has one, expire the oldest top-level tile. Level-0 open tiles
//! are checkpointed every `checkpoint_interval`. On open, temp files and files failing the
//! length/CRC checks are ignored and removed.

mod codec;
mod config;
pub mod frame;
mod query;
pub mod stats;
mod store;
mod tile;

#[cfg(test)]
mod tests;

use std::path::PathBuf;

pub use codec::FORMAT_VERSION;
pub use config::{
    Geometry, HistogramConfig, LevelConfig, LevelGeometry, MAX_LEVELS, PyramidConfig,
};
pub use frame::{DbScratch, FrameInput, GainState};
pub use query::{
    CellStats, ChannelSummary, FULL_CELL_OCCUPANCY, MAX_QUERY_CELLS, RegionHistory, RegionQuery,
    Resolution, burst_histogram,
};
pub use store::{IngestOutcome, Pyramid, PyramidStats};
pub use tile::{MAX_GAIN_STATES, ProvenanceSummary};

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
