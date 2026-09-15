//! Blind device harness (T-039, T-047). This is the **one place** acceptance tests configure and
//! start a run. Every run is driven **through the SDR device interface** (docs/10 §1.1): the
//! recording is served by the mock SDR (T-049, `hk_pipeline::open_mock_replay`), never fed to the
//! pipeline as a file.
//!
//! Test shape:
//! 1. [`private_truth`] (or the test's synthetic fixture) holds the truth; only the test reads it.
//! 2. [`blind_copy`] seals a copy of the fixture's truth in a private directory
//!    ([`hk_e2e::blind::TruthVault`]) and writes the truth-stripped copy the device serves
//!    (`strip_truth`: no annotations, no description), optionally relabelled or IQ-shifted.
//!    [`hk_e2e::blind::assert_truth_free`] checks the copy.
//! 3. [`blind_replay`] / [`blind_config`] / [`replay_config`] / [`blind_live`] open the mock device
//!    over the copy (`MockEnd::Stop`, or `Loop` for live runs), set `device_id` and `source_class`
//!    as for a radio, and optionally script device controls ([`DeviceStep`]: real `tune()` and
//!    `set_gain()` calls on the device). A recording with several centres (a survey) is served by
//!    one mock per centre in sequence ([`SurveyDevice`]).
//! 4. [`finish`] on the returned [`BlindHandle`] checks truth isolation after the run: the device
//!    served only stripped copies, the sealed truth file was never opened, and no output holds the
//!    truth's description or paths.
//! 5. The test matches everything produced against its private truth ([`assert_truth_found`]:
//!    detected within frequency/extent/time tolerance, a reasonable explanation in the top-k of
//!    `/api/inventory`), never looking a frequency up in the database.
//!
//! `HK_DEVICE=hackrf` (T-053) selects the real radio: the fixture-based tests skip with a message
//! (`common::hardware_skip`); a live survey supplies HIL truth later.

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use hk_api::{ApiState, Server, ServerConfig, Token};
use hk_core::{
    BlockHeader, DeviceInfo, Discontinuity, Gains, MockEnd, MockSdrControl, MockSdrSource, Pacing,
    Source, SourceCapabilities, SourceControl, SourceError, SourceStats,
};
use hk_e2e::blind::{
    TruthVault, assert_truth_free, matches_truth, shift_ci8, strip_truth, truth_emissions,
};
use hk_e2e::{Fixture, TruthItem};
use hk_model::sigmf::SigmfMeta;
use hk_model::{ContentClass, FreqRange, Provenance, Region, Timestamp};
use hk_pipeline::{
    Pipeline, PipelineConfig, PipelineHandle, RunSummary, SourceInfo, TrackInventory,
    open_mock_replay, replay_plan, source_class,
};
use num_complex::{Complex, Complex32};
use serde_json::json;

use crate::common::*;

/// A device control the harness applies before reading block `at_block` (0-based) from the
/// device, exactly as a user or scheduler would through the control interface.
#[derive(Clone, Copy, Debug)]
pub struct DeviceStep {
    /// Blocks read before the control is applied.
    pub at_block: u64,
    /// The control.
    pub action: DeviceAction,
}

