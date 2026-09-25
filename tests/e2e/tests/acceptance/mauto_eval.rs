//! **ADR-0015 §7's blind evaluation** (T-863 = MAUTO M-12) — `RESEARCH-002`, `SIGNAL-052`.
//!
//! ```text
//! cargo nextest run -p hk-e2e -E 'binary(acceptance_mauto) & test(/mauto_eval/)'
//! ```
//!
//! # What runs
//!
//! ADR-0015 §7's **generic FSK/OOK sweep** and its **partial quality for unknowns** clause, over
//! the population `hkpy.synth generic_fsk_sweep` draws (300 Bd–50 kBd, a random 16–32-bit sync,
//! a RevEng-catalogue CRC-8/16 — or, for the partial clause, an off-catalogue random polynomial
//! or no check — SNR 6/10/20 dB in the emission's own bandwidth, CFO within ±0.2 × bandwidth).
//! Each scene is replayed **through the mock SDR**, truth stripped, into the whole pipeline with
//! the IQ ring on; the inventory it produced is read over HTTP, and every emitter gets
//! `POST /api/analyze {emitter_id, templates: {off: true}}` and `{…, templates: {off: false}}` —
//! region-analyze jobs, polled to the end on the served `AnalyzeJob` (docs/api.md). Targets are
//! whatever blind detection produced; truth (`hackriff:truth`) is read only by [`judge`], after.
//!
//! # The thresholds, fixed a priori
//!
//! ADR-0015 §7's numbers, as constants, before any engine answered: templates off, **solved ≥
//! [`SOLVED_AT_20DB`] at 20 dB, ≥ [`SOLVED_AT_10DB`] at 10 dB, ≥ [`FRAMED_AT_6DB`] at least
//! `framed` at 6 dB**; partial quality: the verdict equals truth's `deepest_achievable` in ≥
//! [`PARTIAL_MATCH_MIN`] and is **above** it in ≤ [`PARTIAL_OVERCLAIM_MAX`]; and **whenever** a
//! verdict is ≥ `framed` the symbol rate is within [`RATE_TOL`] and the sync word exact. A
//! verdict "counts" only with those parameters right: a `solved` at the wrong rate is not a solve.
//! §7 says the M-12 review may tighten these, never loosen them after seeing results.
//!
//! `deepest_achievable` is the generator's a-priori statement (`py/hkpy/synth/generic_fsk.py`):
//! `solved` needs a check and ≥ 3 whole frames in the hold-out; otherwise `framed`.
//!
//! # What the product can be asked today — stated, not papered over
//!
//! **There is no production search backend.** `hk_pipeline::synth::jobs::server_backend()` —
//! the one function `hk serve` and this suite both read — is `None`: nothing yet implements the
//! engine's `Evaluator` over acquired IQ. So on this build every job **acquires its window for
//! real and ends `failed / no_evaluator`** (`not-searched`, ADR-0021 §7A.4), and **the §7 rates
//! are NOT MEASURED** — not passed, not failed. The suite is **armed** on the product's own
//! declaration, not on the answers:
//!
//! - while `server_backend()` is `None`, the product test asserts exactly that declared state —
//!   every emitter's job acquired IQ and ended `no_evaluator`, and nothing claimed a verdict — and
//!   prints the table with the rates marked unmeasured. Any other failure (a refused `POST`, an
//!   empty acquisition, no emitter over the emission) is a plumbing defect and fails now;
//! - the moment `server_backend()` returns a backend, **every threshold above binds**, with no
//!   edit here; a job that then fails counts as not solved.
//!
//! The coverage manifest marks the cells these rows run as populated (the jobs ran through the
//! product) and the report says, per cell, that nothing was searched. Read both before a rate.
//!
//! **Not run here, and where it lives:** §7's protocol rows (RDS, POCSAG, ACARS, ADS-B, the 915
//! MHz sensor) need content-level judges (PI/PS against `hk_demod::rds`, ≥ 95 % pages, CRC-valid
//! frames, 14/16 squitters) that mean nothing while no job searches; they are listed in
//! [`PROTOCOL_ROWS`] with their must-rules so the review has them in one place. The negatives
//! (noise, CW, FM voice, OFDM → 0 solved / 0 confirms / 0 emitters) are
//! [`crate::mauto_negatives`] (T-568) and the 200-window `deep` noise run is T-576's.
//!
//! # The suite can fail, and can pass
//!
//! [`judge`] and [`breaches`] are pure. Stubs stand in for the product over hand-built truths:
//! an **oracle** that answers truth passes every threshold; a **silent** engine and a
//! **wrong-parameter** engine fail the rates; an **over-claimer** breaks the partial-quality
//! over-claim bound. Those are the tests that prove the numbers above can be met and can be
//! missed — without them a green product run would mean nothing.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hk_e2e::SynthRequest;
use hk_e2e::corpus::{Cell, Row, SeedPlan};
use hk_pipeline::synth::jobs::{
    AnalyzeJobs, PowerPolicy, RepoAttacher, RingJobEnv, server_backend,
};
use hk_store::iqbuffer::IqBufferConfig;
use serde_json::{Value, json};

use crate::blind::{BlindSource, blind_config, start};
use crate::common::*;
use crate::signal_087::api_post;

const USE_CASES: &str = "RESEARCH-002 / SIGNAL-052 (T-863, ADR-0015 §7)";

// ------------------------------------------------------------------------------------------
// ADR-0015 §7's thresholds — fixed before any engine answered.
// ------------------------------------------------------------------------------------------

/// Templates off, 20 dB: share `solved` (with the right rate and sync).
pub const SOLVED_AT_20DB: f64 = 0.80;
/// Templates off, 10 dB: share `solved`.
pub const SOLVED_AT_10DB: f64 = 0.60;
/// Templates off, 6 dB: share at least `framed`.
pub const FRAMED_AT_6DB: f64 = 0.50;
/// Partial quality: share whose verdict equals truth's deepest achievable stage.
pub const PARTIAL_MATCH_MIN: f64 = 0.80;
/// Partial quality: share whose verdict is above it (over-claim).
pub const PARTIAL_OVERCLAIM_MAX: f64 = 0.01;
/// Whenever a verdict is ≥ `framed`: symbol rate within this fraction of truth.
pub const RATE_TOL: f64 = 0.01;

