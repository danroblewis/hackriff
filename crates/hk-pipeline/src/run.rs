//! Starting, observing and finishing a run.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use anyhow::Context as _;
use hk_core::{
    Pacing, ReplayOptions, RingConfig, RingHandle, SigmfReplaySource, Source, ring_buffer,
};
use hk_dsp::radiometry::PowerCalibrations;
use hk_model::sigmf::SigmfMeta;
use hk_model::{
    ContentClass, InventoryQuery, Repository, Survey, SurveyId, SurveyState, SurveySummary,
    Timestamp,
};
use hk_store::{FloorProduct, FloorProductConfig};
use num_complex::Complex;
use serde::Serialize;
use serde_json::Value;

use crate::chains::spec::ChainSpec;
use crate::class::{class_name, source_class};
use crate::config::{PipelineConfig, detection_resolution};
use crate::control::SchedState;
use crate::events::{Candidate, ControlEvent};
use crate::gate::FlowGate;
use crate::inventory::Inventory;
use crate::stats::{Counters, get};

/// Reopens the source for `--loop`.
pub type SourceFactory = Box<dyn FnMut() -> anyhow::Result<Box<dyn Source>> + Send>;

/// What the pipeline needs to know about a source before its first block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SourceInfo {
    /// Sample rate, Hz.
    pub sample_rate_hz: f64,
    /// Centre, Hz.
    pub center_hz: f64,
    /// Time of the first sample.
    pub start_time: Timestamp,
}

/// An opened SigMF replay.
pub struct Replay {
    /// The source.
    pub source: SigmfReplaySource,
    /// Rate, centre, start.
    pub info: SourceInfo,
    /// Source class ([`crate::class::source_class`]).
    pub class: ContentClass,
    /// Metadata.
    pub meta: SigmfMeta,
}

/// Replay block length: ~5 ms, 1024..65536 samples.
pub fn replay_block_len(fs: f64) -> usize {
    ((fs / 200.0) as usize).clamp(1024, 65_536)
}

/// Opens a `.sigmf-meta` recording for the pipeline.
pub fn open_replay(path: &Path, pacing: Pacing, virtual_tuning: bool) -> anyhow::Result<Replay> {
    let meta = SigmfMeta::read(path).with_context(|| format!("reading {}", path.display()))?;
    let fs = meta
        .global
        .sample_rate
        .context("the recording has no core:sample_rate")?;
    let mut source = SigmfReplaySource::open(
        path,
        ReplayOptions {
            block_len: replay_block_len(fs),
            pacing,
        },
    )
    .with_context(|| format!("opening {}", path.display()))?;
    if virtual_tuning {
        source = source.with_virtual_tuning();
    }
    let center_hz = meta
        .captures
        .first()
        .and_then(|c| c.frequency)
        .or_else(|| meta.global.provenance.as_ref().map(|p| p.tune.center_hz))
        .unwrap_or(0.0);
    let start_time = meta
        .captures
        .first()
        .and_then(|c| c.datetime.as_deref())
        .and_then(hk_core::source::sigmf_replay::parse_sigmf_datetime)
        .unwrap_or(Timestamp::UNIX_EPOCH);
    Ok(Replay {
        class: source_class(&meta),
        info: SourceInfo {
            sample_rate_hz: fs,
            center_hz,
            start_time,
        },
        meta,
        source,
    })
}

/// State shared by the pipeline's threads.
pub(crate) struct Shared {
    pub cfg: PipelineConfig,
    pub counters: Arc<Counters>,
    pub ring: RingHandle<Complex<i8>>,
    pub gate: Arc<FlowGate>,
    pub repo: Mutex<Repository>,
    pub db_path: PathBuf,
    pub stop: Arc<AtomicBool>,
    pub survey_id: SurveyId,
    pub fs: f64,
    pub fft_len: usize,
    pub averages: usize,
    pub inventory: Mutex<Box<dyn Inventory>>,
    pub specs: Vec<ChainSpec>,
}