/// A device control.
#[derive(Clone, Copy, Debug)]
pub enum DeviceAction {
    /// `SourceControl::tune` to this centre, Hz.
    Tune(f64),
    /// `SourceControl::set_gain` on a named stage, dB.
    Gain(&'static str, f64),
}

/// How the recording is presented to the system.
#[derive(Clone, Copy, Debug, Default)]
pub struct BlindSource {
    /// RF relabel, Hz: identical samples at a different recorded centre.
    pub relabel_hz: f64,
    /// IQ frequency shift, Hz: every emission moves at an unchanged recorded centre (synthesised).
    /// A device `tune()` cannot do this: the mock preserves absolute frequencies on retune, as the
    /// air does.
    pub iq_shift_hz: f64,
    /// A content class the user vouches for on the recording (`hackriff:content_class`), e.g.
    /// `unrestricted` so content chains may run outside the frequency-derived band priors. This is
    /// user configuration, not truth.
    pub vouched_class: Option<&'static str>,
    /// An extra RF range, Hz, the user adds to the built-in `wfm-rds` chain
    /// (`ScanPlan.extra.pipeline.chains`), so the analog chain may demodulate there. User
    /// configuration, not truth: mode selection still decides whether it is WFM.
    pub demod_freq_hz: Option<[f64; 2]>,
    /// Device controls applied during the run.
    pub steps: &'static [DeviceStep],
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

/// The fixture's meta path and the truth only the test sees; `None` skips.
pub fn private_truth(name: &str) -> Option<(PathBuf, Fixture)> {
    let meta = real_fixture(name)?;
    let fixture = Fixture::load(&meta).unwrap();
    Some((meta, fixture))
}

// ---------------------------------------------------------------------------------------------
// Truth isolation.

/// The truth-stripped copy the device serves, with the fixture's truth sealed away from it.
pub struct Blinded {
    /// `dev/`: what the device reads. `private/`: the sealed truth.
    root: TempDir,
    /// The stripped `.sigmf-meta`.
    pub meta: PathBuf,
    vault: TruthVault,
    /// Strings only the truth holds (description, original and vault paths).
    sentinels: Vec<String>,
}

impl Blinded {
    fn dev_dir(&self) -> PathBuf {
        self.root.0.join("dev")
    }