/// Seeds per population at CI tier, before the hold-out seals a fifth. A guard, not a rate
/// measurement: the rates need docs/22's n, which is a nightly/M-12-review run.
pub const CI_SEEDS: std::ops::RangeInclusive<u64> = 1..=2;
/// The profile every job asks for.
pub const PROFILE: &str = "standard";
/// The test that populates these rows.
pub const PRODUCT_TEST: &str =
    "mauto_eval::section_7_generic_fsk_sweep_through_the_mock_sdr_and_the_analyze_api";

const PRODUCT_TESTS: &[&str] = &[PRODUCT_TEST];

/// ADR-0015 §3.4's verdict ladder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    /// Energy only.
    Energy,
    /// A demodulator fits.
    Demodulated,
    /// A symbol clock.
    Clocked,
    /// A frame boundary (sync word).
    Framed,
    /// A check verified.
    Checked,
    /// Solved on hold-out.
    Solved,
}

impl Verdict {
    /// Parses the wire name.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "energy" => Verdict::Energy,
            "demodulated" => Verdict::Demodulated,
            "clocked" => Verdict::Clocked,
            "framed" => Verdict::Framed,
            "checked" => Verdict::Checked,
            "solved" => Verdict::Solved,
            _ => return None,
        })
    }
}

// ------------------------------------------------------------------------------------------
// The populations.
// ------------------------------------------------------------------------------------------

/// Which §7 clause a population serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Clause {
    /// The sweep, at this SNR (dB).
    Sweep(u8),
    /// Partial quality for unknowns.
    Partial,
}

/// One generated population.
#[derive(Clone, Copy, Debug)]
pub struct Population {
    /// Manifest row id.
    pub id: &'static str,
    /// The clause it is judged under.
    pub clause: Clause,
    /// `modulation` (`2fsk` | `ook`).
    pub modulation: &'static str,
    /// The manifest's A1 level.
    pub family: &'static str,
    /// `check` (`catalogue` | `random-poly` | `absent`).
    pub check: &'static str,
    /// The manifest's A7 level.
    pub a7: &'static str,
    /// SNR, dB.
    pub snr_db: u8,
    /// The manifest's A2 level.
    pub a2: &'static str,
}

/// The populations, in report order.
pub const POPULATIONS: &[Population] = &[
    Population {
        id: "S7-2fsk-20dB",
        clause: Clause::Sweep(20),
        modulation: "2fsk",
        family: "2fsk",
        check: "catalogue",
        a7: "crc-searched",
        snr_db: 20,
        a2: "20dB",
    },
    Population {
        id: "S7-ook-20dB",
        clause: Clause::Sweep(20),
        modulation: "ook",
        family: "ook-ask",
        check: "catalogue",
        a7: "crc-searched",
        snr_db: 20,
        a2: "20dB",
    },
    Population {
        id: "S7-2fsk-10dB",
        clause: Clause::Sweep(10),
        modulation: "2fsk",
        family: "2fsk",
        check: "catalogue",
        a7: "crc-searched",
        snr_db: 10,
        a2: "10dB",
    },
    Population {
        id: "S7-ook-10dB",
        clause: Clause::Sweep(10),
        modulation: "ook",
        family: "ook-ask",
        check: "catalogue",
        a7: "crc-searched",
        snr_db: 10,
        a2: "10dB",
    },
    Population {
        id: "S7-2fsk-6dB",
        clause: Clause::Sweep(6),
        modulation: "2fsk",
        family: "2fsk",
        check: "catalogue",
        a7: "crc-searched",
        snr_db: 6,
        a2: "6dB",
    },
    Population {
        id: "S7-ook-6dB",
        clause: Clause::Sweep(6),
        modulation: "ook",
        family: "ook-ask",
        check: "catalogue",
        a7: "crc-searched",
        snr_db: 6,
        a2: "6dB",
    },
    Population {
        id: "S7-partial-random-poly",
        clause: Clause::Partial,
        modulation: "2fsk",
        family: "2fsk",
        check: "random-poly",
        a7: "crc-random-poly",
        snr_db: 20,
        a2: "20dB",
    },
    Population {
        id: "S7-partial-no-check",
        clause: Clause::Partial,
        modulation: "2fsk",
        family: "2fsk",
        check: "absent",
        a7: "none",
        snr_db: 20,
        a2: "20dB",
    },
];

/// The generator seed for a population's plan seed: one draw group per (family, check), so the
/// three SNR levels of a family see the **same** draws (a paired comparison: a rate difference
/// between SNR levels is the SNR's) while families and check kinds see different ones.
pub fn gen_seed(p: &Population, seed: u64) -> u64 {
    let group = match (p.modulation, p.check) {
        ("2fsk", "catalogue") => 0,
        ("ook", "catalogue") => 1,
        (_, "random-poly") => 2,
        _ => 3,
    };
    seed * 16 + group
}

/// The hold-out's scene id for a population.
pub fn scene_id(p: &Population) -> String {
    format!("mauto/section7/{}", p.id)
}

/// ADR-0015 §7's protocol rows, for the review: `(fixture, templates on must, templates off
/// must)`. Not run here (module docs).
pub const PROTOCOL_ROWS: &[(&str, &str, &str)] = &[
    (
        "RDS, fm_100p8M (real)",
        "solved, rank 1 rds, PI/PS equal the hk_demod::rds oracle, confirmed, standard",
        "framed or better: 26-bit linear-block period and offset words found",
    ),
    (
        "POCSAG, T-095 synthetic 4-channel net (per channel)",
        "solved, >= 95 % of truth pages on hold-out, confirmed",
        "solved: sync 0x7CD215D8 + BCH(31,21) recovered",
    ),
    (
        "ACARS, T-098 synthetic",
        "solved, CRC valid on hold-out",
        "clocked or better",
    ),
    (
        "ADS-B, SIGNAL-001 squitter scene (burst path)",
        ">= 14/16 single squitters solved; burst set confirmed",
        "burst set: CRC-24 recovered from >= 8 frames, solved",
    ),
    (
        "915 MHz FSK sensor (T-078 scene)",
        "n/a (no template)",
        "solved or checked; symbol rate within 1 %",
    ),
];