impl Shared {
    /// The shared repository connection.
    pub fn repo(&self) -> MutexGuard<'_, Repository> {
        self.repo.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Starts pipeline runs.
pub struct Pipeline;

type Worker = (&'static str, JoinHandle<anyhow::Result<()>>);

impl Pipeline {
    /// Opens the stores, opens the Survey, and starts the threads (capture last).
    pub fn start(
        cfg: PipelineConfig,
        source: Box<dyn Source>,
        info: SourceInfo,
        reopen: Option<crate::run::SourceFactory>,
        inventory: Box<dyn Inventory>,
    ) -> anyhow::Result<PipelineHandle> {
        if cfg.lossless && !source.pausable() {
            anyhow::bail!(
                "lossless mode needs a source that can pause (a recording), and {} cannot: a \
                 held-back capture thread would drop live samples; set PipelineConfig::lossless \
                 = false",
                source.capabilities().driver
            );
        }
        std::fs::create_dir_all(&cfg.data_dir)
            .with_context(|| format!("creating {}", cfg.data_dir.display()))?;
        let db_path = cfg.data_dir.join("hackriff.db");
        let mut repo =
            Repository::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;
        if repo.scan_plan(cfg.plan.id, cfg.plan.version).is_err() {
            repo.insert_scan_plan(&cfg.plan)?;
        }
        let survey = Survey {
            id: SurveyId::new(),
            plan_id: cfg.plan.id,
            plan_version: cfg.plan.version,
            device_id: cfg.device_id.clone(),
            state: SurveyState::Open,
            t_start: info.start_time,
            t_end: None,
            summary: None,
        };
        repo.insert_survey(&survey)?;
        let fs = info.sample_rate_hz;
        let (fft_len, averages) = detection_resolution(fs, &cfg.settings);
        let min_block = replay_block_len(fs).min(1024);
        let ring_cfg = RingConfig::for_duration(fs, cfg.settings.ring_s.max(0.5), min_block);
        let (writer, ring) = ring_buffer::<Complex<i8>>(ring_cfg);
        let gate = Arc::new(FlowGate::new(cfg.lossless, ring.sample_capacity()));
        let product = FloorProduct::open(
            cfg.data_dir.join("history"),
            FloorProductConfig::default(),
            PowerCalibrations::new(),
        )
        .map_err(|e| anyhow::anyhow!("opening history: {e}"))?;
        let product = Arc::new(Mutex::new(product));
        let counters = Arc::new(Counters::default());
        let stop = Arc::new(AtomicBool::new(false));
        let sched = if cfg.drive_scheduler {
            Some(SchedState::new(
                &cfg.plan,
                source.control(),
                fs,
                info.start_time,
                Arc::clone(&counters),
                cfg.settings.verify_pois,
            )?)
        } else {
            None
        };
        let specs = cfg.settings.chain_specs();
        let shared = Arc::new(Shared {
            counters: Arc::clone(&counters),
            ring,
            gate,
            repo: Mutex::new(repo),
            db_path,
            stop: Arc::clone(&stop),
            survey_id: survey.id,
            fs,
            fft_len,
            averages,
            inventory: Mutex::new(inventory),
            specs,
            cfg,
        });

        let (tx, rx) = mpsc::channel::<ControlEvent>();
        let mut workers: Vec<Worker> = Vec::new();
        let spawn = |name: &'static str,
                     f: Box<dyn FnOnce() -> anyhow::Result<()> + Send>|
         -> anyhow::Result<Worker> {
            Ok((name, thread::Builder::new().name(name.into()).spawn(f)?))
        };
        {
            let (s, t) = (Arc::clone(&shared), tx.clone());
            workers.push(spawn(
                "hk-detect",
                Box::new(move || crate::detect::run(s, t)),
            )?);
        }
        {
            let (s, p) = (Arc::clone(&shared), Arc::clone(&product));
            workers.push(spawn(
                "hk-history",
                Box::new(move || crate::history::run(s, p)),
            )?);
        }
        {
            let s = Arc::clone(&shared);
            workers.push(spawn(
                "hk-spectrum",
                Box::new(move || crate::spectrum::run(s)),
            )?);
        }
        {
            let s = Arc::clone(&shared);
            workers.push(spawn(
                "hk-control",
                Box::new(move || crate::control::run(s, rx, sched)),
            )?);
        }
        let s = Arc::clone(&shared);
        let capture = hk_core::rt::spawn_capture_thread("hk-capture", move |_priority| {
            let (writer, s) = (writer, s);
            let r = crate::capture::run(source, reopen, writer, Arc::clone(&s));
            if r.is_err() {
                s.stop.store(true, Ordering::SeqCst);
            }
            r
        })?;
        workers.insert(0, ("hk-capture", capture));
        Ok(PipelineHandle {
            shared,
            product,
            workers,
            tx,
            started: Instant::now(),
        })
    }
}

/// Stops a running pipeline ([`PipelineHandle::stopper`]).
#[derive(Clone, Debug)]
pub struct Stopper(Arc<AtomicBool>);

impl Stopper {
    /// Stops capture, like [`PipelineHandle::stop`].
    pub fn stop(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// `stop` has been called (by anyone).
    pub fn is_stopped(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// A running pipeline.
pub struct PipelineHandle {
    shared: Arc<Shared>,
    product: Arc<Mutex<FloorProduct>>,
    workers: Vec<Worker>,
    tx: Sender<ControlEvent>,
    started: Instant,
}

/// Detection resolution of the run.
#[derive(Clone, Copy, Debug, Serialize, PartialEq)]
pub struct ResolutionSummary {
    /// FFT length.
    pub fft_len: usize,
    /// Averages `K`.
    pub averages: usize,
    /// Segment overlap, samples (0).
    pub overlap: usize,
    /// Effective averages of the thresholds (= K with no overlap).
    pub n_eff: f64,
    /// Bin width, Hz.
    pub bin_hz: f64,
    /// Frame period, s.
    pub frame_s: f64,
}

/// What a run did.
#[derive(Clone, Debug, Serialize)]
pub struct RunSummary {
    /// Data directory.
    pub data_dir: PathBuf,
    /// Survey.
    pub survey_id: SurveyId,
    /// Wall-clock run time, s.
    pub elapsed_s: f64,
    /// Source class.
    pub source_class: String,
    /// Detection resolution.
    pub resolution: ResolutionSummary,
    /// Detection rows in the database.
    pub detections_stored: u64,
    /// Live emitters in the inventory (`query_inventory`).
    pub emitters: u64,
    /// Samples the always-on readers lost to ring overruns.
    pub always_on_lost_samples: u64,
    /// Every counter.
    pub counters: Value,
    /// Thread errors.
    pub errors: Vec<String>,
}

impl RunSummary {
    /// A counter by JSON pointer, e.g. `/chains/attached`.
    pub fn counter(&self, pointer: &str) -> u64 {
        self.counters
            .pointer(pointer)
            .and_then(Value::as_u64)
            .unwrap_or(0)
    }

    /// A human-readable summary.
    pub fn to_text(&self) -> String {
        let c = |p: &str| self.counter(p);
        let r = &self.resolution;
        let mut s = String::new();
        let mut line = |l: String| {
            s.push_str(&l);
            s.push('\n');
        };
        line(format!("data dir:    {}", self.data_dir.display()));
        line(format!(
            "survey:      {} ({:.2} s)",
            self.survey_id, self.elapsed_s
        ));
        line(format!("class:       {}", self.source_class));
        line(format!(
            "resolution:  {} bins x {} averages, overlap {}, n_eff {:.0}, {:.1} Hz bins, {:.2} ms frames",
            r.fft_len,
            r.averages,
            r.overlap,
            r.n_eff,
            r.bin_hz,
            r.frame_s * 1e3
        ));
        line(format!(
            "source:      {} samples in {} blocks, {} loops, {} gate waits, {} ring errors",
            c("/source/samples"),
            c("/source/blocks"),
            c("/source/loops"),
            c("/source/gate_waits"),
            c("/source/ring_errors")
        ));
        for name in ["detect", "history", "spectrum"] {
            line(format!(
                "reader {name:<9} {} samples, {} frames, lost {} ({} overruns), gaps {}",
                c(&format!("/readers/{name}/samples")),
                c(&format!("/readers/{name}/frames")),
                c(&format!("/readers/{name}/lost_samples")),
                c(&format!("/readers/{name}/overruns")),
                c(&format!("/readers/{name}/gap_samples")),
            ));
        }
        line(format!(
            "detections:  {} ({} stored), {} confirmations, {} dense frames",
            c("/detect/detections"),
            self.detections_stored,
            c("/detect/confirmations"),
            c("/detect/dense_frames")
        ));
        line(format!(
            "tracks:      {} opened, {} closed, {} confirmed, {} rows, {} links",
            c("/detect/tracks_opened"),
            c("/detect/tracks_closed"),
            c("/detect/tracks_confirmed"),
            c("/detect/track_rows"),
            c("/detect/track_links")
        ));
        line(format!(
            "anomalies:   {} opened, {} closed, {} explanations",
            c("/detect/anomalies_opened"),
            c("/detect/anomalies_closed"),
            c("/detect/explanations")
        ));
        line(format!(
            "chains:      {} attached, {} detached, {} refused (class), {} unmatched, {} errors",
            c("/chains/attached"),
            c("/chains/detached"),
            c("/chains/refused_class"),
            c("/chains/unmatched"),
            c("/chains/errors") + c("/chains/attach_errors")
        ));
        line(format!(
            "decodes:     {} demodulations, {} decodes ({} content withheld), {} CRC-valid; plugins {} decodes, {} records dropped, {} restarts",
            c("/chains/demodulations"),
            c("/chains/decodes"),
            c("/chains/content_withheld"),
            c("/chains/crc_valid"),
            c("/chains/plugin_decodes"),
            c("/chains/plugin_dropped"),
            c("/chains/plugin_restarts")
        ));
        line(format!(
            "rejected:    {} outside window, {} duplicate channel, {} mode rejected, {} FSK boxes missed",
            c("/chains/outside_window"),
            c("/chains/duplicate_channel"),
            c("/chains/mode_rejected"),
            c("/chains/fsk_boxes_missed")
        ));
        line(format!(
            "emitters:    {} in inventory, {} labels; recordings {}",
            self.emitters,
            c("/chains/labels"),
            c("/chains/recordings")
        ));
        line(format!(
            "history:     {} frames, {} tiles written; spectrum {} rows ({} gated)",
            c("/history/frames_ingested"),
            c("/history/tiles_written"),
            c("/spectrum/rows"),
            c("/spectrum/rows_gated")
        ));
        line(format!(
            "scheduler:   {} steps, {} POIs, {} verifications",
            c("/scheduler/steps"),
            c("/scheduler/pois_offered"),
            c("/scheduler/verifications")
        ));
        line(format!(
            "drops:       always-on readers lost {} samples",
            self.always_on_lost_samples
        ));
        for e in &self.errors {
            line(format!("error:       {e}"));
        }
        s
    }
}

impl PipelineHandle {
    /// Live counters.
    pub fn counters(&self) -> Arc<Counters> {
        Arc::clone(&self.shared.counters)
    }

    /// The floor product (history), shared with `/api/history` and `/api/floor`.
    pub fn floor_product(&self) -> Arc<Mutex<FloorProduct>> {
        Arc::clone(&self.product)
    }

    /// The data directory (`hackriff.db`, `history/`, `recordings/`).
    pub fn data_dir(&self) -> &Path {
        &self.shared.cfg.data_dir
    }

    /// The run's Survey.
    pub fn survey_id(&self) -> SurveyId {
        self.shared.survey_id
    }

    /// Stops capture; the readers and chains then drain and finish.
    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);
    }

    /// A handle that stops this run from another thread (a watchdog, a signal handler) while
    /// [`Self::wait`] owns the handle.
    pub fn stopper(&self) -> Stopper {
        Stopper(Arc::clone(&self.shared.stop))
    }

    /// Attaches `spec` for `candidate` at runtime (no restart).
    pub fn attach_chain(&self, spec: ChainSpec, candidate: Candidate) {
        let _ = self.tx.send(ControlEvent::Manual {
            spec: Box::new(spec),
            candidate,
        });
    }

    /// Detaches every chain attached with [`Self::attach_chain`].
    pub fn detach_manual_chains(&self) {
        let _ = self.tx.send(ControlEvent::DetachManual);
    }

    /// The newest ring sample index.
    pub fn ring_position(&self) -> u64 {
        self.shared.ring.next_sample().unwrap_or(0)
    }

    /// Waits for every thread, closes the Survey and summarises.
    pub fn wait(self) -> anyhow::Result<RunSummary> {
        let mut errors = Vec::new();
        for (name, join) in self.workers {
            match join.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => errors.push(format!("{name}: {e:#}")),
                Err(_) => errors.push(format!("{name}: panicked")),
            }
        }
        drop(self.tx);
        let shared = self.shared;
        let counters = &shared.counters;
        let lost = counters.always_on_lost();
        let mut repo = shared.repo();
        let t_end = Timestamp::from_unix_nanos(counters.stream_time_ns.load(Ordering::Relaxed));
        let summary = SurveySummary {
            sweep_frames: 0,
            spectrum_frames: get(&counters.detect_reader.frames),
            detections: get(&counters.detect.detections),
            recordings: get(&counters.chains.recordings),
            dropped_samples: lost + get(&counters.source.source_dropped),
        };
        if let Err(e) = repo.finish_survey(shared.survey_id, SurveyState::Closed, t_end, &summary) {
            errors.push(format!("closing the survey: {e}"));
        }
        let detections_stored = repo.detection_count().unwrap_or(0);
        let mut emitters = 0u64;
        let mut q = InventoryQuery {
            limit: 100,
            ..InventoryQuery::default()
        };
        while let Ok(page) = repo.query_inventory(&q) {
            emitters += page.entries.len() as u64;
            match page.next_offset {
                Some(o) => q.offset = o,
                None => break,
            }
        }
        let _ = repo.checkpoint();
        drop(repo);
        let fs = shared.fs;
        let resolution = ResolutionSummary {
            fft_len: shared.fft_len,
            averages: shared.averages,
            overlap: 0,
            n_eff: shared.averages as f64,
            bin_hz: fs / shared.fft_len as f64,
            frame_s: (shared.fft_len * shared.averages) as f64 / fs,
        };
        Ok(RunSummary {
            data_dir: shared.cfg.data_dir.clone(),
            survey_id: shared.survey_id,
            elapsed_s: self.started.elapsed().as_secs_f64(),
            source_class: class_name(shared.cfg.source_class),
            resolution,
            detections_stored,
            emitters,
            always_on_lost_samples: lost,
            counters: counters.to_json(),
            errors,
        })
    }
}

/// Runs a replay once through the pipeline (the `hk replay` path).
pub fn replay_once(
    path: &Path,
    mut cfg: PipelineConfig,
    pacing: Pacing,
    inventory: Box<dyn Inventory>,
) -> anyhow::Result<RunSummary> {
    let replay = open_replay(path, pacing, cfg.drive_scheduler)?;
    cfg.source_class = replay.class;
    cfg.lossless = matches!(pacing, Pacing::Unpaced);
    if let Some(hw) = &replay.meta.global.hw {
        cfg.device_id = format!("sigmf:{hw}");
    }
    let handle = Pipeline::start(cfg, Box::new(replay.source), replay.info, None, inventory)?;
    handle.wait()
}
