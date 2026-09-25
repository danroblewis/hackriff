//! **The negative-control guard suite** (T-568; docs/22 §4.3, §6.2, §10; ADR-0021 §8.4) —
//! `SIGNAL-001`, `SIGNAL-062`, `RESEARCH-009`.
//!
//! ```text
//! cargo nextest run -p hk-e2e -E 'binary(acceptance_mauto) & test(/mauto_negatives/)'
//! ```
//!
//! Input with **nothing to find** goes through the mock SDR, blind, and the suite counts every
//! label the system puts on it. docs/22 §4.3's five populations, each built by an existing
//! generator (none is rebuilt here):
//!
//! | pop | sub-population (manifest `NP` level) | generator | must return |
//! |---|---|---|---|
//! | N1 | `n1-thermal` | `noise_floor_rise`, `step_db=0`, `n_weak_signals=0` | `unknown` / `no-signal`, deepest `energy`; 0 labels ≥ `framed`; **0 emitters** |
//! | N2 | `n2-cw`, `n2-fm-voice`, `n2-am-voice` | `tone`, `nbfm_voice`, `am_voice` (T-624) | `unknown` / `nothing-scored`, deepest ≤ `demodulated`; 0 ≥ `framed` |
//! | N3 | `n3-ofdm`, `n3-dsss`, `n3-16qam` | T-623's structures (CSS = P9, declared) | `unsupported-structure` naming the structure + `missing_block`; 0 `solved`; `framed` only on a measured framing. **Scored separately** |
//! | N4 | `n4-quiet-433` | the real 433 MHz quiet window (`ism_433p62M_2M_l24g30a1_t162p0_6s`) | as N1, and a spur (`artifact-of`) never ≥ `demodulated` |
//! | N5 | `n5-off-grid`, `n5-adjacent-leakage` | `mismatched_hypothesis` (T-626) | `unknown`, or a correct partial; **never** ≥ `framed` with wrong parameters — judged by `hkpy.synth.mismatch.classify_outcome` (a miss is NOT a false label) |
//!
//! The terminator half of N4 is **T-375, a user action, outstanding**: it is a declared cell in
//! the coverage manifest (`NEG NP=n4-terminator-50ohm`), never silently absent, and every run
//! prints docs/22 §10's sentence that the receiver-only null is unmeasured.
//!
//! # The stated budget
//!
//! **[`FALSE_LABEL_BUDGET`] = 0 false labels over N1 ∪ N2 ∪ N4 ∪ N5**, and **0 `solved` on N3**,
//! counted separately. A *label* is a served verdict ≥ `framed` (`/api/analyze {emitter_id}`,
//! the `docs/07` EmitterSynthesis). Every run prints the count per sub-population and the pooled
//! rule-of-three bound (`3/n`, 95 %) the zero buys at the `n` that actually ran.
//!
//! **CI n is deliberately small** — [`CI_SEEDS`] per synthetic sub-population, minus the sealed
//! hold-out — not docs/22's 400 (N1) / 200 (others): at CI tier this suite is a *guard* (it
//! must be able to go red, and it goes red on the first over-claim), not a rate measurement. The
//! full-n run, the n ≈ 60 000 question and A2's tail-slope report are **T-576**'s; the
//! check-width / degenerate-frame measurements are **T-577**'s.
//!
//! # What the product can and cannot be asked today — stated, not papered over
//!
//! The engine is reached the only way a client reaches it: `POST /api/analyze {emitter_id}` on
//! every emitter the blind run produced. On this build **stage evaluation over IQ has no
//! production backend**: region jobs acquire and end `failed / no_evaluator`
//! (`hk_pipeline::synth::jobs`), and the only `EmitterSynthesis` writers are the trunk chain and
//! attached jobs. So on negatives every emitter answers `not-searched` — which is **not**
//! `unknown` (ADR-0021 §7A.4) and is counted in its own column, never as a pass of the "must
//! return" column. The must-return rules are nevertheless **armed**: the moment any answer
//! carries a resolution other than `not-searched`, [`judge`] holds it to its population's row.
//! The label budget, N1's zero emitters and the N5 mismatch rule bind today.
//!
//! Profiles: the persisted analysis has none (no `quick`/`standard`/`deep` search runs on this
//! build), so the count is reported under one profile, `pipeline`, and says so.
//!
//! # Degeneracy (docs/22 §6.2): the suite must be able to fail
//!
//! [`judge`] and [`budget`] are pure, and the [`Engine`] seam lets a deliberately
//! **over-claiming stub** stand in for the product: it answers `solved` on everything, and
//! `negative_controls_can_fail_*` asserts the suite turns red on it — per population, with N5
//! classified by the same Python rule. A second stub over-claims on N3 only and proves N3 is
//! scored apart from the pooled budget.
//!
//! # Blind
//!
//! The device serves a truth-stripped copy (`blind_config`); nothing tells the run a frequency,
//! a modulation or that the input is a negative. The truth is read only by [`judge`], after the
//! run, to *score* (N3's structure name, N5's true rate and analysed box).

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use hk_e2e::Fixture;
use hk_e2e::SynthRequest;
use hk_e2e::corpus::{Cell, Row, SeedPlan};
use serde_json::{Value, json};