/// The manifest rows these populations fill (T-627's coverage manifest).
pub fn manifest_rows() -> Vec<Row> {
    POPULATIONS
        .iter()
        .map(|p| {
            let a5 = if p.check == "random-poly" {
                "out-of-catalogue"
            } else {
                "in-catalogue-searched"
            };
            Row {
                id: p.id,
                tests: PRODUCT_TESTS,
                seeds: SeedPlan::new(&scene_id(p), CI_SEEDS),
                fixed_seeds: None,
                cells: vec![
                    Cell::new(
                        "A1xA2xA4",
                        &[("A1", p.family), ("A2", p.a2), ("A4", "burst")],
                    ),
                    Cell::new("A1xA5", &[("A1", p.family), ("A5", a5)]),
                    Cell::new("A1xA7", &[("A1", p.family), ("A7", p.a7)]),
                    Cell::new("A1xA8", &[("A1", p.family), ("A8", "off")]),
                    Cell::new("A1xA8", &[("A1", p.family), ("A8", "on")]),
                ],
            }
        })
        .collect()
}

/// One scene to run: its population, seed, recording and sealed truth (`generic_fsk`).
#[derive(Clone, Debug)]
pub struct Case {
    /// Index into [`POPULATIONS`].
    pub pop: usize,
    /// Seed.
    pub seed: u64,
    /// The recording (`None` for a stub case).
    pub meta: Option<std::path::PathBuf>,
    /// The scene's `generic_fsk` truth.
    pub truth: Value,
}

// ------------------------------------------------------------------------------------------
// Answers.
// ------------------------------------------------------------------------------------------

/// What one served `AnalyzeJob` said, reduced to what §7 judges.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct JobAnswer {
    /// `done` · `cancelled` · `failed` · `refused:<status>/<code>` (the `POST` was not 202).
    pub state: String,
    /// `error.code` when it failed.
    pub error: Option<String>,
    /// Samples the acquisition read (`window.samples`).
    pub samples: u64,
    /// Rank-1 verdict, else the resolution's deepest verdict; `None` when not searched.
    pub verdict: Option<Verdict>,
    /// A symbol rate bound in the rank-1 recipe.
    pub symbol_rate_bd: Option<f64>,
    /// The rank-1 recipe's sync word, as a bit string (MSB first).
    pub sync: Option<String>,
    /// `used.wall_s`.
    pub wall_s: f64,
    /// `used.cpu_s`.
    pub cpu_s: f64,
}

impl JobAnswer {
    /// Whether a search ran (ADR-0021 §7A.4: failed and cancelled jobs ruled nothing out).
    pub fn searched(&self) -> bool {
        self.state == "done"
    }
}

/// The sync word a recipe binds, as bits: `sync_search`'s `sync_word` (hex, first bit on air =
/// MSB) over `sync_bits`.
fn recipe_sync(recipe: &Value) -> Option<String> {
    recipe["nodes"].as_array()?.iter().find_map(|n| {
        if n["block"].as_str()? != "sync_search" {
            return None;
        }
        let bits = n["params"]["sync_bits"].as_u64()? as usize;
        let hex = n["params"]["sync_word"].as_str()?;
        let v = u64::from_str_radix(hex.trim_start_matches("0x"), 16).ok()?;
        (1..=64).contains(&bits).then(|| format!("{v:0bits$b}"))
    })
}

/// The first symbol rate a recipe binds (`symbol_rate_bd`, or any `…baud…` parameter).
fn recipe_rate(recipe: &Value) -> Option<f64> {
    recipe["nodes"].as_array()?.iter().find_map(|n| {
        n["params"].as_object()?.iter().find_map(|(k, v)| {
            (k.contains("symbol_rate") || k.contains("baud")).then(|| v.as_f64())?
        })
    })
}

/// Reads one served `AnalyzeJob` (pure; docs/api.md's shape).
pub fn read_job(job: &Value) -> JobAnswer {
    let state = job["state"].as_str().unwrap_or("?").to_owned();
    let top = job["results"].as_array().and_then(|r| r.first());
    let verdict = if state == "done" {
        top.and_then(|r| r["verdict"].as_str())
            .or_else(|| job["resolution"]["deepest_verdict"].as_str())
            .and_then(Verdict::parse)
            .or(Some(Verdict::Energy))
    } else {
        None
    };
    JobAnswer {
        state,
        error: job["error"]["code"].as_str().map(str::to_owned),
        samples: job["window"]["samples"].as_u64().unwrap_or(0),
        verdict,
        symbol_rate_bd: top.and_then(|r| recipe_rate(&r["recipe"])),
        sync: top.and_then(|r| recipe_sync(&r["recipe"])),
        wall_s: job["used"]["wall_s"].as_f64().unwrap_or(0.0),
        cpu_s: job["used"]["cpu_s"].as_f64().unwrap_or(0.0),
    }
}

/// One inventory emitter and its two jobs.
#[derive(Clone, Debug)]
pub struct EmitterAnswer {
    /// Emitter id.
    pub id: String,
    /// Measured band, Hz.
    pub f_lo_hz: f64,
    /// Measured band, Hz.
    pub f_hi_hz: f64,
    /// `templates: {off: true}` — open search, what §7's sweep judges.
    pub off: JobAnswer,
    /// `templates: {off: false}` — the product; reported.
    pub on: JobAnswer,
}

/// What an engine made of one case.
#[derive(Clone, Debug, Default)]
pub struct Answer {
    /// Every emitter the run produced.
    pub emitters: Vec<EmitterAnswer>,
}

/// The product or a stub.
pub trait Engine: Sync {
    /// For the report.
    fn name(&self) -> &'static str;
    /// Runs one case.
    fn run(&self, case: &Case) -> Answer;
}

// ------------------------------------------------------------------------------------------
// The product.
// ------------------------------------------------------------------------------------------

/// **The product**: the case's recording, truth stripped, through the mock SDR and the whole
/// pipeline with the IQ ring on, then the API `hk serve` exposes — inventory, IQ ring and the
/// analyze job manager over [`server_backend`] — asked about every emitter it produced.
pub struct Product;