    /// Truth isolation after a run over `data_dir` whose device served `served`.
    fn verify(&self, data_dir: &Path, served: &[PathBuf]) {
        let dev = self.dev_dir();
        for p in served {
            assert!(
                p.starts_with(&dev),
                "the device served {} instead of the stripped copy",
                p.display()
            );
            assert_truth_free(p);
        }
        static NOTE: std::sync::Once = std::sync::Once::new();
        NOTE.call_once(|| {
            eprintln!(
                "truth isolation: sealed truth access-time check {} on this filesystem",
                if self.vault.tracked() {
                    "effective"
                } else {
                    "unavailable (path isolation and output scans only)"
                }
            )
        });
        self.vault.assert_unopened();
        let bytes = all_bytes(data_dir);
        let sentinels: Vec<Vec<u8>> = self
            .sentinels
            .iter()
            .map(|s| s.as_bytes().to_vec())
            .collect();
        assert_eq!(
            count_found(&bytes, &sentinels),
            0,
            "truth-only text reached the run's outputs ({} bytes scanned)",
            bytes.len()
        );
    }
}

/// Seals `meta`'s truth and writes the stripped copy (relabelled, IQ-shifted or vouched per
/// `source`) the device will serve.
pub fn blind_copy(meta: &Path, tag: &str, source: &BlindSource) -> Blinded {
    let root = TempDir::new(&format!("{tag}src"));
    let dev = root.0.join("dev");
    std::fs::create_dir_all(&dev).unwrap();
    let vault = TruthVault::seal(meta, &root.0.join("private")).unwrap();
    let original = SigmfMeta::read(meta).unwrap();
    if source.iq_shift_hz != 0.0 {
        let fs = original.global.sample_rate.unwrap();
        shift_ci8(
            &meta.with_extension("sigmf-data"),
            &dev.join("blind.sigmf-data"),
            source.iq_shift_hz,
            fs,
        )
        .unwrap();
    }
    let blind = strip_truth(meta, &dev, "blind", source.relabel_hz).unwrap();
    if let Some(class) = source.vouched_class {
        let mut m = SigmfMeta::read(&blind).unwrap();
        m.global
            .extra
            .insert("hackriff:content_class".into(), json!(class));
        m.write(&blind).unwrap();
    }
    assert_truth_free(&blind);
    let mut sentinels = vec![
        meta.to_string_lossy().into_owned(),
        vault.path.to_string_lossy().into_owned(),
    ];
    sentinels.extend(original.global.description.filter(|d| d.len() >= 16));
    Blinded {
        root,
        meta: blind,
        vault,
        sentinels,
    }
}

// ---------------------------------------------------------------------------------------------
// The device.

/// A device opened over a blinded copy.
pub struct BlindDevice {
    /// The device stream.
    pub source: Box<dyn Source>,
    /// Rate, centre, start (the recording's power-on tuning).
    pub info: SourceInfo,
    /// Content class, as `hk_pipeline::source_class` derives it for the recording's window.
    pub class: ContentClass,
    /// Device identity (`mock:<recorded device>`).
    pub device: DeviceInfo,
    /// The mock control handles (stats, overrun injection), one per served recording.
    pub mock: Vec<Arc<MockSdrControl>>,
    served: Vec<PathBuf>,
    blinded: Blinded,
}

/// Opens the mock SDR over `blinded` with `steps` scripted.
fn open_device(
    blinded: Blinded,
    steps: &'static [DeviceStep],
    end: MockEnd,
    pacing: Pacing,
) -> BlindDevice {
    let meta = SigmfMeta::read(&blinded.meta).unwrap();
    let mut centres: Vec<f64> = meta.captures.iter().filter_map(|c| c.frequency).collect();
    centres.dedup();
    let class = source_class(&meta);
    let (source, info, device, mock, served): (Box<dyn Source>, _, _, _, _) = if centres.len() <= 1
    {
        let r = open_mock_replay(&blinded.meta, pacing, end).unwrap();
        let control = r.source.mock_control();
        let served = vec![r.source.recording().path.clone()];
        (Box::new(r.source), r.info, r.device, vec![control], served)
    } else {
        assert_eq!(end, MockEnd::Stop, "a survey recording does not loop");
        let segments = split_by_centre(&blinded.meta, &blinded.dev_dir().join("survey"));
        let opened: Vec<_> = segments
            .iter()
            .map(|p| open_mock_replay(p, Pacing::Unpaced, MockEnd::Stop).unwrap())
            .collect();
        let (info, device) = (opened[0].info, opened[0].device.clone());
        let served = opened
            .iter()
            .map(|r| r.source.recording().path.clone())
            .collect();
        let survey = SurveyDevice::new(opened.into_iter().map(|r| r.source).collect());
        let mock = survey.control.controls.clone();
        (Box::new(survey), info, device, mock, served)
    };
    let source: Box<dyn Source> = if steps.is_empty() {
        source
    } else {
        Box::new(Scripted {
            control: source.control(),
            inner: source,
            steps,
            blocks: 0,
        })
    };
    BlindDevice {
        source,
        info,
        class,
        device,
        mock,
        served,
        blinded,
    }
}

/// Applies [`DeviceStep`]s through the device's control handle as blocks are read.
struct Scripted {
    inner: Box<dyn Source>,
    control: Arc<dyn SourceControl>,
    steps: &'static [DeviceStep],
    blocks: u64,
}

impl Scripted {
    fn apply(&mut self) {
        for s in self.steps.iter().filter(|s| s.at_block == self.blocks) {
            eprintln!("device step at block {}: {:?}", self.blocks, s.action);
            match s.action {
                DeviceAction::Tune(hz) => self.control.tune(hz),
                DeviceAction::Gain(stage, db) => self.control.set_gain(stage, db),
            }
            .unwrap_or_else(|e| panic!("device refused {:?}: {e}", s.action));
        }
        self.blocks += 1;
    }
}

impl Source for Scripted {
    fn capabilities(&self) -> &SourceCapabilities {
        self.inner.capabilities()
    }

    fn control(&self) -> Arc<dyn SourceControl> {
        Arc::clone(&self.control)
    }

    fn pausable(&self) -> bool {
        self.inner.pausable()
    }

    fn read_block(
        &mut self,
        samples: &mut Vec<Complex32>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        self.apply();
        self.inner.read_block(samples)
    }