use crate::blind::{BlindSource, blind_config, start};
use crate::common::*;
use crate::signal_087::{api_post, serve_api_with_control};

const USE_CASES: &str = "SIGNAL-001 / SIGNAL-062 / RESEARCH-009 (T-568)";

/// **The stated false-label budget over N1 ∪ N2 ∪ N4 ∪ N5**, per profile (docs/22 §4.3).
pub const FALSE_LABEL_BUDGET: usize = 0;
/// `solved` answers allowed on N3, which is scored separately.
pub const N3_SOLVED_BUDGET: usize = 0;
/// Seeds drawn per synthetic sub-population at CI tier, before the hold-out seals a fifth.
pub const CI_SEEDS: std::ops::RangeInclusive<u64> = 1..=3;
/// The profile the count is reported under (module docs: no job profile runs on this build).
pub const PROFILE: &str = "pipeline";
/// The manifest plane the populations populate, and its axis.
pub const NEG_PLANE: &str = "NEG";
/// The test that populates the NEG rows.
pub const GUARD_TEST: &str =
    "mauto_negatives::negative_controls_hold_the_false_label_budget_through_the_mock_sdr";

/// docs/22 §4.3's populations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Population {
    /// Thermal noise.
    N1,
    /// Energy without symbols.
    N2,
    /// Out-of-catalogue structure — scored separately.
    N3,
    /// Real empty capture.
    N4,
    /// Mismatched hypothesis.
    N5,
}

impl Population {
    /// Pooled into the false-label count (N3 is not).
    pub fn pooled(self) -> bool {
        self != Population::N3
    }

    /// docs/22 §4.3's CI-tier n for the whole population (T-576 runs it).
    pub fn docs22_n(self) -> usize {
        if self == Population::N1 { 400 } else { 200 }
    }
}

/// Where a sub-population's IQ comes from.
#[derive(Clone, Copy, Debug)]
pub enum Source {
    /// A `hkpy.synth` scenario with these parameters.
    Synth {
        /// Scenario name.
        scenario: &'static str,
        /// `--param k=v`.
        params: &'static [(&'static str, &'static str)],
    },
    /// A real HackRF fixture (`real_fixture_in`).
    Real {
        /// Directory under the repo root.
        dir: &'static str,
        /// Fixture name, without `.sigmf-meta`.
        name: &'static str,
    },
}

/// One sub-population: a level of the manifest's `NP` axis.
#[derive(Clone, Copy, Debug)]
pub struct SubPop {
    /// The `NP` level.
    pub level: &'static str,
    /// The manifest row id.
    pub row: &'static str,
    /// Its population.
    pub pop: Population,
    /// Its IQ.
    pub source: Source,
}

const SYNTH_33: &str = "fixtures/hackrf/2026-09-13";

/// Every sub-population that runs. The declared-but-empty ones (`n3-css`,
/// `n4-terminator-50ohm`) are in `mauto_corpus::declarations`.
pub const SUBPOPS: &[SubPop] = &[
    SubPop {
        level: "n1-thermal",
        row: "N1-thermal",
        pop: Population::N1,
        source: Source::Synth {
            scenario: "noise_floor_rise",
            params: &[("step_db", "0"), ("n_weak_signals", "0")],
        },
    },
    SubPop {
        level: "n2-cw",
        row: "N2-cw",
        pop: Population::N2,
        source: Source::Synth {
            scenario: "tone",
            // The default 0.05 s tone is shorter than the detector's confirmation window and
            // produced no emitter at all -- a negative nobody could label proves nothing. 0.4 s
            // matches the other N2/N3 scenes.
            params: &[("duration_s", "0.4")],
        },
    },
    SubPop {
        level: "n2-fm-voice",
        row: "N2-fm-voice",
        pop: Population::N2,
        source: Source::Synth {
            scenario: "nbfm_voice",
            params: &[],
        },
    },
    SubPop {
        level: "n2-am-voice",
        row: "N2-am-voice",
        pop: Population::N2,
        source: Source::Synth {
            scenario: "am_voice",
            params: &[],
        },
    },
    SubPop {
        level: "n3-ofdm",
        row: "N3-ofdm",
        pop: Population::N3,
        source: Source::Synth {
            scenario: "ofdm_nonstandard_cp",
            params: &[],
        },
    },
    SubPop {
        level: "n3-dsss",
        row: "N3-dsss",
        pop: Population::N3,
        source: Source::Synth {
            scenario: "dsss_m_sequence",
            params: &[],
        },
    },
    SubPop {
        level: "n3-16qam",
        row: "N3-16qam",
        pop: Population::N3,
        source: Source::Synth {
            scenario: "qam16_unframed",
            params: &[],
        },
    },
    SubPop {
        level: "n4-quiet-433",
        row: "N4-quiet-433",
        pop: Population::N4,
        source: Source::Real {
            dir: SYNTH_33,
            name: "ism_433p62M_2M_l24g30a1_t162p0_6s",
        },
    },
    SubPop {
        level: "n5-off-grid",
        row: "N5-off-grid",
        pop: Population::N5,
        source: Source::Synth {
            scenario: "mismatched_hypothesis",
            params: &[("population", "off_grid")],
        },
    },
    SubPop {
        level: "n5-adjacent-leakage",
        row: "N5-adjacent-leakage",
        pop: Population::N5,
        source: Source::Synth {
            scenario: "mismatched_hypothesis",
            params: &[("population", "adjacent_leakage")],
        },
    },
];