/// The API `hk serve` exposes over a finished run's data directory **with the analyze job manager
/// on** — inventory, bookmarks, audit, the IQ ring and `AnalyzeJobs` over [`server_backend`], with
/// the shipped `SynthesizedConfirm` as the attacher's confirm rule. Extracted so the false-confirm
/// suite (T-576) drives the same server this one does, rather than a second copy of it.
pub fn serve_with_analyze(
    dir: &std::path::Path,
    ring: Arc<hk_pipeline::iqbuffer::IqBufferService>,
) -> hk_api::Server {
    let jobs = Arc::new(AnalyzeJobs::with_attacher(
        Arc::new(RingJobEnv::new(
            Arc::clone(&ring),
            Box::new(|| None::<hk_model::FreqRange>),
        )),
        server_backend(),
        PowerPolicy::Mains,
        Some(Arc::new(RepoAttacher::new(
            dir.join("hackriff.db"),
            hk_pipeline::inventory::SynthesizedConfirm::default(),
        ))),
    ));
    let state = hk_api::ApiState {
        inventory: Some(Arc::new(std::sync::Mutex::new(repo(dir)))),
        bookmarks: Some(Arc::new(std::sync::Mutex::new(repo(dir)))),
        audit: Some(Arc::new(
            hk_api::AuditLog::open(&dir.join("audit.jsonl")).unwrap(),
        )),
        iq_buffer: Some(Arc::new(hk_cli::control::PipelineIqBuffer(Arc::clone(
            &ring,
        )))),
        analyze: Some(Arc::new(hk_cli::control::PipelineAnalyze(jobs))),
        ..hk_api::ApiState::default()
    };
    hk_api::Server::start(
        hk_api::ServerConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            hk_api::Token::from_config(API_TOKEN).unwrap(),
        ),
        state,
    )
    .unwrap()
}

