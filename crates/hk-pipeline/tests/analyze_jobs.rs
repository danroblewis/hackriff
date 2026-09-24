//! T-859 (MAUTO M-8; ADR-0015 §5.1–§5.3, ADR-0021 §4, §7A.4): the region-analyze **job manager**
//! behind `/api/analyze`, driven through its two seams — a ring (`RingRead`) and a search backend
//! running the real `hk_synth` engine over an evaluator whose hidden truth the engine never sees.
//!
//! Asserted on the served `AnalyzeJob` (state, `end_reason`, `window` read from the ledger,
//! `results`, `trace_summary`, `resolution`), the ADR-0021 §4.2 trace fetch and the
//! `hackriff.analyze/1` records — never on internals:
//!
//! - a job **acquires what it reads** (coverage from the ledger, 60/40 search/hold-out), searches,
//!   and ends `done` with a solved rank-1 result and no `resolution`;
//! - with **no backend** (MAUTO M-2 not built) it acquires and ends `failed` / `no_evaluator`,
//!   and says `not-searched` — never `unknown`;
//! - **cancel** is immediate and final for a running job (partial results kept) and removes a
//!   queued one; a finished job is **forgotten**, and a forgotten id is `410`, an unissued one `404`;
//! - admission: one running + 4 queued, `503 busy` beyond; `deep` on battery is `422 power`;
//!   the source errors are answered at admission (`410 evicted`, `409 outside_window`, `503`);
//! - a live job waits on the **capture clock** (the ring's edge), not a wall clock;
//! - the stream ends with `done`, after `best` and one `trace` record per stage.
//!
//! Use case: RESEARCH-002 (a never-seen FSK/OOK sensor decoded blind) at the job level — the IQ
//! evaluator is M-2's and the blind suite M-12's.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use hk_blocks::Registry;
use hk_model::{
    BiasTee, ClockSource, ContentClass, EmitterId, FreqRange, Provenance, TimeRange, Timestamp,
    TimestampMethod, Tune,
};
use hk_pipeline::iqbuffer::{ClipError, ReadChunk};
use hk_pipeline::synth::acquire::RingRead;
use hk_pipeline::synth::jobs::{
    AnalyzeJob, AnalyzeJobs, JobEnv, JobRequest, JobState, PowerPolicy, Profile, RingState,
    SearchBackend, SearchInput, SourceChoice, TemplateFilter, TraceQuery,
};
use hk_recipe::{InputSpec, OutputPolicy, PortType};
use hk_store::iqbuffer::ClipPiece;
use hk_synth::engine::{
    EvalError, EvalRequest, Evaluated, Evaluator, NodeEvidence, Observer, RecipeHead, Root,
    SearchOutcome, SearchSpec, search,
};
use hk_synth::trace::{OutcomeKind, ResolutionKind};
use hk_synth::{
    Control, Evidence, EvidenceSet, GroupId, MetricId, Skeleton, Stage, StopReason, Verdict,
};
use serde_json::{Value, json};

const FS: f64 = 1.0e5;
const CENTER_HZ: f64 = 915.0e6;
const T0_NS: i64 = 1_800_000_000_000_000_000;
const S: i64 = 1_000_000_000;
const LIMIT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------------------------
// The ring
// ---------------------------------------------------------------------------------------------

fn provenance() -> Provenance {
    Provenance {
        device_id: "mock:t859".into(),
        tune: Tune {
            center_hz: CENTER_HZ,
            sample_rate_hz: FS,
            lna_db: 16.0,
            vga_db: 20.0,
            amp_on: false,
            bandwidth_hz: 0.75 * FS,
        },
        overload: false,
        quantisation_limited: false,
        noise_sigma_lsb: None,
        temperature_c: None,
        antenna_port: None,
        bias_tee: BiasTee::Unknown,
        clock_source: ClockSource::Internal,
        clock_locked: true,
        calibration_state_ref: None,
        spur_mask_ref: None,
        timestamp_method: TimestampMethod::Synthetic,
        timestamp_error_budget_ns: None,
        capture_artefacts: Vec::new(),
    }
}