/// The hold-out scene id a sub-population's seeds hash under.
pub fn scene_id(sub: &SubPop) -> String {
    format!("mauto/negative/{}", sub.level)
}

/// The N4 real window is one fixed recording; it pre-dates the seal.
const N4_FIXED: &str = "N4 is one real recording (2026-09-13), not seeded draws; it is run in the \
                        open";

/// The NEG plane's populated rows, for `mauto_corpus`'s manifest.
pub fn manifest_rows() -> Vec<Row> {
    SUBPOPS
        .iter()
        .map(|sub| {
            let (seeds, fixed_seeds) = match sub.source {
                Source::Synth { .. } => (SeedPlan::new(&scene_id(sub), CI_SEEDS), None),
                Source::Real { .. } => (SeedPlan::new(&scene_id(sub), [0]), Some(N4_FIXED)),
            };
            Row {
                id: sub.row,
                tests: std::slice::from_ref(&GUARD_TEST),
                seeds,
                fixed_seeds,
                cells: vec![Cell::new(NEG_PLANE, &[("NP", sub.level)])],
            }
        })
        .collect()
}

// ------------------------------------------------------------------------------------------
// Jobs, answers and the engine seam.
// ------------------------------------------------------------------------------------------

/// One negative job.
pub struct Job {
    /// Its sub-population.
    pub sub: &'static SubPop,
    /// The seed (`None` for the real window).
    pub seed: Option<u64>,
    /// The recording the mock device serves (truth-stripped before it does). `None` for a stub.
    pub meta: Option<PathBuf>,
    /// The hidden truth the judge scores against: the scenario's `negative_population` block
    /// (N3, N5), `Null` elsewhere. Never reaches the engine.
    pub truth: Value,
}

/// One inventory emitter the run produced, and what the engine says it is.
#[derive(Clone, Debug)]
pub struct EmitterAnswer {
    /// Emitter id.
    pub id: String,
    /// Band, Hz.
    pub f_lo_hz: f64,
    /// Band, Hz.
    pub f_hi_hz: f64,
    /// `relation.kind` when the row defers to another (`artifact-of`, …).
    pub relation: Option<String>,
    /// The served `AnalyzeResult` (`POST /api/analyze {emitter_id}`).
    pub analysis: Value,
}

/// What an engine made of one job.
#[derive(Clone, Debug, Default)]
pub struct Answer {
    /// Every emitter, related rows included (`relations=all`).
    pub emitters: Vec<EmitterAnswer>,
}

/// Something that turns a negative job into answers: the product through the mock SDR, or a stub.
pub trait Engine: Sync {
    /// Its name, for the report.
    fn name(&self) -> &'static str;
    /// Runs one job.
    fn run(&self, job: &Job) -> Answer;
}

/// **The product**: the job's recording, truth stripped, through the mock SDR and the whole
/// pipeline with the built-in registry and no plan (`json!({})`), then `/api/analyze` asked about
/// every emitter it produced.
pub struct Product;

impl Engine for Product {
    fn name(&self) -> &'static str {
        "product (mock SDR, blind)"
    }

    fn run(&self, job: &Job) -> Answer {
        let meta = job.meta.as_ref().expect("a product job has a recording");
        let tag = format!("neg-{}-{}", job.sub.level, job.seed.unwrap_or(0));
        let cfg = blind_config(meta, &tag, BlindSource::default(), json!({}));
        let dir = cfg.dir;
        let handle = start(cfg.cfg, cfg.replay);
        let _ = finish(handle);
        let server = serve_api_with_control(&dir.0);
        let addr = server.local_addr();
        let mut rows = Vec::new();
        let mut path = "/api/inventory?limit=500&relations=all".to_owned();
        loop {
            let (status, body) = api_get(addr, &path);
            assert_eq!(status, 200, "{path}: {}", String::from_utf8_lossy(&body));
            let v: Value = serde_json::from_slice(&body).unwrap();
            rows.extend(v["entries"].as_array().cloned().unwrap_or_default());
            match v.get("next_cursor").and_then(Value::as_u64) {
                Some(c) => path = format!("/api/inventory?limit=500&relations=all&cursor={c}"),
                None => break,
            }
        }
        let emitters = rows
            .iter()
            .map(|r| {
                let id = r["id"]
                    .as_str()
                    .expect("an inventory row has an id")
                    .to_owned();
                let (status, body) = api_post(
                    addr,
                    "/api/analyze",
                    &json!({ "emitter_id": id }).to_string(),
                );
                assert_eq!(
                    status,
                    200,
                    "[{USE_CASES}] POST /api/analyze {{emitter_id}} on {id}: {}",
                    String::from_utf8_lossy(&body),
                );
                let f = r["f_center_hz"].as_f64().unwrap_or(f64::NAN);
                let bw = r["bandwidth_hz"].as_f64().unwrap_or(0.0);
                EmitterAnswer {
                    id,
                    f_lo_hz: f - bw / 2.0,
                    f_hi_hz: f + bw / 2.0,
                    relation: r["relation"]["kind"].as_str().map(str::to_owned),
                    analysis: serde_json::from_slice(&body).unwrap(),
                }
            })
            .collect();
        Answer { emitters }
    }
}