/// Polls a job to a terminal state. The bound is a hang guard derived from the job's own wall
/// backstop, never a choice of what to assert.
pub fn wait_job(addr: std::net::SocketAddr, job: &Value) -> Value {
    let id = job["id"].as_str().expect("a job has an id").to_owned();
    let backstop = job["budget"]["wall_s"].as_f64().unwrap_or(60.0);
    let guard = Duration::from_secs_f64(backstop * 2.0 + 60.0);
    let t0 = Instant::now();
    loop {
        let (status, body) = api_get(addr, &format!("/api/analyze/{id}"));
        assert_eq!(
            status,
            200,
            "GET /api/analyze/{id}: {}",
            String::from_utf8_lossy(&body)
        );
        let v: Value = serde_json::from_slice(&body).unwrap();
        let v = if v.get("job").is_some() {
            v["job"].clone()
        } else {
            v
        };
        if matches!(v["state"].as_str(), Some("done" | "cancelled" | "failed")) {
            return v;
        }
        assert!(
            t0.elapsed() < guard,
            "[{USE_CASES}] job {id} never finished (state {:?}) within its backstop x2 + 60 s",
            v["state"]
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A job's target: an inventory emitter, or a group of burst detections no emitter covers.
enum Target {
    Emitter(String),
    Band {
        f_lo: f64,
        f_hi: f64,
        t_lo: f64,
        t_hi: f64,
    },
}

fn analyze(addr: std::net::SocketAddr, target: &Target, off: bool) -> JobAnswer {
    let mut body = json!({ "templates": { "off": off }, "profile": PROFILE });
    match target {
        Target::Emitter(id) => body["emitter_id"] = json!(id),
        Target::Band {
            f_lo,
            f_hi,
            t_lo,
            t_hi,
        } => {
            body["band"] = json!({ "f_lo": f_lo, "f_hi": f_hi, "t_lo": t_lo, "t_hi": t_hi });
        }
    }
    let (status, raw) = api_post(addr, "/api/analyze", &body.to_string());
    let v: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
    if status != 202 {
        return JobAnswer {
            state: format!(
                "refused:{status}/{}",
                v["error"]["code"].as_str().unwrap_or("?")
            ),
            ..JobAnswer::default()
        };
    }
    read_job(&wait_job(addr, &v["job"]))
}

impl Engine for Product {
    fn name(&self) -> &'static str {
        "product (mock SDR, blind, /api/analyze)"
    }

    fn run(&self, case: &Case) -> Answer {
        let meta = case.meta.as_ref().expect("a product case has a recording");
        let tag = format!("s7-{}-{}", POPULATIONS[case.pop].id, case.seed);
        let mut cfg = blind_config(meta, &tag, BlindSource::default(), json!({}));
        // "Through the IQ ring" (as t254): a lossless replay leaves the ring off, and a region
        // job reads only the ring. Sized for what a 2 s scene holds. Configuration, not truth.
        cfg.cfg.iq_buffer = IqBufferConfig {
            enabled: Some(true),
            retention_s: 600.0,
            max_bytes: Some(64 << 20),
            min_free_bytes: Some(0),
            ..IqBufferConfig::default()
        };
        let dir = cfg.dir;
        let handle = start(cfg.cfg, cfg.replay);
        let ring = handle.iq_buffer();
        assert!(
            ring.wait_allocated(Duration::from_secs(120)),
            "[{USE_CASES}] the IQ ring did not finish opening"
        );
        let _ = finish(handle);
        let server = serve_with_analyze(&dir.0, Arc::clone(&ring));
        let addr = server.local_addr();
        let (status, body) = api_get(addr, "/api/inventory?limit=500");
        assert_eq!(
            status,
            200,
            "/api/inventory: {}",
            String::from_utf8_lossy(&body)
        );
        let v: Value = serde_json::from_slice(&body).unwrap();
        let mut targets: Vec<(String, f64, f64, Target)> = v["entries"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|r| {
                let id = r["id"].as_str().expect("a row has an id").to_owned();
                let f = r["f_center_hz"].as_f64().unwrap_or(f64::NAN);
                let bw = r["bandwidth_hz"].as_f64().unwrap_or(0.0);
                (id.clone(), f - bw / 2.0, f + bw / 2.0, Target::Emitter(id))
            })
            .collect();
        // ADR-0015 §7: a target is "an inventory emitter or burst detection the run found". A
        // short-burst emitter the inventory never formed (measured in T-863: 4.5 ms bursts at
        // 25.7 kBd left 2 detections and no emitter) is still the run's own finding, so every
        // group of detections no emitter covers becomes a band target over its own time span.
        let dets = repo(&dir.0)
            .detections_in_region(&hk_model::Region::new(
                hk_model::FreqRange::new(0.0, 7.0e9),
                ever(),
            ))
            .unwrap_or_default();
        let mut groups: Vec<[f64; 4]> = Vec::new();
        for d in &dets {
            let covered = targets.iter().any(|t| (t.1..=t.2).contains(&d.f_center_hz));
            if covered {
                continue;
            }
            let half = d.obw_hz.max(1_000.0) / 2.0;
            let g = [
                d.f_center_hz - half,
                d.f_center_hz + half,
                d.time.start.as_unix_nanos() as f64 * 1e-9,
                d.time.end.as_unix_nanos() as f64 * 1e-9,
            ];
            match groups.iter_mut().find(|x| x[0] <= g[1] && x[1] >= g[0]) {
                Some(x) => {
                    x[0] = x[0].min(g[0]);
                    x[1] = x[1].max(g[1]);
                    x[2] = x[2].min(g[2]);
                    x[3] = x[3].max(g[3]);
                }
                None => groups.push(g),
            }
        }
        for g in groups {
            targets.push((
                format!("detections@{:.0}-{:.0}Hz", g[0], g[1]),
                g[0],
                g[1],
                Target::Band {
                    f_lo: g[0],
                    f_hi: g[1],
                    t_lo: g[2],
                    t_hi: g[3].max(g[2] + 1e-3),
                },
            ));
        }
        let emitters = targets
            .into_iter()
            .map(|(id, f_lo_hz, f_hi_hz, t)| EmitterAnswer {
                off: analyze(addr, &t, true),
                on: analyze(addr, &t, false),
                id,
                f_lo_hz,
                f_hi_hz,
            })
            .collect();
        Answer { emitters }
    }
}

/// Generates every population's open seeds. `None` when synthesis is unavailable (the suite-wide
/// skip rule).
fn product_cases() -> Option<Vec<Case>> {
    let mut cases = Vec::new();
    for (i, p) in POPULATIONS.iter().enumerate() {
        for seed in SeedPlan::new(&scene_id(p), CI_SEEDS).runnable() {
            let req = SynthRequest::new("generic_fsk_sweep")
                .seed(gen_seed(p, seed))
                .param("modulation", p.modulation)
                .param("check", p.check)
                .param("snr_db", p.snr_db);
            let out = match req.generate() {
                Ok(out) => out,
                Err(err) if err.is_unavailable() && !hk_e2e::synth::require_synth() => {
                    eprintln!("SKIP {USE_CASES}: {err}");
                    return None;
                }
                Err(err) => panic!("{} seed {seed}: generation failed: {err}", p.id),
            };
            let fx = out.fixture(0).unwrap();
            let truth = fx
                .scenario()
                .and_then(|s| s.get("generic_fsk"))
                .cloned()
                .expect("a generic_fsk_sweep scene carries its generic_fsk truth");
            cases.push(Case {
                pop: i,
                seed,
                meta: Some(fx.meta_path.clone()),
                truth,
            });
        }
    }
    Some(cases)
}

/// Runs every case, a few at a time (each is a whole pipeline run).
fn run_all(engine: &dyn Engine, cases: &[Case]) -> Vec<Answer> {
    const PARALLEL: usize = 3;
    let mut out = Vec::with_capacity(cases.len());
    for chunk in cases.chunks(PARALLEL) {
        let answers: Vec<Answer> = std::thread::scope(|s| {
            let hs: Vec<_> = chunk.iter().map(|c| s.spawn(|| engine.run(c))).collect();
            hs.into_iter()
                .map(|h| h.join().expect("a case panicked"))
                .collect()
        });
        out.extend(answers);
    }
    out
}

// ------------------------------------------------------------------------------------------
// The judge.
// ------------------------------------------------------------------------------------------

/// One case, judged.
#[derive(Clone, Debug)]
pub struct Judged {
    /// Index into [`POPULATIONS`].
    pub pop: usize,
    /// Seed.
    pub seed: u64,
    /// Emitters over the emission.
    pub over: usize,
    /// Whether any of them was searched (templates off).
    pub searched: bool,
    /// The best templates-off verdict over the emission.
    pub verdict: Option<Verdict>,
    /// The same, templates on (reported).
    pub verdict_on: Option<Verdict>,
    /// Rate within [`RATE_TOL`] and sync exact, for that best answer.
    pub params_ok: bool,
    /// Truth's deepest achievable stage.
    pub achievable: Verdict,
    /// Labels (≥ `framed`) on emitters over nothing.
    pub false_labels: usize,
    /// Why this case breaks a rule that binds per case, with its evidence.
    pub violations: Vec<String>,
    /// The failures that were not a search (`no_evaluator`, refusals), for the unarmed check.
    pub failures: Vec<String>,
    /// Least samples any job acquired.
    pub min_samples: u64,
    /// Wall, CPU of the best answer.
    pub wall_s: f64,
    /// CPU.
    pub cpu_s: f64,
}

impl Judged {
    /// `solved` with the right parameters.
    pub fn solved(&self) -> bool {
        self.verdict == Some(Verdict::Solved) && self.params_ok
    }

    /// At least `framed` with the right parameters.
    pub fn framed(&self) -> bool {
        self.verdict.is_some_and(|v| v >= Verdict::Framed) && self.params_ok
    }
}

/// Judges one case against its sealed truth (pure).
pub fn judge(case: &Case, answer: &Answer) -> Judged {
    let t = &case.truth;
    let f = t["rf_center_hz"].as_f64().expect("truth rf_center_hz");
    let bw = t["bandwidth_hz"].as_f64().expect("truth bandwidth_hz");
    let (lo, hi) = (f - bw / 2.0, f + bw / 2.0);
    let rate = t["symbol_rate_bd"].as_f64().expect("truth symbol_rate_bd");
    let sync = t["sync_bitstring"].as_str().expect("truth sync_bitstring");
    let achievable = t["deepest_achievable"]
        .as_str()
        .and_then(Verdict::parse)
        .expect("truth deepest_achievable");
    let params_ok = |a: &JobAnswer| {
        a.symbol_rate_bd
            .is_some_and(|r| ((r - rate) / rate).abs() <= RATE_TOL)
            && a.sync.as_deref() == Some(sync)
    };
    let overlaps = |e: &EmitterAnswer| e.f_lo_hz <= hi && e.f_hi_hz >= lo;
    let mut j = Judged {
        pop: case.pop,
        seed: case.seed,
        over: 0,
        searched: false,
        verdict: None,
        verdict_on: None,
        params_ok: false,
        achievable,
        false_labels: 0,
        violations: Vec::new(),
        failures: Vec::new(),
        min_samples: u64::MAX,
        wall_s: 0.0,
        cpu_s: 0.0,
    };
    let mut best: Option<&JobAnswer> = None;
    for e in &answer.emitters {
        for (which, a) in [("off", &e.off), ("on", &e.on)] {
            j.min_samples = j.min_samples.min(a.samples);
            if !a.searched() {
                j.failures.push(format!(
                    "{} templates {which}: {} ({})",
                    e.id,
                    a.state,
                    a.error.as_deref().unwrap_or("-")
                ));
            }
            let label = a.verdict.is_some_and(|v| v >= Verdict::Framed);
            if !overlaps(e) {
                if label {
                    j.false_labels += 1;
                    j.violations.push(format!(
                        "{} ({:.0}–{:.0} Hz) is over no emission and templates {which} labelled it {:?}",
                        e.id, e.f_lo_hz, e.f_hi_hz, a.verdict
                    ));
                }
                continue;
            }
            if label && !params_ok(a) {
                j.violations.push(format!(
                    "{} templates {which}: {:?} at rate {:?} Bd / sync {:?}, truth {rate:.1} Bd / {sync} \
                     (ADR-0015 §7: whenever >= framed, rate within 1 % and the sync exact)",
                    e.id, a.verdict, a.symbol_rate_bd, a.sync
                ));
            }
            if which == "on" {
                j.verdict_on = j.verdict_on.max(a.verdict);
                continue;
            }
            j.searched |= a.searched();
            let better = match best {
                None => true,
                Some(b) => (a.verdict, params_ok(a)) > (b.verdict, params_ok(b)),
            };
            if better {
                best = Some(a);
            }
        }
        j.over += usize::from(overlaps(e));
    }
    if let Some(b) = best {
        j.verdict = b.verdict;
        j.params_ok = params_ok(b);
        j.wall_s = b.wall_s;
        j.cpu_s = b.cpu_s;
    }
    if j.min_samples == u64::MAX {
        j.min_samples = 0;
    }
    j
}

/// Per population.
#[derive(Clone, Debug, Default)]
pub struct Tally {
    /// Cases.
    pub n: usize,
    /// Cases where some emitter lay over the emission.
    pub detected: usize,
    /// Cases searched.
    pub searched: usize,
    /// `solved` with the right parameters.
    pub solved: usize,
    /// ≥ `framed` with the right parameters.
    pub framed: usize,
    /// Verdict == deepest achievable.
    pub matched: usize,
    /// Verdict above it.
    pub over_claim: usize,
    /// Per-case violations.
    pub violations: Vec<String>,
}

/// Tallies judged cases per population.
pub fn tally(judged: &[Judged]) -> BTreeMap<usize, Tally> {
    let mut t: BTreeMap<usize, Tally> = BTreeMap::new();
    for j in judged {
        let e = t.entry(j.pop).or_default();
        e.n += 1;
        e.detected += usize::from(j.over > 0);
        e.searched += usize::from(j.searched);
        e.solved += usize::from(j.solved());
        e.framed += usize::from(j.framed());
        let v = j.verdict.unwrap_or(Verdict::Energy);
        e.matched += usize::from(v == j.achievable && (v < Verdict::Framed || j.params_ok));
        e.over_claim += usize::from(v > j.achievable);
        e.violations.extend(
            j.violations
                .iter()
                .map(|v| format!("{} seed {}: {v}", POPULATIONS[j.pop].id, j.seed)),
        );
    }
    t
}

fn share(k: usize, n: usize) -> f64 {
    if n == 0 { 0.0 } else { k as f64 / n as f64 }
}

/// Every §7 threshold the tallies break, with the numbers; empty = §7 met. Sweep rates are
/// pooled per SNR over both families; the partial clause pools its populations.
pub fn breaches(t: &BTreeMap<usize, Tally>) -> Vec<String> {
    let mut out = Vec::new();
    let mut by_snr: BTreeMap<u8, (usize, usize, usize)> = BTreeMap::new();
    let (mut pn, mut pm, mut po) = (0, 0, 0);
    for (&i, x) in t {
        out.extend(x.violations.iter().cloned());
        match POPULATIONS[i].clause {
            Clause::Sweep(snr) => {
                let e = by_snr.entry(snr).or_default();
                e.0 += x.n;
                e.1 += x.solved;
                e.2 += x.framed;
            }
            Clause::Partial => {
                pn += x.n;
                pm += x.matched;
                po += x.over_claim;
            }
        }
    }
    for (snr, (n, solved, framed)) in by_snr {
        let (what, k, need) = match snr {
            20 => ("solved", solved, SOLVED_AT_20DB),
            10 => ("solved", solved, SOLVED_AT_10DB),
            _ => ("at least framed", framed, FRAMED_AT_6DB),
        };
        if n == 0 || share(k, n) < need {
            out.push(format!(
                "sweep at {snr} dB: {what} {k}/{n} = {:.0} % < {:.0} % (ADR-0015 §7)",
                100.0 * share(k, n),
                100.0 * need
            ));
        }
    }
    if pn > 0 {
        if share(pm, pn) < PARTIAL_MATCH_MIN {
            out.push(format!(
                "partial quality: verdict = deepest achievable {pm}/{pn} = {:.0} % < {:.0} %",
                100.0 * share(pm, pn),
                100.0 * PARTIAL_MATCH_MIN
            ));
        }
        if share(po, pn) > PARTIAL_OVERCLAIM_MAX {
            out.push(format!(
                "partial quality: over-claim {po}/{pn} = {:.1} % > {:.0} %",
                100.0 * share(po, pn),
                100.0 * PARTIAL_OVERCLAIM_MAX
            ));
        }
    }
    out
}

/// The report: one line per case (SNR, CFO, rate, profile, wall, CPU — §7's reporting rule),
/// then per population, then the verdict.
pub fn report(engine: &str, cases: &[Case], judged: &[Judged], armed: bool) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let build = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let _ = writeln!(
        s,
        "[{USE_CASES}] ADR-0015 §7 generic FSK/OOK sweep — {engine}, profile {PROFILE}, {build} \
         build on {} (the Jetson re-run is M-13's)",
        std::env::consts::OS
    );
    for (c, j) in cases.iter().zip(judged) {
        let t = &c.truth;
        let _ = writeln!(
            s,
            "  {:<24} seed {:>2}: {:>4} {:>8.1} Bd  cfo {:+.3}×bw  sync {:>2}b  check {:<22} \
             achievable {:<7} | emitters over it {} | off {:<12} on {:<12} params {} | wall {:.2} s cpu {:.2} s",
            POPULATIONS[c.pop].id,
            c.seed,
            t["modulation"].as_str().unwrap_or("?"),
            t["symbol_rate_bd"].as_f64().unwrap_or(0.0),
            t["cfo_frac"].as_f64().unwrap_or(0.0),
            t["sync_bits"].as_u64().unwrap_or(0),
            t["check"]["catalogue_name"]
                .as_str()
                .or(t["check_kind"].as_str())
                .unwrap_or("?"),
            format!("{:?}", j.achievable).to_lowercase(),
            j.over,
            j.verdict
                .map_or("not-searched".into(), |v| format!("{v:?}").to_lowercase()),
            j.verdict_on
                .map_or("not-searched".into(), |v| format!("{v:?}").to_lowercase()),
            if j.params_ok { "ok" } else { "-" },
            j.wall_s,
            j.cpu_s,
        );
    }
    for (i, x) in tally(judged) {
        let p = &POPULATIONS[i];
        let _ = writeln!(
            s,
            "  {:<24} n {} detected {} searched {} | solved {} framed+ {} = deepest {} over-claim {}",
            p.id, x.n, x.detected, x.searched, x.solved, x.framed, x.matched, x.over_claim
        );
    }
    if armed {
        let b = breaches(&tally(judged));
        let _ = writeln!(
            s,
            "  §7: {}",
            if b.is_empty() {
                "every threshold met".to_owned()
            } else {
                b.join("; ")
            }
        );
    } else {
        let _ = writeln!(
            s,
            "  §7 rates: NOT MEASURED — server_backend() is None (no production search backend), \
             so every job ended failed/no_evaluator: not-searched is neither a pass nor a fail \
             (ADR-0021 §7A.4). Thresholds armed: solved >= {:.0} % @20 dB, >= {:.0} % @10 dB, \
             framed+ >= {:.0} % @6 dB; partial match >= {:.0} %, over-claim <= {:.0} %.",
            100.0 * SOLVED_AT_20DB,
            100.0 * SOLVED_AT_10DB,
            100.0 * FRAMED_AT_6DB,
            100.0 * PARTIAL_MATCH_MIN,
            100.0 * PARTIAL_OVERCLAIM_MAX
        );
    }
    s
}

// ------------------------------------------------------------------------------------------
// The product test.
// ------------------------------------------------------------------------------------------

#[test]
fn section_7_generic_fsk_sweep_through_the_mock_sdr_and_the_analyze_api() {
    let Some(cases) = product_cases() else {
        return;
    };
    assert!(!cases.is_empty(), "[{USE_CASES}] every seed is sealed?");
    let answers = run_all(&Product, &cases);
    let judged: Vec<Judged> = cases
        .iter()
        .zip(&answers)
        .map(|(c, a)| judge(c, a))
        .collect();
    let armed = server_backend().is_some();
    eprintln!("{}", report(Product.name(), &cases, &judged, armed));

    // Plumbing that binds armed or not. A missed emission is *not* asserted per case: §7 counts
    // it as not solved, and the report's `detected` column shows it (T-863 measured 4.5 ms
    // bursts at 25.7 kBd leaving no emitter). But the suite must have exercised the product at
    // all — some case with a target over its emission — or its green would be vacuous; every job
    // that ran must have read real IQ; and nothing may be labelled over no emission.
    let detected = judged.iter().filter(|j| j.over > 0).count();
    assert!(
        detected > 0,
        "[{USE_CASES}] no case produced a target over its emission: the suite exercised nothing"
    );
    for j in &judged {
        let id = POPULATIONS[j.pop].id;
        if j.over == 0 {
            eprintln!(
                "[{USE_CASES}] WARN {id} seed {}: blind detection left no target over the \
                 emission (counted as not solved)",
                j.seed
            );
        }
        assert!(
            j.over == 0 || j.min_samples > 0,
            "[{USE_CASES}] {id} seed {}: a job read no IQ from the ring (failures: {:?})",
            j.seed,
            j.failures
        );
        assert_eq!(
            j.false_labels, 0,
            "[{USE_CASES}] {id} seed {}: {:?}",
            j.seed, j.violations
        );
    }
    if armed {
        let b = breaches(&tally(&judged));
        assert!(
            b.is_empty(),
            "[{USE_CASES}] ADR-0015 §7 not met:\n  {}",
            b.join("\n  ")
        );
    } else {
        // The declared state, exactly: every job acquired and then said it searched nothing
        // because there is no evaluator — nothing else failed, and nothing claimed a verdict.
        for j in &judged {
            assert!(
                j.verdict.is_none() && j.verdict_on.is_none(),
                "[{USE_CASES}] no backend, yet a verdict was served: {j:?}"
            );
            for f in &j.failures {
                assert!(
                    f.ends_with(": failed (no_evaluator)"),
                    "[{USE_CASES}] {} seed {}: a job failed for a reason other than the declared \
                     missing backend: {f}",
                    POPULATIONS[j.pop].id,
                    j.seed
                );
            }
        }
    }
}

// ------------------------------------------------------------------------------------------
// Degeneracy: the suite can pass and can fail (pure, over hand-built truths).
// ------------------------------------------------------------------------------------------

const SYNC: &str = "1100101000111011010010";

fn stub_truth(i: usize, achievable: &str) -> Value {
    json!({
        "rf_center_hz": 868.42e6, "bandwidth_hz": 12_000.0, "symbol_rate_bd": 4_800.0 + i as f64,
        "sync_bitstring": SYNC, "deepest_achievable": achievable, "modulation": "2fsk",
        "cfo_frac": 0.05, "sync_bits": SYNC.len(), "check_kind": "catalogue",
    })
}

/// Ten cases per population: the partial ones split between `solved` and `framed` achievable.
fn stub_cases() -> Vec<Case> {
    let mut v = Vec::new();
    for (pop, p) in POPULATIONS.iter().enumerate() {
        for seed in 0..10u64 {
            let achievable = match (p.clause, p.check) {
                (Clause::Partial, "absent") => "framed",
                (Clause::Partial, _) if seed % 2 == 0 => "framed",
                _ => "solved",
            };
            v.push(Case {
                pop,
                seed,
                meta: None,
                truth: stub_truth(pop * 10 + seed as usize, achievable),
            });
        }
    }
    v
}

fn answered(truth: &Value, verdict: Option<Verdict>, rate_scale: f64, sync: &str) -> Answer {
    let f = truth["rf_center_hz"].as_f64().unwrap();
    let a = JobAnswer {
        state: if verdict.is_some() { "done" } else { "failed" }.into(),
        error: verdict.is_none().then(|| "no_evaluator".into()),
        samples: 1_200_000,
        verdict,
        symbol_rate_bd: Some(truth["symbol_rate_bd"].as_f64().unwrap() * rate_scale),
        sync: Some(sync.into()),
        wall_s: 1.0,
        cpu_s: 1.0,
    };
    Answer {
        emitters: vec![EmitterAnswer {
            id: "e1".into(),
            f_lo_hz: f - 5_000.0,
            f_hi_hz: f + 5_000.0,
            off: a.clone(),
            on: a,
        }],
    }
}

/// Answers exactly the truth's deepest achievable stage with the right parameters.
struct Oracle;
impl Engine for Oracle {
    fn name(&self) -> &'static str {
        "oracle stub"
    }
    fn run(&self, c: &Case) -> Answer {
        let v = Verdict::parse(c.truth["deepest_achievable"].as_str().unwrap());
        answered(&c.truth, v, 1.0, SYNC)
    }
}