/// A ring holding `[start, end)` on the capture clock, one segment, sample-exact. `end` may move
/// (a live edge), and reads may be held until the test releases them.
struct Ring {
    start_ns: i64,
    end_ns: AtomicU64,
    /// Advance the edge by this much sample time per read of the state (0: frozen).
    advance_ns: i64,
    held: Mutex<bool>,
    release: Condvar,
}

impl Ring {
    fn new(start_ns: i64, end_ns: i64) -> Self {
        Self {
            start_ns,
            end_ns: AtomicU64::new(end_ns as u64),
            advance_ns: 0,
            held: Mutex::new(false),
            release: Condvar::new(),
        }
    }

    fn hold(&self) {
        *self.held.lock().unwrap() = true;
    }

    fn release(&self) {
        *self.held.lock().unwrap() = false;
        self.release.notify_all();
    }

    fn end(&self) -> i64 {
        self.end_ns.load(Ordering::SeqCst) as i64
    }
}

impl RingRead for Ring {
    fn read_ring(
        &self,
        range: TimeRange,
        _band: Option<(f64, f64)>,
    ) -> Result<Vec<ReadChunk>, ClipError> {
        let mut held = self.held.lock().unwrap();
        while *held {
            held = self.release.wait(held).unwrap();
        }
        let (a, b) = (range.start.as_unix_nanos(), range.end.as_unix_nanos());
        if b <= self.start_ns {
            return Err(ClipError::Evicted);
        }
        let (a, b) = (a.max(self.start_ns), b.min(self.end()));
        if b <= a {
            return Err(ClipError::Empty);
        }
        let first = ((a - self.start_ns) as f64 * FS / 1e9).round() as u64;
        let samples = ((b - a) as f64 * FS / 1e9).round() as u64;
        Ok(vec![ReadChunk {
            piece: ClipPiece {
                sample_start: 0,
                samples,
                global_index: first,
                t_ns: a,
                t1_ns: b,
                segment: 0,
                run: 1,
                provenance: provenance(),
                content_class: ContentClass::Unrestricted,
            },
            data: vec![0u8; 2 * samples as usize],
        }])
    }
}

struct Env {
    ring: Arc<Ring>,
    tuned: Option<FreqRange>,
    absent: bool,
}

impl Env {
    fn new(ring: Arc<Ring>) -> Self {
        Self {
            ring,
            tuned: Some(FreqRange::centered(CENTER_HZ, FS)),
            absent: false,
        }
    }
}

impl JobEnv for Env {
    fn ring_state(&self) -> RingState {
        if self.absent {
            return RingState::Absent;
        }
        if self.ring.advance_ns > 0 {
            self.ring
                .end_ns
                .fetch_add(self.ring.advance_ns as u64, Ordering::SeqCst);
        }
        RingState::Buffered(TimeRange::new(
            Timestamp::from_unix_nanos(self.ring.start_ns),
            Timestamp::from_unix_nanos(self.ring.end()),
        ))
    }

    fn ring(&self) -> Option<&dyn RingRead> {
        (!self.absent).then_some(&*self.ring as &dyn RingRead)
    }

    fn tuned(&self) -> Option<FreqRange> {
        self.tuned
    }
}

// ---------------------------------------------------------------------------------------------
// The hidden world and the backend
// ---------------------------------------------------------------------------------------------

/// The truth the engine never sees: FSK at 4800 Bd, NRZI, sync 0x2DD4, CRC poly 0x8005.
struct World;

#[derive(Clone, Copy)]
struct Sig {
    on_truth: bool,
}

fn ev(stage: Stage, metric: MetricId, group: GroupId, n: u32, bits: f32) -> EvidenceSet {
    let mut s = EvidenceSet::new();
    s.push(Evidence::new(stage, metric, group, bits, n, bits))
        .unwrap();
    s
}

