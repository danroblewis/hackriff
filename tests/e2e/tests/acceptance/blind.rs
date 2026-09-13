//! Blind replay harness (T-039). This is the **one place** acceptance tests start a blind
//! replay. Today it feeds a truth-stripped SigMF copy to the pipeline through `common.rs`'s
//! replay helpers. T-047 switches it to the mock SDR device (T-049) here, without touching test
//! bodies.
//!
//! Test shape:
//! 1. [`private_truth`] loads the fixture's truth, which only the test holds.
//! 2. [`blind_replay`] strips the truth (`hk_e2e::blind::strip_truth`), optionally relabels or
//!    IQ-shifts the recording, runs the pipeline, and serves `/api/inventory`. A test that taps
//!    streams or sets a plan extra starts from [`blind_config`] instead (T-037b), which does the
//!    same stripping and returns the configuration before the run starts.
//! 3. The test matches everything produced against its private truth
//!    (`hk_e2e::blind::matching`), never looking a frequency up in the database.

use std::path::{Path, PathBuf};

use hk_e2e::Fixture;
use hk_e2e::blind::{shift_ci8, strip_truth};
use hk_model::sigmf::SigmfMeta;
use hk_pipeline::{PipelineConfig, Replay};
use serde_json::json;

use crate::common::*;

/// How the recording is presented to the system.
#[derive(Clone, Copy, Debug, Default)]
pub struct BlindSource {
    /// RF relabel, Hz: identical samples at a different tuned centre.
    pub relabel_hz: f64,
    /// IQ frequency shift, Hz: every emission moves at an unchanged tuned centre (synthesised).
    pub iq_shift_hz: f64,
}

/// A finished blind run.
pub struct BlindRun {
    /// Data directory (SQLite, tiles).
    pub dir: TempDir,
    /// Run summary.
    pub summary: hk_pipeline::RunSummary,
    /// `/api/inventory` rows.
    pub api_rows: Vec<serde_json::Value>,
    _src: TempDir,
}

/// A blind run's configuration before it starts.
pub struct BlindConfig {
    /// Data directory (SQLite, tiles).
    pub dir: TempDir,
    /// `hk replay`'s configuration over the truth-stripped copy.
    pub cfg: PipelineConfig,
    /// The opened truth-stripped replay.
    pub replay: Replay,
    /// The stripped copy; keep it until the run has finished.
    pub src: TempDir,
}

/// The fixture's meta path and the truth only the test sees; `None` skips.
pub fn private_truth(name: &str) -> Option<(PathBuf, Fixture)> {
    let meta = real_fixture(name)?;
    let fixture = Fixture::load(&meta).unwrap();
    Some((meta, fixture))
}

/// Strips `meta`'s truth and configures an unpaced replay of it with `extra` as the plan's
/// `extra`.
pub fn blind_config(
    meta: &Path,
    tag: &str,
    source: BlindSource,
    extra: serde_json::Value,
) -> BlindConfig {
    let src = TempDir::new(&format!("{tag}src"));
    if source.iq_shift_hz != 0.0 {
        let fs = SigmfMeta::read(meta).unwrap().global.sample_rate.unwrap();
        shift_ci8(
            &meta.with_extension("sigmf-data"),
            &src.0.join("blind.sigmf-data"),
            source.iq_shift_hz,
            fs,
        )
        .unwrap();
    }
    let blind = strip_truth(meta, &src.0, "blind", source.relabel_hz).unwrap();
    let dir = TempDir::new(tag);
    let (cfg, replay) = replay_config(&dir.0, &blind, extra, hk_core::Pacing::Unpaced);
    BlindConfig {
        dir,
        cfg,
        replay,
        src,
    }
}

/// Replays `meta` with its truth stripped.
pub fn blind_replay(meta: &Path, tag: &str, source: BlindSource) -> BlindRun {
    let BlindConfig {
        dir,
        cfg,
        replay,
        src,
    } = blind_config(meta, tag, source, json!({}));
    let handle = start(cfg, replay);
    let counters = handle.counters();
    let summary = finish(handle);
    let server = serve_api(&dir.0, counters);
    let (_, api_rows) = api_inventory(server.local_addr());
    BlindRun {
        dir,
        summary,
        api_rows,
        _src: src,
    }
}