/// Searches nothing (today's product, in effect).
struct Silent;
impl Engine for Silent {
    fn name(&self) -> &'static str {
        "silent stub"
    }
    fn run(&self, c: &Case) -> Answer {
        answered(&c.truth, None, 1.0, SYNC)
    }
}

/// Solves everything with the right parameters — over-claims wherever truth says `framed`.
struct OverClaimer;
impl Engine for OverClaimer {
    fn name(&self) -> &'static str {
        "over-claiming stub"
    }
    fn run(&self, c: &Case) -> Answer {
        answered(&c.truth, Some(Verdict::Solved), 1.0, SYNC)
    }
}

/// Reaches the truth's stage with the symbol rate 2 % off.
struct WrongRate;
impl Engine for WrongRate {
    fn name(&self) -> &'static str {
        "wrong-rate stub"
    }
    fn run(&self, c: &Case) -> Answer {
        let v = Verdict::parse(c.truth["deepest_achievable"].as_str().unwrap());
        answered(&c.truth, v, 1.02, SYNC)
    }
}

fn stub_breaches(engine: &dyn Engine) -> Vec<String> {
    let cases = stub_cases();
    let judged: Vec<Judged> = cases.iter().map(|c| judge(c, &engine.run(c))).collect();
    eprintln!("{}", report(engine.name(), &cases, &judged, true));
    breaches(&tally(&judged))
}