impl Evaluator for World {
    type Output = Sig;

    fn evaluate(&self, req: &EvalRequest<'_, Sig>) -> Result<Evaluated<Sig>, EvalError> {
        let upstream = req.parent.is_none_or(|p| p.on_truth);
        let nodes = &req.candidate.recipe.nodes[req.new_nodes.clone()];
        let last = nodes.last().expect("nodes");
        let p = |k: &str| last.params.get(k).cloned().unwrap_or(Value::Null);
        let holdout = req.window == hk_synth::engine::EvalWindow::Holdout;
        let (set, on) = match req.stage {
            Stage::S1 => {
                let hit = upstream && last.block == "fsk_demod";
                let b = if hit { 9.0 } else { 1.0 };
                (
                    ev(
                        Stage::S1,
                        MetricId::Bimodality,
                        GroupId::DemodShape,
                        4096,
                        b,
                    ),
                    hit,
                )
            }
            Stage::S2 => {
                let rate = p("symbol_rate_bd").as_f64().unwrap_or(0.0);
                let err = (rate / 4800.0).ln().abs();
                let b = if upstream {
                    (11.0 * (-(err / 0.01).powi(2)).exp()) as f32
                } else {
                    0.5
                };
                (
                    ev(Stage::S2, MetricId::EyeOpen, GroupId::Eye, 512, b),
                    upstream && err < 0.015,
                )
            }
            Stage::S3 => {
                let hit = upstream && last.block == "nrzi";
                let b = if hit { 8.0 } else { 2.0 };
                (
                    ev(Stage::S3, MetricId::BitStructure, GroupId::BitShape, 512, b),
                    hit,
                )
            }
            Stage::S4 => {
                let hit = upstream && p("sync_word").as_str() == Some("0x2DD4");
                let b = if hit { 24.0 } else { 1.0 };
                (
                    ev(Stage::S4, MetricId::SyncExcess, GroupId::Undeclared, 30, b),
                    hit,
                )
            }
            Stage::S5 => {
                let hit = upstream && p("poly").as_str() == Some("0x8005");
                let frames = match (hit, holdout) {
                    (true, false) => 20,
                    (true, true) => 12,
                    _ => 0,
                };
                (
                    ev(
                        Stage::S5,
                        MetricId::CheckDistinctValid,
                        GroupId::Undeclared,
                        frames,
                        16.0 * frames as f32,
                    ),
                    hit,
                )
            }
            _ => (EvidenceSet::new(), upstream),
        };
        Ok(Evaluated {
            evidence: vec![NodeEvidence {
                node: last.id.clone(),
                evidence: set,
            }],
            output: Sig { on_truth: on },
            output_bytes: 256,
            check: None,
        })
    }
}

fn skeleton(id: &str, s1_block: &str, family: &str) -> Skeleton {
    serde_json::from_value(json!({
        "id": id, "version": 1,
        "slots": {
            "S1": [ { "id": family, "family": family,
                      "nodes": [ { "id": "demod", "block": s1_block } ] } ],
            "S2": [ { "id": "clock", "nodes": [ { "id": "clock", "block": "clock_recovery",
                       "params": { "pulse": "nrz", "algorithm": "gardner" } } ] } ],
            "S3": [ { "id": "none", "nodes": [ { "id": "slice", "block": "slicer" } ] },
                    { "id": "nrzi", "nodes": [ { "id": "slice", "block": "slicer" },
                                                { "id": "line", "block": "nrzi" } ] } ],
            "S4": [ { "id": "sync", "nodes": [ { "id": "sync", "block": "sync_search",
                       "params": { "mode": "sync-word", "sync_bits": 16, "frame_bits": 64 } } ] } ],
            "S5": [ { "id": "crc16", "nodes": [ { "id": "crc", "block": "crc",
                       "params": { "width": 16 } } ] } ],
        }
    }))
    .unwrap()
}