    fn read_block_ci8(
        &mut self,
        samples: &mut Vec<Complex<i8>>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        self.apply();
        self.inner.read_block_ci8(samples)
    }
}

/// Writes one single-centre recording per run of equal-centre captures of `meta` into `dir`
/// (the mock SDR serves single-centre recordings, T-049).
fn split_by_centre(meta: &Path, dir: &Path) -> Vec<PathBuf> {
    std::fs::create_dir_all(dir).unwrap();
    let m = SigmfMeta::read(meta).unwrap();
    let bps = m.global.datatype.bytes_per_sample();
    let data = std::fs::read(meta.with_extension("sigmf-data")).unwrap();
    let total = (data.len() / bps) as u64;
    let mut out = Vec::new();
    let mut i = 0;
    while i < m.captures.len() {
        let mut j = i + 1;
        while j < m.captures.len() && m.captures[j].frequency == m.captures[i].frequency {
            j += 1;
        }
        let start = m.captures[i].sample_start;
        let end = m.captures.get(j).map_or(total, |c| c.sample_start);
        let mut seg = m.clone();
        seg.captures = m.captures[i..j]
            .iter()
            .map(|c| {
                let mut c = c.clone();
                c.sample_start -= start;
                c.extra.remove("core:global_index");
                c
            })
            .collect();
        let path = dir.join(format!("segment{}.sigmf-meta", out.len()));
        seg.write(&path).unwrap();
        std::fs::write(
            path.with_extension("sigmf-data"),
            &data[start as usize * bps..end as usize * bps],
        )
        .unwrap();
        out.push(path);
        i = j;
    }
    out
}

/// A survey through the device: the radio is retuned to each recorded centre in turn, each
/// segment served by its own mock SDR. Sample counters continue across segments and the first
/// block of each later segment carries the retune discontinuity (not a stream start).
pub struct SurveyDevice {
    segments: Vec<MockSdrSource>,
    control: Arc<SurveyControl>,
    offset: u64,
    next_index: u64,
    last: Option<Provenance>,
    switched: Option<Provenance>,
}

impl SurveyDevice {
    fn new(segments: Vec<MockSdrSource>) -> Self {
        let control = Arc::new(SurveyControl {
            caps: segments[0].capabilities().clone(),
            controls: segments.iter().map(MockSdrSource::mock_control).collect(),
            current: AtomicUsize::new(0),
        });
        Self {
            segments,
            control,
            offset: 0,
            next_index: 0,
            last: None,
            switched: None,
        }
    }

    fn next<T>(
        &mut self,
        samples: &mut Vec<T>,
        read: impl Fn(&mut MockSdrSource, &mut Vec<T>) -> Result<Option<BlockHeader>, SourceError>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        loop {
            let cur = self.control.current.load(Ordering::Relaxed);
            match read(&mut self.segments[cur], samples)? {
                Some(mut h) => {
                    h.time.sample_index += self.offset;
                    if let Some(prev) = self.switched.take() {
                        let bits = h.discontinuity.bits() & !Discontinuity::STREAM_START.bits();
                        h.discontinuity = Discontinuity::from_bits_truncate(bits)
                            | Discontinuity::between(&prev, h.provenance.get());
                    }
                    self.last = Some(h.provenance.get().clone());
                    self.next_index = h.time.sample_index + samples.len() as u64;
                    return Ok(Some(h));
                }
                None if cur + 1 < self.segments.len() => {
                    self.offset = self.next_index;
                    self.switched = self.last.clone();
                    self.control.current.store(cur + 1, Ordering::Relaxed);
                }
                None => return Ok(None),
            }
        }
    }
}

impl Source for SurveyDevice {
    fn capabilities(&self) -> &SourceCapabilities {
        &self.control.caps
    }

    fn control(&self) -> Arc<dyn SourceControl> {
        self.control.clone()
    }

    fn pausable(&self) -> bool {
        self.segments.iter().all(MockSdrSource::pausable)
    }

    fn read_block(
        &mut self,
        samples: &mut Vec<Complex32>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        self.next(samples, |s, b| s.read_block(b))
    }