#[test]
fn section_7_can_pass_an_oracle_meets_every_threshold() {
    let b = stub_breaches(&Oracle);
    assert!(b.is_empty(), "an oracle must meet §7: {b:?}");
}

#[test]
fn section_7_can_fail_a_silent_engine_misses_every_rate() {
    let b = stub_breaches(&Silent);
    for snr in ["20 dB", "10 dB", "6 dB"] {
        assert!(
            b.iter().any(|x| x.starts_with(&format!("sweep at {snr}"))),
            "{snr} not breached: {b:?}"
        );
    }
    assert!(
        b.iter().any(|x| x.starts_with("partial quality: verdict")),
        "{b:?}"
    );
}

#[test]
fn section_7_can_fail_an_over_claimer_breaks_the_partial_bound() {
    let b = stub_breaches(&OverClaimer);
    assert!(
        b.iter()
            .any(|x| x.starts_with("partial quality: over-claim")),
        "{b:?}"
    );
    // It solves every sweep case, so the sweep rates hold: the over-claim is caught by the
    // partial clause alone, which is the clause's reason to exist.
    assert!(!b.iter().any(|x| x.starts_with("sweep")), "{b:?}");
}

#[test]
fn section_7_a_solve_at_the_wrong_rate_is_not_a_solve() {
    let b = stub_breaches(&WrongRate);
    assert!(b.iter().any(|x| x.starts_with("sweep at 20 dB")), "{b:?}");
    assert!(b.iter().any(|x| x.contains("rate within 1 %")), "{b:?}");
}