fn root(sk: Skeleton, prior_bits: f32) -> Root {
    Root {
        skeleton: sk,
        template: None,
        free: vec![
            serde_json::from_value(json!({
                "path": "nodes[clock].params.symbol_rate_bd",
                "domain": { "float": { "lo": 300, "hi": 50000, "scale": "log" } },
                "seed": 4700, "source": "estimate" }))
            .unwrap(),
            serde_json::from_value(json!({
                "path": "nodes[sync].params.sync_word",
                "domain": { "hex": { "candidates": ["0x1234", "0x2DD4"] } } }))
            .unwrap(),
            serde_json::from_value(json!({
                "path": "nodes[crc].params.poly",
                "domain": { "hex": { "candidates": ["0x1021", "0x8005"] } } }))
            .unwrap(),
        ],
        prior_bits,
        seed_source: hk_synth::candidate::SeedSource::Open,
        family: None,
        deferred: None,
    }
}

fn spec(budget: hk_synth::search::SynthBudget, profile: Profile) -> SearchSpec {
    let head = RecipeHead {
        input: InputSpec {
            port: PortType::Iq,
            sample_rate_hz: Some(FS),
            bandwidth_hz: Some(20_000.0),
            channels: Default::default(),
        },
        output_policy: OutputPolicy {
            content_class: ContentClass::Unrestricted,
            metadata_keys: None,
            frame_models: Vec::new(),
            identity: None,
        },
        field_maps: BTreeMap::new(),
    };
    let mut s = SearchSpec::new(
        head,
        vec![
            root(skeleton("generic-fsk-framed", "fsk_demod", "fsk"), -0.5),
            root(skeleton("generic-ook-framed", "am_demod", "ook"), -1.0),
        ],
        profile,
    );
    s.budget = budget;
    // Count-bounded, so the run is reproducible (ADR-0021 §5): the wall backstop is not the unit.
    s.budget.wall_s = 600.0;
    s.budget.cpu_s = 600.0;
    s.budget.max_evaluations = Some(4_000);
    s
}

/// Runs the real engine; optionally blocks until cancelled first (to hold a job "running").
#[derive(Default)]
struct Backend {
    calls: AtomicUsize,
    block_until_cancelled: bool,
    entered: AtomicBool,
    saw_samples: AtomicU64,
}

