//! Blind replay harness (T-039). This is the **one place** acceptance tests start a blind
//! replay. Today it feeds a truth-stripped SigMF copy to the pipeline through `common.rs`'s
//! replay helpers. T-047 switches it to the mock SDR device (T-049) here, without touching test
//! bodies.
//!
//! Test shape:
//! 1. [`private_truth`] loads the fixture's truth, which only the test holds.
//! 2. [`blind_replay`] strips the truth (`hk_e2e::blind::strip_truth`), optionally relabels or
//!    IQ-shifts the recording, runs the pipeline, and serves `/api/inventory`.
//! 3. The test matches everything produced against its private truth
//!    (`hk_e2e::blind::matching`), never looking a frequency up in the database.

use std::path::{Path, PathBuf};

use hk_e2e::Fixture;
use hk_e2e::blind::{shift_ci8, strip_truth};
use hk_model::sigmf::SigmfMeta;
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

/// The fixture's meta path and the truth only the test sees; `None` skips.
pub fn private_truth(name: &str) -> Option<(PathBuf, Fixture)> {
    let meta = real_fixture(name)?;
    let fixture = Fixture::load(&meta).unwrap();
    Some((meta, fixture))
}

/// Replays `meta` with its truth stripped.
pub fn blind_replay(meta: &Path, tag: &str, source: BlindSource) -> BlindRun {
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
    let (cfg, replay) = replay_config(
        &dir.0,
        &blind,
        plan_extra(&source),
        hk_core::Pacing::Unpaced,
    );
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