#[test]
fn section_7_a_label_over_nothing_is_a_false_label() {
    let c = &stub_cases()[0];
    let mut a = Oracle.run(c);
    let mut stray = a.emitters[0].clone();
    stray.id = "e2".into();
    stray.f_lo_hz += 1e6;
    stray.f_hi_hz += 1e6;
    a.emitters.push(stray);
    let j = judge(c, &a);
    assert_eq!(j.false_labels, 2, "{j:?}");
    assert!(j.solved(), "the real emitter's answer still counts: {j:?}");
}

#[test]
fn section_7_reads_the_served_job_shape() {
    let job = json!({
        "id": "a1", "state": "done", "error": null,
        "window": {"samples": 1_200_000},
        "used": {"wall_s": 3.5, "cpu_s": 7.0},
        "results": [{"verdict": "solved", "recipe": {"nodes": [
            {"id": "fsk", "block": "fsk_demod", "params": {}},
            {"id": "clock", "block": "clock_recovery", "params": {"symbol_rate_bd": 4800.0}},
            {"id": "sync", "block": "sync_search", "params": {"mode": "sync-word", "sync_word": "0x2dd4", "sync_bits": 16}},
        ]}}],
    });
    let a = read_job(&job);
    assert_eq!(a.verdict, Some(Verdict::Solved));
    assert_eq!(a.symbol_rate_bd, Some(4800.0));
    assert_eq!(a.sync.as_deref(), Some("0010110111010100"));
    assert!(a.searched());
    let failed = read_job(&json!({"state": "failed", "error": {"code": "no_evaluator"}}));
    assert!(!failed.searched() && failed.verdict.is_none());
    assert_eq!(failed.error.as_deref(), Some("no_evaluator"));
    let unknown = read_job(&json!({"state": "done", "results": [],
        "resolution": {"kind": "unknown", "deepest_verdict": "clocked"}}));
    assert_eq!(unknown.verdict, Some(Verdict::Clocked));
}