impl SearchBackend for Backend {
    fn search(
        &self,
        input: &SearchInput<'_>,
        control: &Control,
        observer: &mut dyn Observer,
    ) -> Result<SearchOutcome, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.saw_samples
            .store(input.acquisition.samples(), Ordering::SeqCst);
        self.entered.store(true, Ordering::SeqCst);
        if self.block_until_cancelled {
            let t = Instant::now();
            while !control.is_cancelled() {
                assert!(t.elapsed() < LIMIT, "never cancelled");
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        let spec = spec(input.budget, input.request.profile);
        Ok(search(
            &spec,
            &Registry::builtin(),
            &World,
            control,
            observer,
        ))
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

fn band_request(window: Option<(i64, i64)>, profile: Profile) -> JobRequest {
    JobRequest {
        target: json!({ "band": { "f_lo": CENTER_HZ - 10e3, "f_hi": CENTER_HZ + 10e3 } }),
        emitter_id: None,
        band: FreqRange::new(CENTER_HZ - 10e3, CENTER_HZ + 10e3),
        window: window.map(|(a, b)| {
            TimeRange::new(Timestamp::from_unix_nanos(a), Timestamp::from_unix_nanos(b))
        }),
        window_explicit: window.is_some(),
        profile,
        max_wall_s: None,
        source: SourceChoice::Auto,
        live_s: None,
        templates: TemplateFilter::default(),
        attach: true,
    }
}

/// A 1 s window in a 10 s ring.
fn one_second() -> Option<(i64, i64)> {
    Some((T0_NS + 5 * S, T0_NS + 6 * S))
}

fn manager(ring: &Arc<Ring>, backend: Option<Arc<Backend>>) -> AnalyzeJobs {
    AnalyzeJobs::new(
        Arc::new(Env::new(Arc::clone(ring))),
        backend.map(|b| b as Arc<dyn SearchBackend>),
        PowerPolicy::Mains,
    )
}

fn ring10() -> Arc<Ring> {
    Arc::new(Ring::new(T0_NS, T0_NS + 10 * S))
}

/// Waits until `id` has finished and says when (a cancelled job is terminal at once, but its
/// `ended` is set then too, so callers that need the worker's hand-back wait on `end_reason`).
fn wait_terminal(jobs: &AnalyzeJobs, id: &str) -> AnalyzeJob {
    let t = Instant::now();
    loop {
        let j = jobs.get(id).expect("the job exists");
        if j.state.is_terminal() && j.ended.is_some() {
            return j;
        }
        assert!(t.elapsed() < LIMIT, "{id} never finished: {:?}", j.state);
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < LIMIT, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[test]
fn a_job_acquires_what_it_reads_searches_and_streams_to_done() {
    let ring = ring10();
    ring.hold();
    let backend = Arc::new(Backend::default());
    let jobs = manager(&ring, Some(Arc::clone(&backend)));
    let job = jobs
        .start(band_request(one_second(), Profile::Standard))
        .unwrap();
    assert_eq!((job.id.as_str(), job.state), ("a1", JobState::Queued));
    assert_eq!(job.href, "/api/analyze/a1");
    let rx = jobs.subscribe("a1").unwrap();
    ring.release();
    let done = wait_terminal(&jobs, "a1");

    // Coverage is what was read, from the ledger: the 1 s window, 60/40, as two parts.
    let w = done.window.as_ref().expect("window");
    assert_eq!(w.source, "ring");
    assert_eq!(w.samples, FS as u64);
    assert_eq!(backend.saw_samples.load(Ordering::SeqCst), FS as u64);
    assert_eq!((w.bursts, w.bursts_missing), (2, 0));
    assert_eq!(w.t_lo, Some((T0_NS + 5 * S) as f64 / 1e9));
    assert_eq!(w.t_hi, Some((T0_NS + 6 * S) as f64 / 1e9));
    let ch = done.channel.expect("channel");
    assert_eq!((ch.center_hz, ch.bandwidth_hz), (CENTER_HZ, 20e3));
    assert_eq!(ch.sample_rate_hz, Some(FS));

    // The blind search solved the hidden truth; a solved job carries no negative result.
    assert_eq!(done.state, JobState::Done, "{:?}", done.error);
    assert_eq!(done.end_reason, Some(StopReason::Solved));
    let best = &done.results[0];
    assert_eq!(best.verdict, Verdict::Solved);
    assert_eq!(
        best.recipe
            .nodes
            .iter()
            .find(|n| n.id == "crc")
            .unwrap()
            .params["poly"],
        json!("0x8005")
    );
    assert!(done.resolution.is_none());
    assert!(done.used.evaluations > 0);

    // ADR-0021 §4.1: the summary accounts for every decision.
    let ts = done.trace_summary.as_ref().expect("trace_summary");
    assert_eq!(ts.decisions, ts.nodes_recorded + ts.nodes_elided);
    assert!(ts.nodes_recorded > 0);
    assert!(ts.replayable, "a count-bounded job is replayable");
    assert_eq!(
        ts.by_outcome.values().sum::<u64>(),
        ts.decisions,
        "every decision has an outcome"
    );

    // ADR-0021 §4.2: the full trace is a separate, filtered fetch.
    let all = jobs.trace("a1", &TraceQuery::default()).unwrap();
    assert_eq!(all["final"], json!(true));
    assert_eq!(all["engine"], json!(hk_synth::ENGINE));
    let n_all = all["nodes"].as_array().unwrap().len();
    assert_eq!(n_all as u64, ts.nodes_recorded.min(512));
    let s5 = jobs
        .trace(
            "a1",
            &TraceQuery {
                stage: Some(Stage::S5),
                ..TraceQuery::default()
            },
        )
        .unwrap();
    assert!(
        s5["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|n| n["stage"] == json!("S5"))
    );
    let one = jobs
        .trace(
            "a1",
            &TraceQuery {
                limit: Some(1),
                ..TraceQuery::default()
            },
        )
        .unwrap();
    assert_eq!(one["nodes"].as_array().unwrap().len(), 1);
    let tried = jobs
        .trace(
            "a1",
            &TraceQuery {
                tried: Some(true),
                ..TraceQuery::default()
            },
        )
        .unwrap();
    assert!(
        tried["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|n| n["tried"] == json!(true))
    );
    assert_eq!(
        jobs.trace(
            "a1",
            &TraceQuery {
                limit: Some(513),
                ..TraceQuery::default()
            }
        )
        .unwrap_err()
        .status,
        400
    );

    // The stream: a snapshot first, `best` and one `trace` per stage, and `done` last.
    let records: Vec<Value> = rx.iter().collect();
    let kinds: Vec<&str> = records
        .iter()
        .map(|r| r["type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds.first(), Some(&"progress"), "{kinds:?}");
    assert_eq!(kinds.last(), Some(&"done"), "{kinds:?}");
    assert!(kinds.contains(&"best"), "{kinds:?}");
    let traces = kinds.iter().filter(|k| **k == "trace").count();
    assert_eq!(traces, ts.by_stage.len(), "one trace record per stage");
    assert!(records.iter().all(|r| r["job_id"] == json!("a1")));
    let last = records.last().unwrap();
    assert_eq!(last["job"]["state"], json!("done"));
    for r in records.iter().filter(|r| r["type"] == "trace") {
        assert!(r["nodes"].as_array().unwrap().len() <= 8);
    }
    // A late subscriber gets the finished job as one `done` and the stream ends.
    let late: Vec<Value> = jobs.subscribe("a1").unwrap().iter().collect();
    assert_eq!(late.len(), 1);
    assert_eq!(late[0]["type"], json!("done"));
}

#[test]
fn without_an_evaluator_a_job_acquires_then_fails_not_searched_never_unknown() {
    let ring = ring10();
    let jobs = manager(&ring, None);
    jobs.start(band_request(one_second(), Profile::Quick))
        .unwrap();
    let j = wait_terminal(&jobs, "a1");
    assert_eq!(j.state, JobState::Failed);
    assert_eq!(j.error.as_ref().unwrap().code, "no_evaluator");
    assert_eq!(j.end_reason, None);
    // It still says exactly what it would have searched.
    assert_eq!(j.window.as_ref().unwrap().samples, FS as u64);
    let r = j.resolution.expect("a failed job still says what it means");
    assert_eq!(r.kind, ResolutionKind::NotSearched);
    assert!(r.reason.is_none() && r.coverage.is_none());
    assert!(j.results.is_empty() && j.trace_summary.is_none());
    let t = jobs.trace("a1", &TraceQuery::default()).unwrap();
    assert_eq!(t["nodes"], json!([]));
}

#[test]
fn cancel_is_immediate_and_final_for_a_running_job_and_removes_a_queued_one() {
    let ring = ring10();
    let backend = Arc::new(Backend {
        block_until_cancelled: true,
        ..Backend::default()
    });
    let jobs = manager(&ring, Some(Arc::clone(&backend)));
    jobs.start(band_request(one_second(), Profile::Quick))
        .unwrap();
    jobs.start(band_request(one_second(), Profile::Quick))
        .unwrap();
    wait_until("a1 to reach the backend", || {
        backend.entered.load(Ordering::SeqCst)
    });
    assert_eq!(jobs.get("a1").unwrap().state, JobState::Searching);
    assert_eq!(jobs.get("a2").unwrap().state, JobState::Queued);

    // Queued: removed, cancelled, never run.
    let (q, forgotten) = jobs.cancel("a2").unwrap();
    assert!(!forgotten);
    assert_eq!(q.state, JobState::Cancelled);
    assert_eq!(
        q.resolution.as_ref().unwrap().kind,
        ResolutionKind::NotSearched
    );
    // Running: cancelled at once, and it stays cancelled when the engine hands back.
    let (r, forgotten) = jobs.cancel("a1").unwrap();
    assert!(!forgotten);
    assert_eq!(r.state, JobState::Cancelled);
    wait_until("a1's worker to hand back", || {
        jobs.get("a1").unwrap().end_reason.is_some()
    });
    let a1 = jobs.get("a1").unwrap();
    assert_eq!(a1.state, JobState::Cancelled);
    assert_eq!(a1.end_reason, Some(StopReason::Cancelled));
    assert_eq!(
        a1.resolution.as_ref().unwrap().kind,
        ResolutionKind::NotSearched,
        "a cancelled job ruled nothing out"
    );
    // a2 never reached the backend.
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    assert_eq!(jobs.get("a2").unwrap().started, None);

    // Cancelling a finished job forgets it: `410`, not `404`.
    let (_, forgotten) = jobs.cancel("a1").unwrap();
    assert!(forgotten);
    assert_eq!(jobs.get("a1").unwrap_err().status, 410);
    assert_eq!(jobs.get("a1").unwrap_err().code, "gone");
    assert_eq!(
        jobs.trace("a1", &TraceQuery::default()).unwrap_err().status,
        410
    );
    assert_eq!(jobs.get("a99").unwrap_err().status, 404);
    assert_eq!(jobs.get("x1").unwrap_err().status, 404);
}

#[test]
fn admission_is_one_running_four_queued_and_the_power_policy() {
    let ring = ring10();
    let backend = Arc::new(Backend {
        block_until_cancelled: true,
        ..Backend::default()
    });
    let jobs = manager(&ring, Some(Arc::clone(&backend)));
    jobs.start(band_request(one_second(), Profile::Quick))
        .unwrap();
    wait_until("a1 to run", || backend.entered.load(Ordering::SeqCst));
    for _ in 0..4 {
        jobs.start(band_request(one_second(), Profile::Quick))
            .unwrap();
    }
    let busy = jobs
        .start(band_request(one_second(), Profile::Quick))
        .unwrap_err();
    assert_eq!((busy.status, busy.code), (503, "busy"));
    let listed: Vec<String> = jobs.list(None).into_iter().map(|j| j.id).collect();
    assert_eq!(listed, ["a5", "a4", "a3", "a2", "a1"], "newest first");
    assert_eq!(jobs.list(Some(JobState::Queued)).len(), 4);
    for id in ["a5", "a4", "a3", "a2", "a1"] {
        jobs.cancel(id).unwrap();
    }

    let battery = AnalyzeJobs::new(Arc::new(Env::new(ring10())), None, PowerPolicy::Battery);
    let e = battery
        .start(band_request(one_second(), Profile::Deep))
        .unwrap_err();
    assert_eq!((e.status, e.code), (422, "power"));
    let ok = battery
        .start(band_request(one_second(), Profile::Standard))
        .unwrap();
    assert_eq!(ok.budget.threads, 1, "battery halves threads");
    // `max_wall_s` can only lower the wall backstop, and is bounded.
    let mut r = band_request(one_second(), Profile::Standard);
    r.max_wall_s = Some(5.0);
    assert_eq!(battery.start(r.clone()).unwrap().budget.wall_s, 5.0);
    r.max_wall_s = Some(601.0);
    assert_eq!(battery.start(r).unwrap_err().status, 400);
}

#[test]
fn source_errors_are_answered_at_admission() {
    let ring = ring10();
    let jobs = manager(&ring, None);
    // Older than the oldest sample.
    let e = jobs
        .start(band_request(
            Some((T0_NS - 5 * S, T0_NS - 4 * S)),
            Profile::Quick,
        ))
        .unwrap_err();
    assert_eq!((e.status, e.code), (410, "evicted"));
    // Live, with the band outside the tuned window.
    let mut r = band_request(None, Profile::Quick);
    r.band = FreqRange::new(100e6, 100.1e6);
    let e = jobs.start(r).unwrap_err();
    assert_eq!((e.status, e.code), (409, "outside_window"));
    // No ring at all.
    let no_ring = AnalyzeJobs::new(
        Arc::new(Env {
            absent: true,
            ..Env::new(ring10())
        }),
        None,
        PowerPolicy::Mains,
    );
    let e = no_ring
        .start(band_request(None, Profile::Quick))
        .unwrap_err();
    assert_eq!((e.status, e.code), (503, "unavailable"));
    // Nothing was admitted by any of those.
    assert!(jobs.list(None).is_empty());
}

#[test]
fn a_live_job_waits_on_the_capture_clock_not_a_wall_clock() {
    // The edge moves 250 ms of sample time per look, independent of wall time: the job's window is
    // exactly 1 s of *sample* time starting at the edge it saw, whatever the wall clock did.
    let ring = Arc::new(Ring {
        advance_ns: 250_000_000,
        ..Ring::new(T0_NS, T0_NS + 10 * S)
    });
    let jobs = manager(&ring, None);
    let mut r = band_request(None, Profile::Quick);
    r.live_s = Some(1.0);
    r.emitter_id = Some(EmitterId::new());
    let job = jobs.start(r).unwrap();
    assert_eq!(job.live_s, Some(1.0));
    let j = wait_terminal(&jobs, "a1");
    let w = j.window.as_ref().expect("window");
    assert_eq!(w.source, "live");
    let (lo, hi) = (w.t_lo.unwrap(), w.t_hi.unwrap());
    assert!((hi - lo - 1.0).abs() < 1e-6, "{lo}..{hi}");
    assert!(
        lo >= (T0_NS + 10 * S) as f64 / 1e9,
        "the window starts at the live edge"
    );
    assert_eq!(w.samples, FS as u64);
    assert_eq!(j.emitter_id, job.emitter_id);
}

#[test]
fn a_long_window_keeps_its_newest_two_seconds_and_says_so() {
    let ring = ring10();
    let jobs = manager(&ring, None);
    jobs.start(band_request(
        Some((T0_NS + S, T0_NS + 9 * S)),
        Profile::Quick,
    ))
    .unwrap();
    let j = wait_terminal(&jobs, "a1");
    let w = j.window.as_ref().unwrap();
    assert_eq!(w.samples, 2 * FS as u64);
    assert_eq!(w.t_lo, Some((T0_NS + 7 * S) as f64 / 1e9));
    assert_eq!(j.warnings.len(), 1, "{:?}", j.warnings);
}

#[test]
fn only_the_last_fifty_finished_jobs_are_kept_and_the_rest_answer_gone() {
    let ring = ring10();
    let jobs = manager(&ring, None);
    for i in 1..=51 {
        jobs.start(band_request(one_second(), Profile::Quick))
            .unwrap();
        wait_terminal(&jobs, &format!("a{i}"));
    }
    // The worker forgets the oldest as it hands the newest back.
    wait_until("the fifty-job window", || jobs.list(None).len() == 50);
    assert_eq!(jobs.get("a1").unwrap_err().status, 410);
    assert_eq!(jobs.get("a2").unwrap().state, JobState::Failed);
    assert_eq!(jobs.get("a52").unwrap_err().status, 404);
    // `outcome` filters parse from the wire names (ADR-0021 §2.2's closed enum).
    assert_eq!(
        hk_pipeline::synth::jobs::parse_name::<OutcomeKind>("pruned_floor"),
        Some(OutcomeKind::PrunedFloor)
    );
    assert_eq!(
        hk_pipeline::synth::jobs::parse_name::<Stage>("S3"),
        Some(Stage::S3)
    );
    assert_eq!(hk_pipeline::synth::jobs::parse_name::<Stage>("S9"), None);
}