    fn read_block_ci8(
        &mut self,
        samples: &mut Vec<Complex<i8>>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        self.next(samples, |s, b| s.read_block_ci8(b))
    }
}

/// The survey's control handle: the segment being served.
pub struct SurveyControl {
    caps: SourceCapabilities,
    controls: Vec<Arc<MockSdrControl>>,
    current: AtomicUsize,
}

impl SurveyControl {
    fn cur(&self) -> &MockSdrControl {
        &self.controls[self.current.load(Ordering::Relaxed)]
    }
}

impl SourceControl for SurveyControl {
    fn capabilities(&self) -> &SourceCapabilities {
        &self.caps
    }
    fn tune(&self, center_hz: f64) -> Result<(), SourceError> {
        self.cur().tune(center_hz)
    }
    fn set_sample_rate(&self, sample_rate_hz: f64) -> Result<(), SourceError> {
        self.cur().set_sample_rate(sample_rate_hz)
    }
    fn set_gains(&self, gains: &Gains) -> Result<(), SourceError> {
        self.cur().set_gains(gains)
    }
    fn set_gain(&self, stage: &str, db: f64) -> Result<(), SourceError> {
        self.cur().set_gain(stage, db)
    }
    fn set_baseband_filter(&self, bandwidth_hz: f64) -> Result<(), SourceError> {
        self.cur().set_baseband_filter(bandwidth_hz)
    }
    fn set_bias_tee(&self, enabled: bool) -> Result<(), SourceError> {
        self.cur().set_bias_tee(enabled)
    }
    fn start(&self) -> Result<(), SourceError> {
        self.cur().start()
    }
    fn stop(&self) -> Result<(), SourceError> {
        self.controls.iter().try_for_each(|c| c.stop())
    }
    fn stats(&self) -> Option<SourceStats> {
        self.cur().stats()
    }
    fn device_info(&self) -> Option<DeviceInfo> {
        self.cur().device_info()
    }
}

// ---------------------------------------------------------------------------------------------
// Runs.

/// A running (or startable) blind run's handle: the pipeline's, plus the truth-isolation checks
/// [`finish`] runs once it has ended.
pub struct BlindHandle {
    handle: PipelineHandle,
    data_dir: PathBuf,
    served: Vec<PathBuf>,
    blinded: Blinded,
}

impl Deref for BlindHandle {
    type Target = PipelineHandle;
    fn deref(&self) -> &PipelineHandle {
        &self.handle
    }
}

impl Finishable for BlindHandle {
    fn wait_summary(self) -> RunSummary {
        let s = self.handle.wait().unwrap();
        self.blinded.verify(&self.data_dir, &self.served);
        s
    }
}

/// The run configuration for `dev`: the replay plan with `extra`, the device's id and class,
/// lossless (the unpaced mock is pausable). No mode, chain or parameter is chosen by the test.
fn device_config(dir: &Path, dev: &BlindDevice, extra: serde_json::Value) -> PipelineConfig {
    let mut plan = replay_plan(
        dev.info.center_hz,
        dev.info.sample_rate_hz,
        dev.info.start_time,
    );
    plan.extra = extra;
    let mut cfg = PipelineConfig::new(dir, plan).unwrap();
    cfg.source_class = dev.class;
    cfg.lossless = dev.source.pausable();
    cfg.device_id = dev.device.device_id.clone();
    cfg.device_hw = Some(dev.device.hw.clone());
    cfg
}

/// Starts a run on `dev` with the default inventory policy (T-018 clustering + band-plan priors).
pub fn start(cfg: PipelineConfig, dev: BlindDevice) -> BlindHandle {
    let data_dir = cfg.data_dir.clone();
    let handle = Pipeline::start(
        cfg,
        dev.source,
        dev.info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    BlindHandle {
        handle,
        data_dir,
        served: dev.served,
        blinded: dev.blinded,
    }
}

/// A (synthetic or real) fixture through the mock device with its truth stripped, configured
/// with `extra` as the plan's `extra`. `pacing` must be unpaced (lossless).
pub fn replay_config(
    dir: &Path,
    meta: &Path,
    extra: serde_json::Value,
    pacing: Pacing,
) -> (PipelineConfig, BlindDevice) {
    assert!(matches!(pacing, Pacing::Unpaced), "acceptance runs unpaced");
    let dev = open_device(
        blind_copy(meta, "dev", &BlindSource::default()),
        &[],
        MockEnd::Stop,
        Pacing::Unpaced,
    );
    (device_config(dir, &dev, extra), dev)
}

/// A blind run's configuration before it starts.
pub struct BlindConfig {
    /// Data directory (SQLite, tiles).
    pub dir: TempDir,
    /// The run's configuration.
    pub cfg: PipelineConfig,
    /// The device over the truth-stripped copy.
    pub replay: BlindDevice,
}

fn configure(
    meta: &Path,
    tag: &str,
    source: BlindSource,
    extra: serde_json::Value,
    end: MockEnd,
    pacing: Pacing,
) -> BlindConfig {
    let blinded = blind_copy(meta, tag, &source);
    let dir = TempDir::new(tag);
    let mut plan = plan_extra(&source);
    merge_json(&mut plan, extra);
    let replay = open_device(blinded, source.steps, end, pacing);
    let cfg = device_config(&dir.0, &replay, plan);
    BlindConfig { dir, cfg, replay }
}

/// Strips `meta`'s truth and configures an unpaced device run of it with `extra` as the plan's
/// `extra`.
pub fn blind_config(
    meta: &Path,
    tag: &str,
    source: BlindSource,
    extra: serde_json::Value,
) -> BlindConfig {
    configure(meta, tag, source, extra, MockEnd::Stop, Pacing::Unpaced)
}

/// A blind run whose device keeps replaying its truth-stripped copy (`MockEnd::Loop`, as
/// `hk serve --device mock:<meta>` does) until stopped, for tests that act on the running system
/// (Listen, T-043).
pub struct BlindLive {
    /// Data directory (SQLite, tiles).
    pub dir: TempDir,
    /// The running pipeline; stop it with `handle.stop()` then [`finish`].
    pub handle: BlindHandle,
}

/// Starts a looping blind device run of `meta`.
pub fn blind_live(meta: &Path, tag: &str, source: BlindSource) -> BlindLive {
    blind_live_paced(meta, tag, source, Pacing::Unpaced)
}

/// [`blind_live`] with the device released at `pacing` (real time makes the run live-like: not
/// lossless, the ring overwrites and chains skip to the live edge, as on a HackRF).
pub fn blind_live_paced(meta: &Path, tag: &str, source: BlindSource, pacing: Pacing) -> BlindLive {
    let BlindConfig { dir, cfg, replay } =
        configure(meta, tag, source, json!({}), MockEnd::Loop, pacing);
    BlindLive {
        dir,
        handle: start(cfg, replay),
    }
}

/// [`blind_live`] with the run's streams registered in `streams`, as `hk serve` wires them, so
/// recipe pipelines (T-088) serve their inspector and stage streams over the API (T-094).
pub fn blind_live_streams(
    meta: &Path,
    tag: &str,
    source: BlindSource,
    streams: &hk_api::StreamRegistry,
) -> BlindLive {
    let BlindConfig {
        dir,
        mut cfg,
        replay,
    } = configure(meta, tag, source, json!({}), MockEnd::Loop, Pacing::Unpaced);
    let reg = streams.clone();
    cfg.stream_sink = Some(Arc::new(move |h, p| reg.register(h, p)));
    let reg = streams.clone();
    cfg.stream_unsink = Some(Arc::new(move |id| {
        reg.unregister(id);
    }));
    BlindLive {
        dir,
        handle: start(cfg, replay),
    }
}

/// A finished blind run.
pub struct BlindRun {
    /// Data directory (SQLite, tiles).
    pub dir: TempDir,
    /// Run summary.
    pub summary: RunSummary,
    /// `/api/inventory` rows.
    pub api_rows: Vec<serde_json::Value>,
    /// The mock stats of the device(s) after the run.
    pub mock: Vec<hk_core::MockStats>,
}

/// Runs `meta` through the device with its truth stripped.
pub fn blind_replay(meta: &Path, tag: &str, source: BlindSource) -> BlindRun {
    let BlindConfig { dir, cfg, replay } = blind_config(meta, tag, source, json!({}));
    let controls = replay.mock.clone();
    let handle = start(cfg, replay);
    let counters = handle.counters();
    let summary = finish(handle);
    let server = serve_api(&dir.0, counters);
    let (_, api_rows) = api_inventory(server.local_addr());
    BlindRun {
        dir,
        summary,
        api_rows,
        mock: controls.iter().map(|c| c.mock_stats()).collect(),
    }
}

// ---------------------------------------------------------------------------------------------
// Blind truth assertions.

/// Explanations considered per emitter: the top-k of `/api/inventory`.
pub const TOP_K: usize = 3;
/// Time tolerance around a truth emission's span, s.
pub const TIME_TOL_S: f64 = 0.01;
/// Smallest centre tolerance, Hz; wider emissions allow half their bandwidth.
pub const MIN_CENTER_TOL_HZ: f64 = 10e3;

/// Centre tolerance for `t`.
pub fn center_tol_hz(t: &TruthItem) -> f64 {
    (0.5 * t.bandwidth_hz()).max(MIN_CENTER_TOL_HZ)
}

/// The explanations (`Explanation.service`) the suite accepts as reasonable for a truth kind.
/// Set once here; a kind with no entry asserts detection only.
pub fn reasonable_services(kind: &str) -> &'static [&'static str] {
    match kind {
        "wfm-broadcast" => &["fm-broadcast"],
        "fsk-burst" => &["ism"],
        "adsb-df17" => &["adsb"],
        // 446.0 MHz in the (US) band table: the 70 cm amateur allocation.
        "nbfm-burst" => &["amateur", "land-mobile"],
        // An unmodulated carrier has no family: only an allocation could be suggested, and
        // asserting that would be DB-as-truth (docs/10 §3.2).
        _ => &[],
    }
}

/// `/api/inventory` rows of a finished run's data directory (inventory only).
pub fn inventory_rows(dir: &Path) -> Vec<serde_json::Value> {
    let config = ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        Token::from_config(API_TOKEN).unwrap(),
    );
    let state = ApiState {
        inventory: Some(Arc::new(std::sync::Mutex::new(repo(dir)))),
        ..ApiState::default()
    };
    let server = Server::start(config, state).unwrap();
    api_inventory(server.local_addr()).1
}

/// Recording start of `fx` (its first capture's `core:datetime`).
pub fn recording_start(fx: &Fixture) -> Timestamp {
    hk_core::source::sigmf_replay::parse_sigmf_datetime(
        fx.meta.captures[0].datetime.as_deref().unwrap(),
    )
    .unwrap()
}

/// Every truth emission of `fx` carrying an identity (`/identity/value` or `icao`) was decoded
/// by the system: a CRC-valid Decode with that identity within `tol_s` of the emission, each
/// decode accounting for one emission only (paired in time order). For bursts shorter than a
/// detector frame (ADS-B squitters, 120 µs), which the decoder chain resolves.
pub fn assert_truth_decoded(tag: &str, dir: &Path, fx: &Fixture, tol_s: f64) {
    use std::collections::BTreeMap;
    let t0 = recording_start(fx);
    let repo = repo(dir);
    let truth = truth_emissions(fx);
    assert!(!truth.is_empty(), "[{tag}] the fixture has no truth list");
    let mut per_id: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for t in &truth {
        assert_eq!(
            t.kind, "adsb-df17",
            "[{tag}] no identity scheme for truth kind {}",
            t.kind
        );
        let id = t
            .identity()
            .map(|(_, v)| v.to_owned())
            .or_else(|| t.str("icao").map(str::to_owned))
            .unwrap_or_else(|| panic!("[{tag}] truth {} has no identity", t.kind));
        per_id.entry(id).or_default().push(t.t_start_s);
    }
    let mut missed = Vec::new();
    for (id, mut times) in per_id {
        times.sort_by(f64::total_cmp);
        let mut decoded: Vec<f64> = repo
            .decodes_for_identity(&hk_model::DecodedIdentity {
                scheme: hk_model::IdentityScheme::AdsbIcao,
                value: id.clone(),
            })
            .unwrap()
            .iter()
            .filter(|d| d.crc_status == hk_model::CrcStatus::Valid)
            .map(|d| (d.t.as_unix_nanos() - t0.as_unix_nanos()) as f64 * 1e-9)
            .collect();
        decoded.sort_by(f64::total_cmp);
        let mut next = 0;
        for t in times {
            match decoded[next..].iter().position(|d| (d - t).abs() <= tol_s) {
                Some(k) => next += k + 1,
                None => missed.push((id.clone(), t, decoded.clone())),
            }
        }
    }
    eprintln!(
        "[{tag}] {} of {} truth emissions decoded within {tol_s} s",
        truth.len() - missed.len(),
        truth.len()
    );
    assert!(
        missed.is_empty(),
        "[{tag}] truth emissions not decoded: {missed:?}"
    );
}

/// What [`assert_truth_found`] saw per truth emission.
#[derive(Debug)]
#[allow(dead_code)] // read through Debug in failure messages and by tests as needed
pub struct TruthFound {
    /// Truth kind and label.
    pub kind: String,
    /// Matching detections.
    pub detections: usize,
    /// Matching `/api/inventory` emitters and their top-k services.
    pub emitters: Vec<(f64, Vec<String>)>,
}

/// The blind ground-truth check (T-047): every emission of `fx`'s private truth list (moved by
/// `shift_hz`) must be **detected** within frequency/extent ([`center_tol_hz`] and overlap) and
/// time ([`TIME_TOL_S`]) tolerance, and, when `explain`, an emitter matching it must carry a
/// [`reasonable_services`] explanation among its [`TOP_K`] on `/api/inventory`. Everything the run
/// produced is listed first; truth is only read here, after the run.
pub fn assert_truth_found(
    tag: &str,
    dir: &Path,
    fx: &Fixture,
    shift_hz: f64,
    explain: bool,
) -> Vec<TruthFound> {
    truth_check(tag, dir, fx, shift_hz, Some(explain))
}

/// [`assert_truth_found`]'s matching, reported without asserting: for truth the system is known
/// not to persist or resolve yet (each caller says why), so the gap stays visible in the log.
pub fn truth_report(tag: &str, dir: &Path, fx: &Fixture, shift_hz: f64) -> Vec<TruthFound> {
    truth_check(tag, dir, fx, shift_hz, None)
}

fn truth_check(
    tag: &str,
    dir: &Path,
    fx: &Fixture,
    shift_hz: f64,
    assert_explain: Option<bool>,
) -> Vec<TruthFound> {
    let t0 = recording_start(fx);
    let repo = repo(dir);
    let detections = repo
        .detections_in_region(&Region::new(FreqRange::new(0.0, 7.0e9), ever()))
        .unwrap();
    let rows = inventory_rows(dir);
    let truth = truth_emissions(fx);
    assert!(!truth.is_empty(), "[{tag}] the fixture has no truth list");
    let mut out = Vec::new();
    for t in truth {
        let tol = center_tol_hz(t);
        let (a, b) = (
            t0.saturating_add_nanos(((t.t_start_s - TIME_TOL_S) * 1e9) as i64),
            t0.saturating_add_nanos(((t.t_end_s + TIME_TOL_S) * 1e9) as i64),
        );
        let dets = detections
            .iter()
            .filter(|d| {
                d.time.start <= b
                    && d.time.end >= a
                    && matches_truth(t, shift_hz, d.f_center_hz, d.obw_hz, tol)
            })
            .count();
        let emitters: Vec<(f64, Vec<String>)> = rows
            .iter()
            .filter(|r| {
                matches_truth(
                    t,
                    shift_hz,
                    r["f_center_hz"].as_f64().unwrap_or(f64::NAN),
                    r["bandwidth_hz"].as_f64().unwrap_or(0.0),
                    tol,
                )
            })
            .map(|r| {
                let top = r["explanations"]
                    .as_array()
                    .map(|x| {
                        x.iter()
                            .take(TOP_K)
                            .filter_map(|e| e["service"].as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                (r["f_center_hz"].as_f64().unwrap_or(f64::NAN), top)
            })
            .collect();
        let label = t.label.clone().unwrap_or_default();
        eprintln!(
            "[{tag}] truth {} {label:?} {:.4}-{:.4} MHz (+{shift_hz} Hz) t {:.3}-{:.3} s: {dets} \
             detections, emitters (Hz, top-{TOP_K}) {emitters:?}",
            t.kind,
            t.f_lo_hz / 1e6,
            t.f_hi_hz / 1e6,
            t.t_start_s,
            t.t_end_s
        );
        let Some(explain) = assert_explain else {
            out.push(TruthFound {
                kind: t.kind.clone(),
                detections: dets,
                emitters,
            });
            continue;
        };
        assert!(
            dets > 0,
            "[{tag}] truth {} {label:?} was not detected blind",
            t.kind
        );
        let want = reasonable_services(&t.kind);
        if explain && !want.is_empty() {
            assert!(
                emitters
                    .iter()
                    .any(|(_, top)| top.iter().any(|s| want.contains(&s.as_str()))),
                "[{tag}] truth {} {label:?}: no emitter with {want:?} in its top-{TOP_K}: \
                 {emitters:?}",
                t.kind
            );
        }
        out.push(TruthFound {
            kind: t.kind.clone(),
            detections: dets,
            emitters,
        });
    }
    out
}
