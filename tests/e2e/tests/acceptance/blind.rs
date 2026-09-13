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
    /// A content class the user vouches for on the recording (`hackriff:content_class`), e.g.
    /// `unrestricted` so content chains may run outside the frequency-derived band priors. This is
    /// user configuration, not truth.
    pub vouched_class: Option<&'static str>,
    /// An extra RF range, Hz, the user adds to the built-in `wfm-rds` chain
    /// (`ScanPlan.extra.pipeline.chains`), so the analog chain may demodulate there. User
    /// configuration, not truth: mode selection still decides whether it is WFM.
    pub demod_freq_hz: Option<[f64; 2]>,
}

/// `ScanPlan.extra` for `source`.
fn plan_extra(source: &BlindSource) -> serde_json::Value {
    let Some(range) = source.demod_freq_hz else {
        return json!({});
    };
    let mut chains = hk_pipeline::builtin_chains();
    for c in chains.iter_mut().filter(|c| c.id == "wfm-rds") {
        c.freq_hz.push(range);
    }
    json!({ "pipeline": { "chains": chains } })
}

/// Deep-merges `extra` into `base`: objects merge key by key, anything else replaces.
fn merge_json(base: &mut serde_json::Value, extra: serde_json::Value) {
    match (base, extra) {
        (serde_json::Value::Object(b), serde_json::Value::Object(e)) => {
            for (k, v) in e {
                merge_json(b.entry(k).or_insert(serde_json::Value::Null), v);
            }
        }
        (b, e) => *b = e,
    }
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
    if let Some(class) = source.vouched_class {
        let mut m = SigmfMeta::read(&blind).unwrap();
        m.global
            .extra
            .insert("hackriff:content_class".into(), json!(class));
        m.write(&blind).unwrap();
    }
    let dir = TempDir::new(tag);
    let mut plan = plan_extra(&source);
    merge_json(&mut plan, extra);
    let (cfg, replay) = replay_config(&dir.0, &blind, plan, hk_core::Pacing::Unpaced);
    BlindConfig {
        dir,
        cfg,
        replay,
        src,
    }
}

/// A blind run that keeps replaying its truth-stripped copy (as `hk serve --replay --loop` does)
/// until stopped, for tests that act on the running system (Listen, T-043). T-047 moves it to
/// the mock device with the other entry points.
pub struct BlindLive {
    /// Data directory (SQLite, tiles).
    pub dir: TempDir,
    /// The running pipeline; stop it with `handle.stop()` then [`finish`].
    pub handle: hk_pipeline::PipelineHandle,
    /// The stripped copy (kept until the run ends).
    _src: TempDir,
}

/// Starts a looping blind run of `meta`.
pub fn blind_live(meta: &Path, tag: &str, source: BlindSource) -> BlindLive {
    let BlindConfig {
        dir,
        cfg,
        replay,
        src,
    } = blind_config(meta, tag, source, json!({}));
    let copy = std::fs::read_dir(&src.0)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.to_string_lossy().ends_with(".sigmf-meta"))
        .expect("stripped copy");
    let reopen: hk_pipeline::SourceFactory = Box::new(move || {
        hk_pipeline::open_replay(&copy, hk_core::Pacing::Unpaced, false)
            .map(|r| Box::new(r.source) as Box<dyn hk_core::Source>)
    });
    let handle = hk_pipeline::Pipeline::start(
        cfg,
        Box::new(replay.source),
        replay.info,
        Some(reopen),
        Box::new(hk_pipeline::TrackInventory::default()),
    )
    .unwrap();
    BlindLive {
        dir,
        handle,
        _src: src,
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