/// Builds the product jobs: generates each synthetic sub-population's open seeds and finds the
/// real window. `None` when synthesis is unavailable (the suite-wide skip rule). A missing real
/// fixture skips that sub-population alone, and says so.
fn product_jobs() -> Option<Vec<Job>> {
    let mut jobs = Vec::new();
    for sub in SUBPOPS {
        match sub.source {
            Source::Synth { scenario, params } => {
                for seed in SeedPlan::new(&scene_id(sub), CI_SEEDS).runnable() {
                    let mut req = SynthRequest::new(scenario).seed(seed);
                    for (k, v) in params {
                        req = req.param(*k, v);
                    }
                    let out = match req.generate() {
                        Ok(out) => out,
                        Err(err) if err.is_unavailable() && !hk_e2e::synth::require_synth() => {
                            eprintln!("SKIP {USE_CASES}: {err}");
                            return None;
                        }
                        Err(err) => panic!("{scenario} seed {seed}: generation failed: {err}"),
                    };
                    let fx = out.fixture(0).unwrap();
                    let truth = fx
                        .scenario()
                        .and_then(|s| s.get("negative_population"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    jobs.push(Job {
                        sub,
                        seed: Some(seed),
                        meta: Some(fx.meta_path.clone()),
                        truth,
                    });
                }
            }
            Source::Real { dir, name } => {
                let Some(meta) = real_fixture_in(dir, name) else {
                    eprintln!(
                        "SKIP {USE_CASES}: {} ({name}) is not fetched; N4 did not run",
                        sub.level
                    );
                    continue;
                };
                Fixture::load(&meta).expect("the N4 window loads");
                jobs.push(Job {
                    sub,
                    seed: None,
                    meta: Some(meta),
                    truth: Value::Null,
                });
            }
        }
    }
    Some(jobs)
}

/// Runs every job through `engine`, a few at a time (each is a whole pipeline run).
fn run_all(engine: &dyn Engine, jobs: &[Job]) -> Vec<Answer> {
    const PARALLEL: usize = 3;
    let mut out = Vec::with_capacity(jobs.len());
    for chunk in jobs.chunks(PARALLEL) {
        let answers: Vec<Answer> = std::thread::scope(|s| {
            let hs: Vec<_> = chunk.iter().map(|j| s.spawn(|| engine.run(j))).collect();
            hs.into_iter()
                .map(|h| h.join().expect("a job panicked"))
                .collect()
        });
        out.extend(answers);
    }
    out
}

// ------------------------------------------------------------------------------------------
// The judge.
// ------------------------------------------------------------------------------------------

/// The ADR-0015 §3.4 ladder rank of a served verdict; `None` for no verdict (never analysed).
pub fn verdict_rank(v: &str) -> Option<u8> {
    Some(match v {
        "energy" => 0,
        "demodulated" => 1,
        "clocked" => 2,
        "framed" => 3,
        "checked" => 4,
        "solved" => 5,
        _ => return None,
    })
}

const FRAMED: u8 = 3;
const DEMODULATED: u8 = 1;
const SOLVED: u8 = 5;

/// The trace shows a **measured** framing: an S4 node that was tried and carries a measurement.
fn measured_framing(a: &Value) -> bool {
    a["trace"].as_array().is_some_and(|t| {
        t.iter().any(|n| {
            n["stage"] == "s4-framing" && n["tried"] == json!(true) && !n["measured"].is_null()
        })
    })
}

/// The symbol rate a result bound, from `pipeline.params`.
fn bound_symbol_rate(a: &Value) -> Option<f64> {
    a["pipeline"]["params"].as_array()?.iter().find_map(|p| {
        let name = p.get(0)?.as_str()?;
        if !(name.contains("symbol_rate") || name.contains("baud")) {
            return None;
        }
        let v = p.get(1)?;
        v.as_f64().or_else(|| v.as_str()?.trim().parse().ok())
    })
}

/// Per sub-population counts.
#[derive(Clone, Debug, Default)]
pub struct Tally {
    /// Jobs run.
    pub jobs: usize,
    /// Emitters produced (all relations).
    pub emitters: usize,
    /// Answers with a verdict ≥ `framed`.
    pub labels: usize,
    /// Labels that are false (every label on N1/N2/N4; an N5 `mismatch`). Not counted on N3.
    pub false_labels: usize,
    /// `solved` answers.
    pub solved: usize,
    /// `not-searched` answers: nothing looked, nothing ruled out.
    pub not_searched: usize,
    /// Every other resolution, `kind/reason/deepest`.
    pub resolutions: BTreeMap<String, usize>,
    /// N5 outcomes from `classify_outcome`.
    pub n5: BTreeMap<String, usize>,
    /// Must-return violations, each with its evidence.
    pub violations: Vec<String>,
}

/// One N5 classification request: the rank-1 result in the analysed box (or `Null`) and truth.
struct N5Item {
    sub: usize,
    input: Value,
    truth: Value,
    where_: String,
}

/// Scores every job. Pure except for N5, which is classified by
/// `hkpy.synth.mismatch.classify_outcome` in one `uv` call.
pub fn judge(jobs: &[Job], answers: &[Answer]) -> BTreeMap<&'static str, Tally> {
    let mut t: BTreeMap<&'static str, Tally> = BTreeMap::new();
    let mut n5_items = Vec::new();
    for (ji, (job, ans)) in jobs.iter().zip(answers).enumerate() {
        let sub = job.sub;
        let pop = sub.pop;
        let where_ = format!("{} seed {:?}", sub.level, job.seed);
        let tally = t.entry(sub.level).or_default();
        tally.jobs += 1;
        tally.emitters += ans.emitters.len();
        if pop == Population::N1 && !ans.emitters.is_empty() {
            tally.violations.push(format!(
                "{where_}: thermal noise produced {} emitter(s) (N1 must produce 0): {}",
                ans.emitters.len(),
                ans.emitters
                    .iter()
                    .map(|e| format!("{:.4}-{:.4} MHz", e.f_lo_hz / 1e6, e.f_hi_hz / 1e6))
                    .collect::<Vec<_>>()
                    .join(", "),
            ));
        }
        for e in &ans.emitters {
            let a = &e.analysis;
            let rank = a["verdict"].as_str().and_then(verdict_rank);
            let res = &a["resolution"];
            let kind = res["kind"].as_str().unwrap_or("");
            let reason = res["reason"].as_str().unwrap_or("");
            let deepest = res["deepest_verdict"].as_str().and_then(verdict_rank);
            let at = format!("{where_} emitter {} ({:.4} MHz)", e.id, e.f_lo_hz / 1e6);
            let label = rank.is_some_and(|r| r >= FRAMED);
            if label {
                tally.labels += 1;
            }
            if rank == Some(SOLVED) {
                tally.solved += 1;
            }
            if kind == "not-searched" {
                tally.not_searched += 1;
            } else if !kind.is_empty() {
                let d = res["deepest_verdict"].as_str().unwrap_or("-");
                *tally
                    .resolutions
                    .entry(format!("{kind}/{reason}/{d}"))
                    .or_default() += 1;
            }
            let searched = !kind.is_empty() && kind != "not-searched";
            match pop {
                Population::N1 | Population::N4 => {
                    if label {
                        tally.false_labels += 1;
                    }
                    if pop == Population::N4
                        && e.relation.as_deref() == Some("artifact-of")
                        && rank.is_some_and(|r| r >= DEMODULATED)
                    {
                        tally.violations.push(format!(
                            "{at}: a spur (artifact-of) reached verdict {} -- a spur is never \
                             a verdict >= demodulated",
                            a["verdict"]
                        ));
                    }
                    if searched
                        && (kind != "unknown" || reason != "no-signal" || deepest != Some(0))
                    {
                        tally.violations.push(format!(
                            "{at}: resolution {res} -- {pop:?} must return unknown / no-signal, \
                             deepest_verdict energy"
                        ));
                    }
                }
                Population::N2 => {
                    if label {
                        tally.false_labels += 1;
                    }
                    if searched
                        && (kind != "unknown"
                            || reason != "nothing-scored"
                            || deepest.is_none_or(|d| d > DEMODULATED))
                    {
                        tally.violations.push(format!(
                            "{at}: resolution {res} -- N2 must return unknown / nothing-scored, \
                             deepest_verdict <= demodulated"
                        ));
                    }
                }
                Population::N3 => {
                    if rank == Some(SOLVED) {
                        tally.violations.push(format!(
                            "{at}: SOLVED an out-of-catalogue structure ({}) -- N3 allows 0",
                            job.truth["structure_kind"]
                        ));
                    }
                    if label && !measured_framing(a) {
                        tally.violations.push(format!(
                            "{at}: verdict {} with no measured framing in the trace -- N3 may \
                             reach framed only where the trace shows one",
                            a["verdict"]
                        ));
                    }
                    if searched {
                        let summary = res["summary"].as_str().unwrap_or("").to_lowercase();
                        let names = |k: &str| {
                            job.truth[k]
                                .as_str()
                                .is_some_and(|s| summary.contains(&s.to_lowercase()))
                        };
                        let unsupported =
                            kind == "unsupported-structure" || reason == "unsupported-structure";
                        if !unsupported || !names("structure_kind") || !names("missing_block") {
                            tally.violations.push(format!(
                                "{at}: resolution {res} -- N3 must return unsupported-structure \
                                 naming the structure ({}) and its missing_block ({})",
                                job.truth["structure_kind"], job.truth["missing_block"]
                            ));
                        }
                    }
                }
                Population::N5 => {}
            }
        }
        if pop == Population::N5 {
            let bx = &job.truth["analysed_box"];
            let (lo, hi) = (
                bx["f_lo_hz"].as_f64().unwrap_or(f64::NEG_INFINITY),
                bx["f_hi_hz"].as_f64().unwrap_or(f64::INFINITY),
            );
            let in_box: Vec<&EmitterAnswer> = ans
                .emitters
                .iter()
                .filter(|e| e.f_hi_hz >= lo && e.f_lo_hz <= hi)
                .collect();
            let mut push = |input: Value, w: String| {
                n5_items.push(N5Item {
                    sub: ji,
                    input,
                    truth: job.truth.clone(),
                    where_: w,
                })
            };
            if in_box.is_empty() {
                push(
                    Value::Null,
                    format!("{where_}: nothing in the analysed box"),
                );
            }
            for e in in_box {
                let a = &e.analysis;
                push(
                    json!({
                        "verdict": a["verdict"],
                        "resolution": a["resolution"],
                        "symbol_rate_bd": bound_symbol_rate(a),
                    }),
                    format!("{where_} emitter {}", e.id),
                );
            }
        }
    }
    if !n5_items.is_empty() {
        let outcomes = classify_n5(&n5_items);
        for (item, out) in n5_items.iter().zip(outcomes) {
            let tally = t.get_mut(jobs[item.sub].sub.level).unwrap();
            let Some(out) = out else {
                tally.violations.push(format!(
                    "{}: not classified -- hkpy.synth.mismatch.classify_outcome could not run \
                     (uv unavailable)",
                    item.where_
                ));
                continue;
            };
            let o = out["outcome"].as_str().unwrap_or("?").to_owned();
            *tally.n5.entry(o.clone()).or_default() += 1;
            if o == "mismatch" {
                tally.false_labels += 1;
                if out["grid_snapped"] == json!(true) {
                    *tally.n5.entry("grid_snapped".to_owned()).or_default() += 1;
                }
                tally.violations.push(format!(
                    "{}: MISMATCH -- a label with the wrong parameters ({out})",
                    item.where_
                ));
            }
        }
    }
    t
}

/// `hkpy.synth.mismatch.classify_outcome` over every item, in one `uv` call. `None` per item when
/// `uv` cannot run it.
fn classify_n5(items: &[N5Item]) -> Vec<Option<Value>> {
    let none = || vec![None; items.len()];
    let Some(uv) = hk_e2e::synth::find_uv() else {
        return none();
    };
    let script = "import json, sys\n\
                  from hkpy.synth.mismatch import classify_outcome\n\
                  items = json.load(sys.stdin)\n\
                  json.dump([classify_outcome(r, t) for r, t in items], sys.stdout)\n";
    let payload = serde_json::to_vec(
        &items
            .iter()
            .map(|i| json!([i.input, i.truth]))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let child = Command::new(uv)
        .current_dir(hk_e2e::paths::repo_root())
        .args(["run", "--locked", "--quiet", "--project"])
        .arg(hk_e2e::paths::py_project())
        .args(["python", "-c", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let Ok(mut child) = child else {
        return none();
    };
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&payload)
        .expect("write the N5 items");
    let out = child.wait_with_output().expect("classify_outcome ran");
    assert!(
        out.status.success(),
        "classify_outcome failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Vec<Value> = serde_json::from_slice(&out.stdout).expect("classify_outcome JSON");
    assert_eq!(v.len(), items.len());
    v.into_iter().map(Some).collect()
}

/// The budget verdict: every failure, with its evidence. Empty = the budget held.
pub fn budget(t: &BTreeMap<&'static str, Tally>) -> Vec<String> {
    let mut fails = Vec::new();
    let pop_of = |level: &str| SUBPOPS.iter().find(|s| s.level == level).unwrap().pop;
    let pooled: usize = t
        .iter()
        .filter(|(l, _)| pop_of(l).pooled())
        .map(|(_, x)| x.false_labels)
        .sum();
    if pooled > FALSE_LABEL_BUDGET {
        fails.push(format!(
            "{pooled} false label(s) over N1 u N2 u N4 u N5 (profile {PROFILE}); the budget is \
             {FALSE_LABEL_BUDGET}"
        ));
    }
    let n3_solved: usize = t
        .iter()
        .filter(|(l, _)| pop_of(l) == Population::N3)
        .map(|(_, x)| x.solved)
        .sum();
    if n3_solved > N3_SOLVED_BUDGET {
        fails.push(format!(
            "{n3_solved} solved on N3 (scored separately); the budget is {N3_SOLVED_BUDGET}"
        ));
    }
    for x in t.values() {
        fails.extend(x.violations.iter().cloned());
    }
    fails
}

/// The report: per sub-population, per population, the pooled bound, and docs/22 §10's sentence.
pub fn report(engine: &str, t: &BTreeMap<&'static str, Tally>) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(
        s,
        "[{USE_CASES}] negative controls, engine: {engine}, profile: {PROFILE}"
    );
    let _ = writeln!(
        s,
        "  {:<22} {:>4} {:>8} {:>6} {:>6} {:>6} {:>12}  other",
        "sub-population", "jobs", "emitters", "labels", "false", "solved", "not-searched"
    );
    for sub in SUBPOPS {
        let Some(x) = t.get(sub.level) else {
            let _ = writeln!(s, "  {:<22} did not run", sub.level);
            continue;
        };
        let _ = writeln!(
            s,
            "  {:<22} {:>4} {:>8} {:>6} {:>6} {:>6} {:>12}  {:?} {:?}",
            sub.level,
            x.jobs,
            x.emitters,
            x.labels,
            if sub.pop.pooled() {
                x.false_labels.to_string()
            } else {
                "(sep)".to_owned()
            },
            x.solved,
            x.not_searched,
            x.resolutions,
            x.n5,
        );
    }
    for pop in [
        Population::N1,
        Population::N2,
        Population::N3,
        Population::N4,
        Population::N5,
    ] {
        let (mut jobs, mut fl) = (0, 0);
        for sub in SUBPOPS.iter().filter(|s| s.pop == pop) {
            if let Some(x) = t.get(sub.level) {
                jobs += x.jobs;
                fl += x.false_labels;
            }
        }
        let _ = writeln!(
            s,
            "  {pop:?}: n={jobs} (docs/22 CI n = {}, run at full n by T-576), {}",
            pop.docs22_n(),
            if pop.pooled() {
                format!("false labels {fl}")
            } else {
                "scored separately".to_owned()
            }
        );
    }
    let n: usize = SUBPOPS
        .iter()
        .filter(|s| s.pop.pooled())
        .filter_map(|s| t.get(s.level))
        .map(|x| x.jobs)
        .sum();
    let fl: usize = SUBPOPS
        .iter()
        .filter(|s| s.pop.pooled())
        .filter_map(|s| t.get(s.level))
        .map(|x| x.false_labels)
        .sum();
    let bound = if n > 0 { 3.0 / n as f64 } else { 1.0 };
    let _ = writeln!(
        s,
        "  pooled N1 u N2 u N4 u N5: {fl} false label(s) in n={n} jobs (budget \
         {FALSE_LABEL_BUDGET}); rule-of-three 95 % upper bound on the per-job rate: {bound:.3}"
    );
    let _ = writeln!(
        s,
        "  N4: the 50-ohm terminator row is ABSENT (T-375, a user action) -- the receiver-only \
         null is UNMEASURED; N4 here is the 433 MHz antenna window alone, a result about the \
         environment plus the receiver jointly (docs/22 §10)"
    );
    s
}

// ------------------------------------------------------------------------------------------
// The tests.
// ------------------------------------------------------------------------------------------

/// **The guard**: every population, blind through the mock SDR, against the stated budget.
#[test]
fn negative_controls_hold_the_false_label_budget_through_the_mock_sdr() {
    let Some(jobs) = product_jobs() else { return };
    let answers = run_all(&Product, &jobs);
    let t = judge(&jobs, &answers);
    eprintln!("{}", report(Product.name(), &t));
    for (job, ans) in jobs.iter().zip(&answers) {
        for e in &ans.emitters {
            eprintln!(
                "  [{} seed {:?}] emitter {:.4}-{:.4} MHz relation {:?} verdict {} resolution {}",
                job.sub.level,
                job.seed,
                e.f_lo_hz / 1e6,
                e.f_hi_hz / 1e6,
                e.relation,
                e.analysis["verdict"],
                e.analysis["resolution"]["kind"],
            );
        }
    }
    // Every sub-population with IQ ran: a population that silently ran nothing would pass.
    for sub in SUBPOPS {
        if matches!(sub.source, Source::Synth { .. }) {
            assert!(
                t.get(sub.level).is_some_and(|x| x.jobs > 0),
                "[{USE_CASES}] {} ran no job -- an empty population passes every budget",
                sub.level
            );
        }
    }
    let fails = budget(&t);
    assert!(
        fails.is_empty(),
        "[{USE_CASES}] the negative-control budget FAILED on the product:\n{}",
        fails.join("\n")
    );
}

/// An engine that **over-claims on everything**: one `solved` emitter per job, spanning the
/// spectrum, bound to a proposal-grid rate (2400 Bd) with no measured framing in its trace.
struct OverClaimer {
    only: Option<Population>,
}

impl Engine for OverClaimer {
    fn name(&self) -> &'static str {
        "stub: over-claims solved"
    }

    fn run(&self, job: &Job) -> Answer {
        if self.only.is_some_and(|p| p != job.sub.pop) {
            return Answer::default();
        }
        Answer {
            emitters: vec![EmitterAnswer {
                id: format!("stub-{}", job.sub.level),
                f_lo_hz: 0.0,
                f_hi_hz: 1e10,
                relation: (job.sub.pop == Population::N4).then(|| "artifact-of".to_owned()),
                analysis: json!({
                    "verdict": "solved",
                    "pipeline": {
                        "demod": "2fsk", "decode": "stub",
                        "params": [["symbol_rate_bd", 2400.0]], "summary": "stub",
                    },
                    "evidence": [], "trace": [],
                }),
            }],
        }
    }
}

/// Stub jobs: one per sub-population, with the truth blocks the judge reads (never the engine).
fn stub_jobs() -> Vec<Job> {
    SUBPOPS
        .iter()
        .map(|sub| Job {
            sub,
            seed: Some(1),
            meta: None,
            truth: match sub.pop {
                Population::N3 => json!({"structure_kind": "ofdm", "missing_block": "ofdm-demod"}),
                Population::N5 => json!({
                    "off_grid": {"symbol_rate_bd": 1873.0},
                    "analysed_box": {"f_lo_hz": 433.9e6, "f_hi_hz": 433.95e6},
                }),
                _ => Value::Null,
            },
        })
        .collect()
}

/// **Degeneracy detector (docs/22 §6.2)**: a suite that cannot fail proves nothing. The
/// over-claiming stub must turn the budget red in every population that can hold a label.
#[test]
fn negative_controls_can_fail_an_over_claiming_engine_turns_the_suite_red() {
    let jobs = stub_jobs();
    let engine = OverClaimer { only: None };
    let t = judge(&jobs, &run_all(&engine, &jobs));
    eprintln!("{}", report(engine.name(), &t));
    let fails = budget(&t);
    assert!(
        !fails.is_empty(),
        "the over-claiming stub PASSED the negative controls -- the suite cannot fail"
    );
    assert!(
        fails
            .iter()
            .any(|f| f.contains("false label(s) over N1 u N2 u N4 u N5")),
        "the pooled budget did not fire: {fails:#?}"
    );
    for level in [
        "n1-thermal",
        "n2-cw",
        "n2-fm-voice",
        "n2-am-voice",
        "n4-quiet-433",
    ] {
        assert_eq!(t[level].false_labels, 1, "{level}: {:?}", t[level]);
    }
    assert!(
        fails
            .iter()
            .any(|f| f.contains("n1-thermal") && f.contains("N1 must produce 0")),
        "N1's zero-emitter rule did not fire: {fails:#?}"
    );
    assert!(
        fails
            .iter()
            .any(|f| f.contains("n4-quiet-433") && f.contains("a spur")),
        "N4's spur rule did not fire: {fails:#?}"
    );
    assert!(
        fails.iter().any(|f| f.contains("solved on N3")),
        "N3's solved budget did not fire: {fails:#?}"
    );
    assert!(
        fails.iter().any(|f| f.contains("no measured framing")),
        "N3's measured-framing rule did not fire: {fails:#?}"
    );
    // N5 through the Python rule: a solved result at 2400 Bd on a 1873 Bd emitter is a mismatch,
    // snapped to the grid. Needs `uv`; without it the item is reported unclassified (and red).
    if hk_e2e::synth::find_uv().is_some() {
        for level in ["n5-off-grid", "n5-adjacent-leakage"] {
            assert_eq!(
                t[level].n5.get("mismatch"),
                Some(&1),
                "{level}: {:?}",
                t[level]
            );
            assert_eq!(
                t[level].n5.get("grid_snapped"),
                Some(&1),
                "{level}: {:?}",
                t[level]
            );
            assert_eq!(t[level].false_labels, 1, "{level}");
        }
    }
}

/// **N3 is scored separately**: an engine over-claiming only on out-of-catalogue structure spends
/// none of the pooled false-label budget, and still fails on N3's own budget.
#[test]
fn negative_controls_score_n3_separately_from_the_pooled_budget() {
    let jobs = stub_jobs();
    let t = judge(
        &jobs,
        &run_all(
            &OverClaimer {
                only: Some(Population::N3),
            },
            &jobs,
        ),
    );
    let fails = budget(&t);
    assert!(
        !fails.iter().any(|f| f.contains("over N1 u N2 u N4 u N5")),
        "N3 labels leaked into the pooled budget: {fails:#?}"
    );
    assert!(
        fails.iter().any(|f| f.contains("3 solved on N3")),
        "{fails:#?}"
    );
}

/// **A miss is not a false label**: an engine that finds nothing passes the budget — which is
/// exactly why this suite needs the P rows' recall control beside it (docs/22 §6.2 item 1) — and
/// N5 reads it as `abstain`, the wanted answer, never as a mismatch.
#[test]
fn negative_controls_a_silent_engine_spends_no_budget_and_n5_reads_it_as_abstain() {
    struct Silent;
    impl Engine for Silent {
        fn name(&self) -> &'static str {
            "stub: finds nothing"
        }
        fn run(&self, _: &Job) -> Answer {
            Answer::default()
        }
    }
    let jobs = stub_jobs();
    let t = judge(&jobs, &run_all(&Silent, &jobs));
    if hk_e2e::synth::find_uv().is_none() {
        return;
    }
    assert!(budget(&t).is_empty(), "{:#?}", budget(&t));
    assert_eq!(t["n5-off-grid"].n5.get("abstain"), Some(&1));
}
