//! The M-3 search engine (ADR-0015 §3, ADR-0021 §1–§3, T-854), driven through its only seam, the
//! `Evaluator`, over a synthetic world whose **truth the engine never sees**: the evaluator scores
//! each prefix against a hidden FSK signal (symbol rate, line code, sync word, CRC polynomial) the
//! way M-2's blocks will — bits of significance per stage — and the tests assert on what the
//! engine reports: `PipelineResult`s (ADR-0015 §3.4), the `TraceNode`s (ADR-0021 §2), the
//! `Coverage` and the `reason` (ADR-0021 §7A).
//!
//! Every candidate the engine builds is checked against the real `hk_blocks::Registry::builtin()`
//! catalogue, so "a candidate is a recipe" (§1.2) holds for every result here.
//!
//! Use case: RESEARCH-002 (a never-seen FSK/OOK sensor decoded blind) at engine level — the IQ
//! path arrives with M-2/M-6 and the full blind suite is M-12's.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use hk_blocks::Registry;
use hk_model::ContentClass;
use hk_recipe::{InputSpec, OutputPolicy, PortType};
use hk_synth::engine::{
    EvalError, EvalRequest, EvalWindow, Evaluated, Evaluator, NodeEvidence, Progress,
    ProposalReply, ProposeRequest, RecipeHead, Root, SearchOutcome, SearchSpec, Suggestion,
    UnsupportedStructure, search,
};
use hk_synth::result::{CheckOrigin, CheckSummary, HoldoutFrame};
use hk_synth::search::{JobState, Profile, StopReason};
use hk_synth::trace::{Outcome, OutcomeKind, Reason};
use hk_synth::{
    Control, Evidence, EvidenceSet, GroupId, MetricId, PowerPolicy, ProposalOp, Skeleton, Stage,
    Verdict,
};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------------------------
// The hidden world
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Truth {
    /// S0 in-band SNR significance, bits.
    s0_bits: f32,
    /// `None`: no modulation at all (a bare carrier or noise).
    family: Option<&'static str>,
    rate_bd: f64,
    line: &'static str,
    sync: &'static str,
    poly: &'static str,
}

const FSK: Truth = Truth {
    s0_bits: 10.0,
    family: Some("fsk"),
    rate_bd: 4812.0,
    line: "nrzi",
    sync: "0x2DD4",
    poly: "0x8005",
};

/// What a stage hands its children: whether the prefix so far is still on the truth.
#[derive(Clone, Copy, Debug)]
struct Sig {
    on_truth: bool,
}

type Hook = Box<dyn Fn(u64) + Send + Sync>;

struct World {
    truth: Truth,
    calls: AtomicU64,
    holdout_calls: AtomicU64,
    /// Evaluations over ADR-0021 §8.2's null windows.
    null_calls: AtomicU64,
    /// The null windows carry the "signal" too: structure the search finds in structureless data.
    null_fits: bool,
    /// Valid frames on hold-out at the true CRC (the check's `n`, and its `raw` too unless
    /// `holdout_payloads` says the payloads repeat).
    holdout_frames: u32,
    /// `Some(k)`: those frames carry only `k` distinct payloads — a beacon repeating itself. The
    /// S5 record then says `raw = k` (ADR-0022 §4.2's `differences`) over `n = holdout_frames`,
    /// while its `bits` still claim every frame (an evaluator over-reporting).
    holdout_payloads: Option<u32>,
    /// The CRC's width, as the check summary reports it.
    width: u32,
    proposals: AtomicU64,
    grants: Mutex<Vec<u64>>,
    hook: Option<Hook>,
}

impl World {
    fn new(truth: Truth) -> Self {
        Self {
            truth,
            calls: AtomicU64::new(0),
            holdout_calls: AtomicU64::new(0),
            null_calls: AtomicU64::new(0),
            null_fits: false,
            holdout_frames: 12,
            holdout_payloads: None,
            width: 16,
            proposals: AtomicU64::new(0),
            grants: Mutex::new(Vec::new()),
            hook: None,
        }
    }

    fn with_hook(mut self, hook: impl Fn(u64) + Send + Sync + 'static) -> Self {
        self.hook = Some(Box::new(hook));
        self
    }
}

fn ev(stage: Stage, metric: MetricId, group: GroupId, raw: f32, n: u32, bits: f32) -> EvidenceSet {
    let mut s = EvidenceSet::new();
    s.push(Evidence::new(stage, metric, group, raw, n, bits))
        .unwrap();
    s
}

impl Evaluator for World {
    type Output = Sig;

    fn evaluate(&self, req: &EvalRequest<'_, Sig>) -> Result<Evaluated<Sig>, EvalError> {
        let k = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if req.window == EvalWindow::Holdout {
            self.holdout_calls.fetch_add(1, Ordering::SeqCst);
        }
        let null = matches!(req.window, EvalWindow::Null(_));
        if null {
            self.null_calls.fetch_add(1, Ordering::SeqCst);
        }
        if let Some(h) = &self.hook {
            h(k);
        }
        let t = &self.truth;
        // A phase-randomised or time-reversed null keeps the PSD (S0) and destroys everything
        // after it — unless this world plants structure in the nulls too.
        let upstream = req.parent.is_none_or(|p| p.on_truth)
            && !(null && req.stage > Stage::S0 && !self.null_fits);
        let nodes = &req.candidate.recipe.nodes[req.new_nodes.clone()];
        let last = nodes.last().expect("every alternative here has nodes");
        let p = |name: &str| last.params.get(name).cloned().unwrap_or(Value::Null);
        let holdout = req.window == EvalWindow::Holdout;
        let (set, on_truth) = match req.stage {
            Stage::S0 => (
                ev(
                    Stage::S0,
                    MetricId::Snr,
                    GroupId::Undeclared,
                    t.s0_bits,
                    4096,
                    t.s0_bits,
                ),
                t.s0_bits >= 6.0,
            ),
            Stage::S1 => {
                let family = match last.block.as_str() {
                    "fsk_demod" => "fsk",
                    "am_demod" => "ook",
                    _ => "?",
                };
                let hit = upstream && t.family == Some(family);
                let bits = if hit { 9.0 } else { 1.5 };
                (
                    ev(
                        Stage::S1,
                        MetricId::Bimodality,
                        GroupId::DemodShape,
                        0.7,
                        4096,
                        bits,
                    ),
                    hit,
                )
            }
            Stage::S2 => {
                let rate = p("symbol_rate_bd").as_f64().unwrap_or(0.0);
                let err = (rate / t.rate_bd).ln().abs();
                let bits = if upstream {
                    (11.0 * (-(err / 0.01).powi(2)).exp()) as f32
                } else {
                    0.5
                };
                let mut s = ev(Stage::S2, MetricId::EyeOpen, GroupId::Eye, 0.5, 512, bits);
                s.push(Evidence::new(
                    Stage::S2,
                    MetricId::TimingVar,
                    GroupId::SoftQuality,
                    0.1,
                    512,
                    bits * 0.5,
                ))
                .unwrap();
                (s, upstream && err < 0.015)
            }
            Stage::S3 => {
                let line = match last.block.as_str() {
                    "nrzi" => "nrzi",
                    "manchester" => "manchester",
                    _ => "none",
                };
                let hit = upstream && line == t.line;
                let bits = if hit { 8.0 } else { 2.0 };
                (
                    ev(
                        Stage::S3,
                        MetricId::BitStructure,
                        GroupId::BitShape,
                        0.5,
                        512,
                        bits,
                    ),
                    hit,
                )
            }
            Stage::S4 => {
                let hit = upstream && p("sync_word").as_str() == Some(t.sync);
                let bits = if hit { 24.0 } else { 1.0 };
                (
                    ev(
                        Stage::S4,
                        MetricId::SyncExcess,
                        GroupId::Undeclared,
                        30.0,
                        30,
                        bits,
                    ),
                    hit,
                )
            }
            Stage::S5 => {
                let hit = upstream && p("poly").as_str() == Some(t.poly);
                let frames = match (hit, holdout || null) {
                    (true, false) => 20,
                    (true, true) => self.holdout_frames,
                    _ => 0,
                };
                let distinct = match (holdout || null, self.holdout_payloads) {
                    (true, Some(k)) => k.min(frames),
                    _ => frames,
                };
                (
                    ev(
                        Stage::S5,
                        MetricId::CheckDistinctValid,
                        GroupId::Undeclared,
                        distinct as f32,
                        frames,
                        self.width as f32 * frames as f32,
                    ),
                    hit,
                )
            }
            Stage::S6 => (EvidenceSet::new(), upstream),
        };
        let check = (req.stage == Stage::S5).then(|| CheckSummary {
            kind: "crc".into(),
            model: format!("CRC-{}", self.width),
            width: self.width,
            pass_rate: if on_truth { 1.0 } else { 0.0 },
            distinct_valid: if on_truth { self.holdout_frames } else { 0 },
            corrected_excluded: 0,
            tested: self.holdout_frames.max(1),
            holdout,
        });
        let frames = if holdout && on_truth && req.stage == Stage::S5 {
            (0..self.holdout_frames)
                .map(|i| HoldoutFrame {
                    t_ns: 1_000_000 * i64::from(i),
                    check_valid: true,
                    corrected: false,
                    frame_model: "hk-framing".into(),
                    identity: None,
                    metadata: json!({ "len": 64 }),
                    content: None,
                })
                .collect()
        } else {
            Vec::new()
        };
        Ok(Evaluated {
            evidence: vec![NodeEvidence {
                node: last.id.clone(),
                evidence: set,
            }],
            output: Sig { on_truth },
            output_bytes: 1024,
            check,
            frames,
        })
    }

    fn propose(&self, req: &ProposeRequest<'_, Sig>) -> Result<ProposalReply, EvalError> {
        self.proposals.fetch_add(1, Ordering::SeqCst);
        self.grants.lock().unwrap().push(req.budget.max_ops);
        assert_eq!(req.op, ProposalOp::Sync);
        // The operator finds the true word in the node's bits — and a decoy it happens to like
        // better. The decoy's better prior must not win: only measured evidence ranks (§3.2).
        let mut suggestions = vec![Suggestion {
            bind: BTreeMap::from([(req.path.to_owned(), json!("0x1234"))]),
            prior_bits: 0.0,
        }];
        if req.parent.is_some_and(|p| p.on_truth) {
            suggestions.push(Suggestion {
                bind: BTreeMap::from([(req.path.to_owned(), json!(self.truth.sync))]),
                prior_bits: -1.0,
            });
        }
        Ok(ProposalReply {
            suggestions,
            ops: 1_000,
            hypotheses: 1 << 16,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Skeletons, all in real ADR-0011 blocks
// ---------------------------------------------------------------------------------------------

fn skeleton(id: &str, s1: Value, with_s0: bool, s5_polys: &[&str]) -> Skeleton {
    let mut slots = json!({
        "S1": [ s1 ],
        "S2": [ { "id": "clock", "nodes": [ { "id": "clock", "block": "clock_recovery",
                   "params": { "pulse": "nrz", "algorithm": "gardner" } } ] } ],
        "S3": [ { "id": "none", "nodes": [ { "id": "slice", "block": "slicer" } ] },
                { "id": "nrzi", "nodes": [ { "id": "slice", "block": "slicer" },
                                            { "id": "line", "block": "nrzi" } ] },
                { "id": "manchester", "nodes": [ { "id": "slice", "block": "slicer" },
                                                  { "id": "line", "block": "manchester" } ] } ],
        "S4": [ { "id": "sync", "nodes": [ { "id": "sync", "block": "sync_search",
                   "params": { "mode": "sync-word", "sync_bits": 16, "frame_bits": 64 } } ] } ],
    });
    if !s5_polys.is_empty() {
        slots["S5"] = json!([ { "id": "crc16", "nodes": [ { "id": "crc", "block": "crc",
                                "params": { "width": 16 } } ] } ]);
    }
    if with_s0 {
        slots["S0"] =
            json!([ { "id": "chan", "nodes": [ { "id": "chan", "block": "identity" } ] } ]);
    }
    serde_json::from_value(json!({ "id": id, "version": 1, "slots": slots })).unwrap()
}

fn fsk_s1() -> Value {
    json!({ "id": "fsk", "family": "fsk", "nodes": [ { "id": "demod", "block": "fsk_demod" } ] })
}

fn ook_s1() -> Value {
    json!({ "id": "ook", "family": "ook", "nodes": [ { "id": "demod", "block": "am_demod" } ] })
}

fn free(polys: &[&str]) -> Vec<hk_synth::candidate::FreeParam> {
    let mut v: Vec<hk_synth::candidate::FreeParam> = vec![
        serde_json::from_value(json!({
            "path": "nodes[clock].params.symbol_rate_bd",
            "domain": { "float": { "lo": 300, "hi": 50000, "scale": "log" } },
            "seed": 4700, "source": "estimate" }))
        .unwrap(),
        serde_json::from_value(json!({
            "path": "nodes[sync].params.sync_word", "domain": { "proposal": "assist.sync" } }))
        .unwrap(),
    ];
    if !polys.is_empty() {
        v.push(
            serde_json::from_value(json!({
                "path": "nodes[crc].params.poly", "domain": { "hex": { "candidates": polys } } }))
            .unwrap(),
        );
    }
    v
}

const POLYS: [&str; 3] = ["0x1021", "0x8005", "0x3D65"];

fn root(sk: Skeleton, prior_bits: f32, polys: &[&str]) -> Root {
    Root {
        skeleton: sk,
        template: None,
        free: free(polys),
        prior_bits,
        seed_source: hk_synth::candidate::SeedSource::Open,
        family: None,
        deferred: None,
        check_origin: CheckOrigin::Searched,
    }
}

fn head() -> RecipeHead {
    RecipeHead {
        input: InputSpec {
            port: PortType::Iq,
            sample_rate_hz: Some(48_000.0),
            bandwidth_hz: Some(20_000.0),
            channels: Default::default(),
            liveness: None,
        },
        output_policy: OutputPolicy {
            content_class: ContentClass::Unrestricted,
            metadata_keys: None,
            frame_models: Vec::new(),
            identity: None,
        },
        field_maps: BTreeMap::new(),
    }
}

fn spec(roots: Vec<Root>, profile: Profile) -> SearchSpec {
    let mut s = SearchSpec::new(head(), roots, profile);
    // Count-bounded and wall-free, so every run here is exactly reproducible (ADR-0021 §5).
    s.budget.wall_s = 600.0;
    s.budget.cpu_s = 600.0;
    s.budget.max_evaluations = Some(4_000);
    s
}

fn standard_roots() -> Vec<Root> {
    vec![
        root(
            skeleton("generic-fsk-framed", fsk_s1(), false, &POLYS),
            -0.5,
            &POLYS,
        ),
        root(
            skeleton("generic-ook-framed", ook_s1(), false, &POLYS),
            -1.0,
            &POLYS,
        ),
    ]
}

fn run(spec: &SearchSpec, world: &World, control: &Control) -> SearchOutcome {
    search(spec, &Registry::builtin(), world, control, &mut ())
}

/// Invariants every outcome must satisfy, whatever it found.
fn check_outcome(o: &SearchOutcome, world: &World) {
    // ADR-0021 §1: the trace names a subset of the decisions and accounts for all of the work.
    assert_eq!(
        o.trace.evaluations(),
        o.used.evaluations,
        "accounting identity"
    );
    // …and the engine counted every call it made: no uncharged work.
    assert_eq!(world.calls.load(Ordering::SeqCst), o.used.evaluations);
    assert_eq!(
        world.proposals.load(Ordering::SeqCst),
        o.used.proposal_calls
    );
    let ids: std::collections::BTreeSet<&str> =
        o.trace.nodes.iter().map(|n| n.id.as_str()).collect();
    for n in &o.trace.nodes {
        n.check().unwrap_or_else(|e| panic!("{}: {e:?}", n.id));
        // Lineage is never dropped: a retained node's parent is retained.
        if let Some(p) = &n.parent {
            assert!(
                ids.contains(p.as_str()),
                "{} names dropped parent {p}",
                n.id
            );
        }
    }
    let reg = Registry::builtin();
    for r in &o.results {
        // A result is an ordinary, runnable recipe (§1.2).
        let c = hk_synth::Candidate {
            skeleton: String::new(),
            choices: BTreeMap::new(),
            recipe: r.recipe.clone(),
            free: Vec::new(),
        };
        c.check_prefix(&reg)
            .unwrap_or_else(|e| panic!("result {} is not a recipe: {e}", r.rank));
    }
    assert!(o.trace_cost.fraction >= 0.0 && o.trace_cost.fraction <= 1.0);
    assert!(o.trace_cost.bytes > 0 || o.trace.nodes.is_empty());
    // ADR-0021 §5: only a not-tried node whose cause was time or an external event is flagged,
    // and any flagged node makes the whole job non-replayable.
    for n in o.trace.nodes.iter().filter(|n| n.nondeterministic) {
        assert!(
            matches!(
                n.outcome.kind(),
                OutcomeKind::DeferredBudget | OutcomeKind::RefusedPower
            ),
            "{} flagged as {:?}",
            n.id,
            n.outcome.kind()
        );
        assert!(
            o.nondeterministic,
            "{} flagged but the job is replayable",
            n.id
        );
    }
    // ADR-0021 §2.3: the peak (sampled after every insert) is never below the end state.
    assert!(o.trace.peak_nodes >= o.trace.nodes.len() as u64);
    assert!(o.trace.peak_bytes >= o.trace.bytes);
    assert_eq!(o.replay_key.engine, hk_synth::ENGINE);
    assert_eq!(o.replay_key.seed_ref.len(), 64);
}

fn param<'a>(r: &'a hk_synth::PipelineResult, node: &str, name: &str) -> &'a Value {
    &r.recipe
        .nodes
        .iter()
        .find(|n| n.id == node)
        .unwrap_or_else(|| panic!("no node {node}"))
        .params[name]
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[test]
fn blind_search_solves_the_hidden_fsk_signal() {
    let world = World::new(FSK);
    let mut s = spec(standard_roots(), Profile::Standard);
    s.unsupported.push(UnsupportedStructure {
        structure: "css".into(),
        missing_block: "css_demod".into(),
        reference: "ADR-0011 §1.5".into(),
        family: Some("css".into()),
        slot: Stage::S1,
        posterior: Some(0.05),
    });
    let o = run(&s, &world, &Control::new());
    check_outcome(&o, &world);

    assert_eq!(o.state, JobState::Done);
    assert_eq!(o.stop, Some(StopReason::Solved));
    assert_eq!(o.reason, None, "a solved search has no negative reason");
    assert!(!o.nondeterministic);
    let top = &o.results[0];
    assert_eq!(top.verdict, Verdict::Solved);
    assert_eq!(top.stage_reached, Stage::S5);
    // The measured structure, not the seed's and not the operator's favourite.
    let rate = param(top, "clock", "symbol_rate_bd").as_f64().unwrap();
    assert!(
        (rate / FSK.rate_bd - 1.0).abs() < 0.01,
        "symbol rate within 1 %: {rate}"
    );
    assert_eq!(
        top.recipe
            .nodes
            .iter()
            .find(|n| n.id == "line")
            .unwrap()
            .block,
        "nrzi"
    );
    assert_eq!(
        param(top, "sync", "sync_word"),
        &json!(FSK.sync),
        "evidence beat the decoy's prior"
    );
    assert_eq!(param(top, "crc", "poly"), &json!(FSK.poly));
    // Only hold-out evidence solves: the winner was re-run on the hold-out window.
    assert!(world.holdout_calls.load(Ordering::SeqCst) >= 5);
    // The evidence ladder covers every stage of the chain.
    let stages: Vec<Stage> = top.stages.iter().map(|e| e.stage).collect();
    for st in [Stage::S1, Stage::S2, Stage::S3, Stage::S4, Stage::S5] {
        assert!(stages.contains(&st), "{st:?} missing from the ladder");
    }

    // The trace says why the alternatives lost, and keeps tried apart from not-tried.
    let nodes = &o.trace.nodes;
    let ook = nodes
        .iter()
        .find(|n| n.hypothesis.family.as_deref() == Some("ook") && n.stage == Stage::S1)
        .expect("the OOK hypothesis is in the trace");
    assert!(
        matches!(ook.outcome, Outcome::PrunedFloor { .. }),
        "{:?}",
        ook.outcome
    );
    assert!(ook.tried && ook.measured.is_some());
    let css = nodes
        .iter()
        .find(|n| matches!(n.outcome, Outcome::Unsupported { .. }))
        .expect("the unsupported suspicion is recorded");
    assert!(!css.tried && css.measured.is_none());
    // The decoy sync word was tried and measured, then lost on evidence.
    let decoy = nodes
        .iter()
        .find(|n| n.hypothesis.params.get("nodes[sync].params.sync_word") == Some(&json!("0x1234")))
        .expect("the decoy was evaluated");
    assert!(decoy.tried);
    assert!(matches!(decoy.outcome, Outcome::PrunedFloor { .. }));
    // Continuous sweeps collapse onto their node (ADR-0021 §1 rule 2).
    let clock = nodes
        .iter()
        .find(|n| n.stage == Stage::S2 && n.tried && !n.hypothesis.swept.is_empty())
        .unwrap();
    assert!(clock.evaluations > 1 && clock.hypothesis.swept[0].points > 1);
    // Coverage: two skeletons offered and tried, one unsupported; the FSK family reached S5.
    assert_eq!(o.coverage.skeletons.offered, 2);
    assert_eq!(o.coverage.skeletons.tried, 2);
    assert_eq!(o.coverage.skeletons.unsupported, 1);
    let fsk = o
        .coverage
        .families
        .iter()
        .find(|f| f.family == "fsk")
        .unwrap();
    assert_eq!(fsk.deepest_stage, Some(Stage::S5));
    assert!(o.used.proposal_calls >= 1);
    assert!(
        o.used.hypotheses >= 1 << 16,
        "the operator's hypotheses pay look-elsewhere"
    );
}

#[test]
fn the_same_spec_gives_the_same_trace_at_any_thread_count() {
    let outcome = |threads: u32| {
        let world = World::new(FSK);
        let mut s = spec(standard_roots(), Profile::Standard);
        s.budget.threads = threads;
        let o = run(&s, &world, &Control::new());
        check_outcome(&o, &world);
        o
    };
    let (a, b) = (outcome(1), outcome(4));
    assert!(!a.nondeterministic && !b.nondeterministic);
    let strip = |o: &SearchOutcome| {
        o.trace
            .nodes
            .iter()
            .map(|n| {
                let mut n = n.clone();
                n.cpu_ms = 0;
                serde_json::to_value(&n).unwrap()
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(strip(&a), strip(&b));
    assert_eq!(a.results, b.results);
    assert_eq!(a.used.evaluations, b.used.evaluations);
}

#[test]
fn shared_prefixes_are_memoised_not_paid_twice() {
    // Two skeletons identical through S4, differing only at S5's candidates.
    let world = World::new(FSK);
    let roots = vec![
        root(skeleton("fsk-a", fsk_s1(), false, &POLYS), -0.5, &POLYS),
        root(
            skeleton("fsk-b", fsk_s1(), false, &["0x8005"]),
            -0.5,
            &["0x8005"],
        ),
    ];
    let mut s = spec(roots, Profile::Deep);
    s.solve.min_analytic_holdout_bits = 1e9; // never solves: both searches run to the end
    let o = run(&s, &world, &Control::new());
    check_outcome(&o, &world);
    let memo: Vec<_> = o
        .trace
        .nodes
        .iter()
        .filter(|n| n.outcome.kind() == OutcomeKind::Memoised)
        .collect();
    assert!(
        !memo.is_empty(),
        "the second skeleton's shared prefix is a memoised hit"
    );
    for m in memo {
        assert_eq!(m.evaluations, 0, "a memoised hit does no new work");
        assert!(m.measured.is_none());
    }
    assert_eq!(o.stop, Some(StopReason::Exhausted));
}

#[test]
fn a_tight_evaluation_budget_stops_and_says_what_it_did_not_try() {
    let world = World::new(FSK);
    let mut s = spec(standard_roots(), Profile::Quick);
    s.budget.max_evaluations = Some(20);
    let o = run(&s, &world, &Control::new());
    check_outcome(&o, &world);
    assert_eq!(o.stop, Some(StopReason::Budget));
    assert!(o.coverage.budget.exhausted);
    assert!(
        o.used.evaluations <= 20,
        "the cap holds: {}",
        o.used.evaluations
    );
    let not_tried: Vec<_> = o.trace.nodes.iter().filter(|n| !n.tried).collect();
    assert!(!not_tried.is_empty());
    for n in &not_tried {
        assert!(matches!(
            n.outcome,
            Outcome::DeferredBudget {
                stop: StopReason::Budget,
                ..
            }
        ));
        assert!(n.measured.is_none() && n.evidence_bits.is_none());
    }
    assert_eq!(o.reason, Some(Reason::BudgetExhausted));
    assert!(
        !o.results.is_empty(),
        "there are always ranked partial results"
    );
    assert_ne!(o.results[0].verdict, Verdict::Solved);
}

#[test]
fn proposal_calls_are_budgeted_by_count_and_share_the_op_pool() {
    // One call allowed: it is granted its share of the op pool, and it is enough.
    let world = World::new(FSK);
    let mut s = spec(standard_roots(), Profile::Standard);
    s.budget.max_proposal_calls = Some(1);
    s.budget.max_assist_ops = Some(10_000);
    let o = run(&s, &world, &Control::new());
    check_outcome(&o, &world);
    assert_eq!(o.used.proposal_calls, 1);
    assert!(o.used.assist_ops <= 10_000);
    assert_eq!(*world.grants.lock().unwrap(), [10_000]);
    assert_eq!(o.stop, Some(StopReason::Solved));

    // None allowed: the S4 alternative that needs the operator is not tried, and the search
    // stops on budget — however much evaluation budget is left.
    let world = World::new(FSK);
    s.budget.max_proposal_calls = Some(0);
    let o = run(&s, &world, &Control::new());
    check_outcome(&o, &world);
    assert_eq!(o.used.proposal_calls, 0);
    assert_eq!(o.stop, Some(StopReason::Budget));
    let s4 = o
        .trace
        .nodes
        .iter()
        .find(|n| n.stage == Stage::S4)
        .expect("the S4 hypothesis is recorded");
    assert!(!s4.tried, "not tried is not ruled out");
    assert!(o.used.evaluations < 100);
    assert_eq!(o.results[0].verdict, Verdict::Clocked);
}

#[test]
fn noise_is_no_signal_and_a_bare_carrier_is_nothing_scored() {
    let roots = || {
        vec![
            root(skeleton("fsk-s0", fsk_s1(), true, &POLYS), -0.5, &POLYS),
            root(skeleton("ook-s0", ook_s1(), true, &POLYS), -1.0, &POLYS),
        ]
    };
    let noise = World::new(Truth {
        s0_bits: 1.0,
        family: None,
        ..FSK
    });
    let o = run(&spec(roots(), Profile::Standard), &noise, &Control::new());
    check_outcome(&o, &noise);
    assert_eq!(o.stop, Some(StopReason::Exhausted));
    assert_eq!(o.reason, Some(Reason::NoSignal));
    assert!(o.results.iter().all(|r| r.verdict != Verdict::Solved));
    assert_eq!(o.results[0].verdict, Verdict::Energy);

    let carrier = World::new(Truth {
        s0_bits: 11.0,
        family: None,
        ..FSK
    });
    let o = run(&spec(roots(), Profile::Standard), &carrier, &Control::new());
    check_outcome(&o, &carrier);
    assert_eq!(o.stop, Some(StopReason::Exhausted));
    assert_eq!(
        o.reason,
        Some(Reason::NothingScored),
        "not merged with no-signal"
    );
}

#[test]
fn cancel_keeps_partial_results_and_rules_nothing_out() {
    let control = Arc::new(Control::new());
    let c2 = Arc::clone(&control);
    let world = World::new(FSK).with_hook(move |k| {
        if k == 20 {
            c2.cancel();
        }
    });
    let o = run(&spec(standard_roots(), Profile::Standard), &world, &control);
    check_outcome(&o, &world);
    assert_eq!(o.state, JobState::Cancelled);
    assert_eq!(o.stop, Some(StopReason::Cancelled));
    assert_eq!(
        o.reason, None,
        "an aborted look writes not-searched, never unknown"
    );
    assert!(!o.results.is_empty());
    assert!(o.trace.nodes.iter().any(|n| matches!(
        n.outcome,
        Outcome::DeferredBudget {
            stop: StopReason::Cancelled,
            ..
        }
    )));
}

#[test]
fn capture_loss_throttles_the_job_before_capture_is_hurt() {
    let control = Arc::new(Control::new());
    let c2 = Arc::clone(&control);
    let world = World::new(FSK).with_hook(move |k| {
        if k == 5 {
            c2.set_lost_samples(128);
        }
    });
    let mut s = spec(standard_roots(), Profile::Deep);
    s.budget.threads = 4;
    let states = Mutex::new(Vec::new());
    let mut obs = |p: &Progress| states.lock().unwrap().push(p.state);
    let o = search(&s, &Registry::builtin(), &world, &control, &mut obs);
    check_outcome(&o, &world);
    assert_eq!(o.throttle_events, 1);
    assert!(o.nondeterministic);
    assert!(states.lock().unwrap().contains(&Some(JobState::Throttled)));
    // Throttling slows the search; it does not change what is found.
    assert_eq!(o.stop, Some(StopReason::Solved));
}

#[test]
fn switching_to_battery_refuses_the_rest_of_a_deep_job() {
    let control = Arc::new(Control::new());
    let c2 = Arc::clone(&control);
    let world = World::new(FSK).with_hook(move |k| {
        if k == 20 {
            c2.set_power(PowerPolicy::Battery);
        }
    });
    let o = run(&spec(standard_roots(), Profile::Deep), &world, &control);
    check_outcome(&o, &world);
    assert_eq!(o.refused_power, Some(PowerPolicy::Battery));
    assert_eq!(o.stop, Some(StopReason::Budget));
    let refused: Vec<_> = o
        .trace
        .nodes
        .iter()
        .filter(|n| matches!(&n.outcome, Outcome::RefusedPower { policy } if policy == "battery"))
        .collect();
    assert!(!refused.is_empty());
    assert!(refused.iter().all(|n| !n.tried));
}

#[test]
fn a_thermal_flag_pauses_expansion_until_it_clears() {
    let control = Arc::new(Control::new());
    let c2 = Arc::clone(&control);
    let world = World::new(FSK).with_hook(move |k| {
        if k == 10 {
            c2.set_thermal(true);
            let c3 = Arc::clone(&c2);
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(20));
                c3.set_thermal(false);
            });
        }
    });
    let states = Mutex::new(Vec::new());
    let mut obs = |p: &Progress| states.lock().unwrap().push(p.state);
    let s = spec(standard_roots(), Profile::Standard);
    let o = search(&s, &Registry::builtin(), &world, &control, &mut obs);
    check_outcome(&o, &world);
    assert!(states.lock().unwrap().contains(&Some(JobState::Throttled)));
    assert_eq!(
        o.stop,
        Some(StopReason::Solved),
        "the pause delays, it does not stop"
    );
}

#[test]
fn a_deferred_family_waits_in_the_side_queue() {
    let mut deferred = root(
        skeleton("generic-ook-framed", ook_s1(), false, &POLYS),
        -6.0,
        &POLYS,
    );
    deferred.deferred = Some(0.01);
    deferred.family = Some("ook".into());
    // Main pass solves → the side queue is never reached: deferred_prior, not tried.
    let world = World::new(FSK);
    let roots = vec![
        root(
            skeleton("generic-fsk-framed", fsk_s1(), false, &POLYS),
            -0.5,
            &POLYS,
        ),
        deferred.clone(),
    ];
    let o = run(
        &spec(roots.clone(), Profile::Standard),
        &world,
        &Control::new(),
    );
    check_outcome(&o, &world);
    let d = o
        .trace
        .nodes
        .iter()
        .find(|n| matches!(n.outcome, Outcome::DeferredPrior { .. }))
        .expect("the deferred family is recorded, not deleted");
    assert!(!d.tried);
    assert_eq!(d.hypothesis.family.as_deref(), Some("ook"));
    let ook = o
        .coverage
        .families
        .iter()
        .find(|f| f.family == "ook")
        .unwrap();
    assert_eq!(ook.deferred_as, Some(OutcomeKind::DeferredPrior));

    // Nothing solves → the main pass exhausts and the side queue runs after it.
    let world = World::new(Truth {
        s0_bits: 11.0,
        family: None,
        ..FSK
    });
    let o = run(&spec(roots, Profile::Standard), &world, &Control::new());
    check_outcome(&o, &world);
    let first_ook = o
        .trace
        .nodes
        .iter()
        .filter(|n| n.hypothesis.family.as_deref() == Some("ook") && n.tried)
        .map(|n| n.id[1..].parse::<u32>().unwrap())
        .min()
        .expect("the side queue ran");
    let last_fsk = o
        .trace
        .nodes
        .iter()
        .filter(|n| n.hypothesis.skeleton == "generic-fsk-framed@1" && n.tried)
        .map(|n| n.id[1..].parse::<u32>().unwrap())
        .max()
        .unwrap();
    assert!(
        first_ook > last_fsk,
        "deferred runs only after the main pass"
    );
}

#[test]
fn the_trace_stays_within_its_bound_and_is_complete_in_counts() {
    // Many hypotheses: 16 hex polynomials × every surviving S4 node, at quick's 128-node bound.
    let polys: Vec<String> = (0..120).map(|i| format!("0x{:04X}", 0x1000 + i)).collect();
    let mut polys: Vec<&str> = polys.iter().map(String::as_str).collect();
    polys.push(FSK.poly);
    let roots = vec![
        root(skeleton("fsk-a", fsk_s1(), false, &polys), -0.5, &polys),
        root(skeleton("fsk-b", fsk_s1(), false, &polys), -0.6, &polys),
        root(skeleton("ook-a", ook_s1(), false, &polys), -1.0, &polys),
    ];
    let world = World::new(FSK);
    let mut s = spec(roots, Profile::Quick);
    s.budget.max_proposal_calls = Some(100);
    s.solve.min_analytic_holdout_bits = 1e9;
    let bound = Profile::Quick.trace_bounds().max_trace_nodes as usize;
    let o = run(&s, &world, &Control::new());
    check_outcome(&o, &world);
    if !o.trace.over_bound {
        assert!(
            o.trace.nodes.len() <= bound,
            "{} > {bound}",
            o.trace.nodes.len()
        );
    }
    assert!(o.trace.truncated && o.trace.nodes_elided > 0);
    // Every not-tried node survives the bound.
    let evaluated: u64 = o.trace.nodes.iter().map(|n| n.evaluations).sum();
    assert!(
        evaluated < o.used.evaluations,
        "some work is only in elided counts"
    );
    assert!(o.trace.elided.iter().all(|e| e.outcome.tried()));
}

#[test]
fn memoised_prefixes_survive_cache_eviction_by_recomputing() {
    // A 1-byte cache holds nothing: every parent output is recomputed, and paid for.
    let world = World::new(FSK);
    let mut s = spec(standard_roots(), Profile::Standard);
    s.budget.max_cache_bytes = 1;
    let o = run(&s, &world, &Control::new());
    check_outcome(&o, &world);
    assert_eq!(o.stop, Some(StopReason::Solved));
    assert_eq!(o.used.cache_bytes, 0);
    let cached = {
        let w = World::new(FSK);
        let o = run(
            &spec(standard_roots(), Profile::Standard),
            &w,
            &Control::new(),
        );
        o.used.evaluations
    };
    assert!(
        o.used.evaluations > cached,
        "recomputation is charged, not free"
    );
}

// ---------------------------------------------------------------------------------------------
// T-565: the trace, produced inside the beam (ADR-0021 §1–§3, §5)
// ---------------------------------------------------------------------------------------------

/// Many hypotheses: `polys` CRC polynomials under every surviving S4 node of three skeletons,
/// never solving, so the whole space is walked and the trace bound has to bite.
fn crowded_spec(n_polys: usize, profile: Profile) -> SearchSpec {
    let polys: Vec<String> = (0..n_polys)
        .map(|i| format!("0x{:04X}", 0x1000 + i))
        .collect();
    let mut polys: Vec<&str> = polys.iter().map(String::as_str).collect();
    polys.push(FSK.poly);
    let roots = vec![
        root(skeleton("fsk-a", fsk_s1(), false, &polys), -0.5, &polys),
        root(skeleton("fsk-b", fsk_s1(), false, &polys), -0.6, &polys),
        root(skeleton("ook-a", ook_s1(), false, &polys), -1.0, &polys),
    ];
    let mut s = spec(roots, profile);
    s.budget.max_proposal_calls = Some(100);
    s.budget.max_evaluations = Some(20_000);
    s.solve.min_analytic_holdout_bits = 1e9;
    s
}

fn unsupported(structure: &str, block: &str) -> UnsupportedStructure {
    UnsupportedStructure {
        structure: structure.into(),
        missing_block: block.into(),
        reference: "ADR-0011 §1.5".into(),
        family: Some(structure.into()),
        slot: Stage::S1,
        posterior: Some(0.05),
    }
}

/// The trace minus its only non-deterministic field (`cpu_ms` is measured time).
fn deterministic_bytes(o: &SearchOutcome) -> Vec<u8> {
    let nodes: Vec<_> = o
        .trace
        .nodes
        .iter()
        .map(|n| {
            let mut n = n.clone();
            n.cpu_ms = 0;
            n
        })
        .collect();
    serde_json::to_vec(&(&nodes, &o.trace.elided, o.trace.nodes_elided)).unwrap()
}

#[test]
fn the_trace_accounts_for_all_of_the_work() {
    // ADR-0021 §1's identity, with the bound biting so the elided side is not empty: the trace
    // names a subset of the decisions and accounts for ALL of the work.
    let world = World::new(FSK);
    let o = run(&crowded_spec(120, Profile::Quick), &world, &Control::new());
    check_outcome(&o, &world);
    assert!(o.trace.nodes_elided > 0 && !o.trace.elided.is_empty());
    let recorded: u64 = o.trace.nodes.iter().map(|n| n.evaluations).sum();
    let elided: u64 = o.trace.elided.iter().map(|e| e.evaluations).sum();
    assert!(elided > 0, "some of the work is only in the elided buckets");
    assert_eq!(recorded + elided, o.used.evaluations);
    assert_eq!(world.calls.load(Ordering::SeqCst), o.used.evaluations);
    // Every drop is counted in exactly one bucket, and only tried kinds are ever dropped.
    let counted: u64 = o.trace.elided.iter().map(|e| e.count).sum();
    assert_eq!(counted, o.trace.nodes_elided);
    for e in &o.trace.elided {
        assert!(e.outcome.tried(), "{:?} was elided", e.outcome);
        assert!(e.count > 0 && e.bits_min <= e.bits_max);
    }
}

#[test]
fn residency_never_exceeds_the_bound_during_the_search() {
    // The node cap binding: peak residency is sampled after EVERY insert, so a transient
    // overshoot mid-search would show here even though the finished trace is back under.
    let world = World::new(FSK);
    let mut s = crowded_spec(120, Profile::Quick);
    s.trace_bounds.max_trace_nodes = 48;
    let o = run(&s, &world, &Control::new());
    check_outcome(&o, &world);
    assert!(!o.trace.over_bound, "the protected set fits 48 nodes");
    assert!(o.trace.nodes_elided > 0);
    assert!(o.trace.peak_nodes <= 48, "peak {} > 48", o.trace.peak_nodes);
    assert!(o.trace.peak_bytes <= u64::from(s.trace_bounds.max_trace_bytes));
    // The byte cap binding instead (ADR-0021 §2.3: the hard limit, first to bind at `deep`).
    let world = World::new(FSK);
    let mut s = crowded_spec(120, Profile::Deep);
    s.trace_bounds.max_trace_bytes = 24 * 1024;
    let o = run(&s, &world, &Control::new());
    check_outcome(&o, &world);
    assert!(!o.trace.over_bound);
    assert!(o.trace.nodes_elided > 0);
    assert!(
        o.trace.peak_bytes <= 24 * 1024,
        "peak {} bytes > 24 KiB",
        o.trace.peak_bytes
    );
    assert!(o.trace.bytes <= 24 * 1024);
    assert!(o.trace.peak_nodes < u64::from(Profile::Deep.trace_bounds().max_trace_nodes));
}

#[test]
fn a_grid_sweep_is_one_node_with_a_swept_descriptor() {
    // ADR-0021 §1 rule 2: the symbol-rate grid around the seed is evaluated point by point but
    // recorded as ONE node per S2 hypothesis, carrying a `swept` descriptor and the points'
    // evaluations — never one node per grid point.
    let world = World::new(FSK);
    let mut s = spec(standard_roots(), Profile::Standard);
    s.trace_bounds.max_trace_nodes = 100_000;
    s.trace_bounds.max_trace_bytes = u32::MAX;
    let o = run(&s, &world, &Control::new());
    check_outcome(&o, &world);
    assert_eq!(
        o.trace.nodes_elided, 0,
        "nothing elided: every node is visible"
    );
    let s2: Vec<_> = o
        .trace
        .nodes
        .iter()
        .filter(|n| n.stage == Stage::S2 && n.tried)
        .collect();
    assert!(!s2.is_empty());
    let mut per_parent: BTreeMap<&str, usize> = BTreeMap::new();
    for n in &s2 {
        *per_parent
            .entry(n.parent.as_deref().unwrap_or(""))
            .or_default() += 1;
        if n.outcome.kind() == OutcomeKind::Memoised {
            continue;
        }
        assert_eq!(n.hypothesis.swept.len(), 1, "{}: one swept axis", n.id);
        let sw = &n.hypothesis.swept[0];
        assert_eq!(sw.path, "nodes[clock].params.symbol_rate_bd");
        assert!(sw.points >= 7 && sw.lo < sw.hi, "{sw:?}");
        assert!(
            n.evaluations >= u64::from(sw.points),
            "{}: the node carries its points' evaluations ({} < {})",
            n.id,
            n.evaluations,
            sw.points
        );
    }
    // One S2 slot alternative and no discrete S2 parameter: one node per parent.
    for (parent, count) in per_parent {
        assert_eq!(
            count, 1,
            "{parent} has {count} S2 children: a node per grid point?"
        );
    }
}

#[test]
fn the_same_replay_key_gives_the_same_trace_byte_for_byte() {
    // ADR-0021 §5: count-bounded, so a re-run makes every decision again — including the
    // `deferred_budget` nodes a COUNT cap leaves, which are therefore not flagged.
    let run_once = || {
        let world = World::new(FSK);
        let mut s = crowded_spec(120, Profile::Quick);
        s.budget.max_evaluations = Some(150);
        s.unsupported.push(unsupported("css", "css_demod"));
        let o = run(&s, &world, &Control::new());
        check_outcome(&o, &world);
        o
    };
    let (a, b) = (run_once(), run_once());
    assert_eq!(a.replay_key, b.replay_key);
    assert_eq!(a.stop, Some(StopReason::Budget));
    assert!(
        !a.nondeterministic && !b.nondeterministic,
        "count-bounded: replayable"
    );
    let deferred: Vec<_> = a
        .trace
        .nodes
        .iter()
        .filter(|n| n.outcome.kind() == OutcomeKind::DeferredBudget)
        .collect();
    assert!(!deferred.is_empty(), "the count cap left work untried");
    assert!(deferred.iter().all(|n| !n.nondeterministic));
    assert!(a.trace.nodes_elided > 0, "retention decisions replay too");
    assert_eq!(deterministic_bytes(&a), deterministic_bytes(&b));
    assert_eq!(a.used.evaluations, b.used.evaluations);
    assert_eq!(a.results, b.results);
    // The key really keys: a different budget or seeding is a different key.
    let world = World::new(FSK);
    let mut s = crowded_spec(120, Profile::Quick);
    s.budget.max_evaluations = Some(151);
    s.unsupported.push(unsupported("css", "css_demod"));
    let c = run(&s, &world, &Control::new());
    assert_ne!(c.replay_key.budget, a.replay_key.budget);
    assert_eq!(c.replay_key.seed_ref, a.replay_key.seed_ref);
    let mut s2 = crowded_spec(121, Profile::Quick);
    s2.budget.max_evaluations = Some(150);
    let d = run(&s2, &World::new(FSK), &Control::new());
    assert_ne!(d.replay_key.seed_ref, a.replay_key.seed_ref);
    // It names what the decisions depend on: the blocks at their versions, the profile.
    let names: Vec<&str> = a
        .replay_key
        .blocks
        .iter()
        .map(|b| b.name.as_str())
        .collect();
    for want in [
        "fsk_demod",
        "am_demod",
        "clock_recovery",
        "sync_search",
        "crc",
        "nrzi",
    ] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }
    assert_eq!(a.replay_key.profile, Profile::Quick);
}

#[test]
fn a_time_caused_stop_flags_what_it_left_and_the_job_is_not_replayable() {
    // The wall backstop decides the stop: whatever it left untried is flagged, and the job says
    // it cannot be replayed.
    let world = World::new(FSK);
    let mut s = spec(standard_roots(), Profile::Standard);
    s.budget.wall_s = 1e-9;
    let o = run(&s, &world, &Control::new());
    check_outcome(&o, &world);
    assert_eq!(o.stop, Some(StopReason::Budget));
    assert!(o.nondeterministic);
    let deferred: Vec<_> = o
        .trace
        .nodes
        .iter()
        .filter(|n| n.outcome.kind() == OutcomeKind::DeferredBudget)
        .collect();
    assert!(!deferred.is_empty());
    assert!(deferred.iter().all(|n| n.nondeterministic));
    let v = serde_json::to_value(deferred[0]).unwrap();
    assert_eq!(v["nondeterministic"], true);
    // …and the flag is absent, not `false`, on every deterministic node.
    let settled = o
        .trace
        .nodes
        .iter()
        .find(|n| n.tried)
        .map(|n| serde_json::to_value(n).unwrap());
    if let Some(v) = settled {
        assert!(v.get("nondeterministic").is_none());
    }
}

#[test]
fn not_tried_nodes_survive_a_cap_that_drops_95_percent_of_the_tried() {
    let mut deferred = root(
        skeleton("generic-ook-framed", ook_s1(), false, &POLYS),
        -6.0,
        &POLYS,
    );
    deferred.deferred = Some(0.01);
    deferred.family = Some("ook".into());
    let world = World::new(FSK);
    let mut s = crowded_spec(250, Profile::Deep);
    let polys: Vec<String> = (0..250).map(|i| format!("0x{:04X}", 0x2000 + i)).collect();
    let polys: Vec<&str> = polys.iter().map(String::as_str).collect();
    for (i, prior) in [-0.7f32, -0.8, -0.9].into_iter().enumerate() {
        s.roots.push(root(
            skeleton(&format!("fsk-x{i}"), fsk_s1(), false, &polys),
            prior,
            &polys,
        ));
    }
    s.roots.push(deferred);
    for (st, b) in [
        ("css", "css_demod"),
        ("ofdm", "ofdm_demod"),
        ("psk", "psk_demod"),
    ] {
        s.unsupported.push(unsupported(st, b));
    }
    // The whole space is ~530 evaluations: a 420 cap stops the main pass part-way, so the
    // budget leaves live nodes untried and the side queue is never reached.
    s.budget.max_evaluations = Some(420);
    s.trace_bounds.max_trace_nodes = 40;
    let o = run(&s, &world, &Control::new());
    check_outcome(&o, &world);

    let tried_kept = o.trace.nodes.iter().filter(|n| n.tried).count() as u64;
    let tried_elided: u64 = o.trace.elided.iter().map(|e| e.count).sum();
    let tried_total = tried_kept + tried_elided;
    let dropped = tried_elided as f64 / tried_total as f64;
    assert!(
        dropped >= 0.95,
        "the cap drops {:.1} % of {tried_total} tried nodes",
        dropped * 100.0
    );
    let not_tried: Vec<_> = o.trace.nodes.iter().filter(|n| !n.tried).collect();
    // Every not-tried node the engine made is still here: none was elided …
    assert!(o.trace.elided.iter().all(|e| e.outcome.tried()));
    // … and they are the honesty rows: the budget's leftovers, the deferred family and one
    // row per unsupported structure.
    let kinds: std::collections::BTreeSet<OutcomeKind> =
        not_tried.iter().map(|n| n.outcome.kind()).collect();
    assert!(kinds.contains(&OutcomeKind::DeferredBudget), "{kinds:?}");
    assert!(kinds.contains(&OutcomeKind::DeferredPrior), "{kinds:?}");
    let structures: std::collections::BTreeSet<&str> = not_tried
        .iter()
        .filter_map(|n| match &n.outcome {
            Outcome::Unsupported { structure, .. } => Some(structure.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        structures,
        ["css", "ofdm", "psk"].into_iter().collect(),
        "one node per distinct unsupported structure"
    );
    let not_tried_made = {
        // The live progress counter saw every not-tried decision the engine made.
        let world = World::new(FSK);
        let mut last = Progress::default();
        let mut obs = |p: &Progress| last = *p;
        let _ = search(&s, &Registry::builtin(), &world, &Control::new(), &mut obs);
        last.not_tried
    };
    assert_eq!(not_tried.len() as u64, not_tried_made);
}

// ---------------------------------------------------------------------------------------------
// The trace's cost, measured (ADR-0021 §3; T-453's constraint: measured, not assumed)
// ---------------------------------------------------------------------------------------------

/// Counts every allocation on the current thread, and separately those made while the engine
/// is building or retaining a trace node (`hk_synth::trace_sink::in_trace_scope`). Thread-local,
/// so tests running in parallel in this binary do not pollute each other; the measured search
/// runs at `threads = 1`, where evaluation happens on the calling thread too.
struct Counting;

thread_local! {
    static ALLOC_TOTAL: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static ALLOC_TRACE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn count_alloc(bytes: usize) {
    let b = bytes as u64;
    let _ = ALLOC_TOTAL.try_with(|c| c.set(c.get() + b));
    if hk_synth::trace_sink::in_trace_scope() {
        let _ = ALLOC_TRACE.try_with(|c| c.set(c.get() + b));
    }
}

// SAFETY: every call is forwarded unchanged to the system allocator; counting touches only
// const-initialised, destructor-free thread-locals, which never allocate.
unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        count_alloc(layout.size());
        unsafe { std::alloc::System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        count_alloc(layout.size());
        unsafe { std::alloc::System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        count_alloc(new_size.saturating_sub(layout.size()));
        unsafe { std::alloc::System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// One measured search: (outcome, total bytes allocated, bytes allocated for the trace).
fn measured(s: &SearchSpec) -> (SearchOutcome, u64, u64) {
    let world = World::new(FSK);
    let control = Control::new();
    let registry = Registry::builtin();
    ALLOC_TOTAL.with(|c| c.set(0));
    ALLOC_TRACE.with(|c| c.set(0));
    let o = search(s, &registry, &world, &control, &mut ());
    let total = ALLOC_TOTAL.with(std::cell::Cell::get);
    let trace = ALLOC_TRACE.with(std::cell::Cell::get);
    check_outcome(&o, &world);
    (o, total, trace)
}

#[test]
fn the_trace_cost_is_measured_not_assumed() {
    // Two shapes: quick's 128-node bound under a crowded space (retention busy on almost every
    // insert), and deep's 2 048-node / 256 KiB bound (the largest retained set to scan).
    let mut rows = Vec::new();
    for (label, profile) in [("quick", Profile::Quick), ("deep", Profile::Deep)] {
        let mut s = crowded_spec(250, profile);
        s.budget.threads = 1;
        let (o, total, trace) = measured(&s);
        assert!(trace > 0 && trace <= total, "{trace} of {total}");
        let c = o.trace_cost;
        assert!(c.retention_s <= c.wall_s + 1e-9);
        assert_eq!(
            c.decisions,
            o.trace.nodes.len() as u64 + o.trace.nodes_elided
        );
        // The synthetic evaluator is nearly free, so `fraction` here is an upper bound on what a
        // real search pays. Projected against docs/27 §3's measured ≤ 5 ms per cached-stage
        // evaluation, the same trace work is:
        let projected = c.wall_s / (c.wall_s + o.used.evaluations as f64 * 5e-3);
        rows.push(format!(
            "{label}: {} decisions ({} kept, {} elided), {} evaluations; trace wall {:.1} ms \
             ({:.1} us/decision, retention {:.1} ms) = {:.1} % of a free-evaluator search, \
             {:.2} % projected at 5 ms/evaluation; trace allocations {} of {} bytes = {:.1} %; \
             retained {} bytes (peak {} nodes / {} bytes)",
            c.decisions,
            o.trace.nodes.len(),
            o.trace.nodes_elided,
            o.used.evaluations,
            c.wall_s * 1e3,
            c.wall_s * 1e6 / c.decisions.max(1) as f64,
            c.retention_s * 1e3,
            c.fraction * 100.0,
            projected * 100.0,
            trace,
            total,
            trace as f64 * 100.0 / total as f64,
            c.bytes,
            o.trace.peak_nodes,
            o.trace.peak_bytes,
        ));
    }
    for r in &rows {
        eprintln!("T-565 trace cost: {r}");
    }
}

// ---------------------------------------------------------------------------------------------
// MAUTO M-9 (T-860): ADR-0022's solve rule on hold-out, and ADR-0021 §8.2's null control
// ---------------------------------------------------------------------------------------------

/// The winning open-search result solves only through ADR-0022's inequality on hold-out — the
/// analytic-null currency, per-stage look-elsewhere — and only after the shuffled-null control
/// ran and passed. Everything the confirm gate reads is on the result.
#[test]
fn m9_an_open_search_solves_on_analytic_hold_out_bits_after_the_null_control_passes() {
    let world = World::new(FSK);
    let o = run(
        &spec(standard_roots(), Profile::Standard),
        &world,
        &Control::new(),
    );
    check_outcome(&o, &world);
    let top = &o.results[0];
    assert_eq!(top.verdict, Verdict::Solved);
    let h = top
        .holdout
        .as_ref()
        .expect("a solved result carries its hold-out evidence");
    // The confirm key is the analytic part only: S4's sync excess and S5's check, each net of its
    // own stage's L — never the calibrated S0–S3 bits.
    let analytic_on_ladder: f32 = h
        .stages
        .iter()
        .filter(|e| e.metric.pays_for_confirm())
        .map(|e| e.bits)
        .sum();
    assert!(
        h.analytic_bits < analytic_on_ladder,
        "look-elsewhere is charged per stage: {} vs {analytic_on_ladder}",
        h.analytic_bits
    );
    assert!(h.analytic_bits < h.evidence_bits + 1e3);
    assert_eq!(top.analytic_holdout_bits, Some(h.analytic_bits));
    assert!(h.analytic_bits >= 24.0);
    assert_eq!(h.check_width, Some(16));
    assert_eq!(h.differences, 12);
    assert!(h.check_bits.unwrap() >= 16.0);
    let l_check = h
        .l_check
        .expect("an open search's L_check is the S5 stage's own");
    assert!(l_check > 0.0, "three polynomials were tried at S5");
    assert!((h.check_bits.unwrap() - (16.0 * 12.0 - l_check)).abs() < 1e-3);
    assert_eq!(h.check_origin, CheckOrigin::Searched);
    // ADR-0021 §8.2: K = 2 at standard, the unchanged prefix over each null, recorded.
    let nc = h
        .null_control
        .expect("a searched check runs the null control");
    assert!(nc.ran && !nc.capped && nc.passed());
    assert_eq!(nc.k, 2);
    assert!(nc.margin_bits >= 8.0);
    assert_eq!(
        world.null_calls.load(Ordering::SeqCst),
        2 * top
            .stages
            .iter()
            .map(|e| e.stage)
            .collect::<std::collections::BTreeSet<_>>()
            .len() as u64,
        "K nulls × the chain's stages, no more"
    );
    // The decodes the attach step stores are the solved rank-1's hold-out frames.
    assert_eq!(o.holdout_frames.len(), 12);
    assert!(
        o.holdout_frames
            .iter()
            .all(|f| f.check_valid && !f.corrected)
    );
    assert!(o.null_control().is_some_and(|n| n.passed()));
    assert_eq!(
        o.seal(None, None, "t"),
        None,
        "a solved search seals no negative result"
    );
}

/// A search that finds its structure in the null windows too cannot tell the fit from chance:
/// ADR-0021 §8.2 caps the verdict at `framed`, the resolution says `tied`, and the record says why.
#[test]
fn m9_the_null_control_caps_a_fit_that_the_nulls_reproduce() {
    let mut world = World::new(FSK);
    world.null_fits = true;
    let o = run(
        &spec(standard_roots(), Profile::Standard),
        &world,
        &Control::new(),
    );
    check_outcome(&o, &world);
    assert!(
        o.results.iter().all(|r| r.verdict != Verdict::Solved),
        "the control can only cap, and here it must"
    );
    assert_ne!(o.stop, Some(StopReason::Solved));
    assert_eq!(o.reason, Some(Reason::Tied));
    let capped = o
        .results
        .iter()
        .find(|r| r.holdout.as_ref().and_then(|h| h.null_control).is_some())
        .expect("the capped candidate keeps its null-control record");
    assert!(capped.verdict <= Verdict::Framed);
    let nc = capped.holdout.as_ref().unwrap().null_control.unwrap();
    assert!(nc.ran && nc.capped && !nc.passed());
    assert!(nc.margin_bits < 8.0);
    assert!(
        o.holdout_frames.is_empty(),
        "nothing solved, nothing to store"
    );
    let res = o
        .seal(None, None, "t")
        .expect("an unsolved search seals a resolution");
    assert_eq!(res.kind, hk_synth::ResolutionKind::Unknown);
    assert_eq!(res.null_control, Some(nc));
    assert!(res.summary.contains("null windows"), "{}", res.summary);
}

/// A template that fixes the whole check has `L_check = 0` beyond its own S5 count and needs no
/// null control; at `quick` (K = 0) no control runs either. Neither is charged for one.
#[test]
fn m9_a_template_fixed_check_and_a_quick_job_run_no_null_control() {
    let world = World::new(FSK);
    let mut fixed = root(
        skeleton("generic-fsk-framed", fsk_s1(), false, &["0x8005"]),
        0.0,
        &["0x8005"],
    );
    fixed.check_origin = CheckOrigin::TemplateFixed;
    let o = run(
        &spec(vec![fixed], Profile::Standard),
        &world,
        &Control::new(),
    );
    check_outcome(&o, &world);
    let top = &o.results[0];
    assert_eq!(top.verdict, Verdict::Solved);
    let h = top.holdout.as_ref().unwrap();
    assert_eq!(
        h.l_check,
        Some(0.0),
        "one polynomial: zero look-elsewhere at S5"
    );
    assert_eq!(h.null_control, None);
    assert_eq!(world.null_calls.load(Ordering::SeqCst), 0);

    let world = World::new(FSK);
    let o = run(
        &spec(standard_roots(), Profile::Quick),
        &world,
        &Control::new(),
    );
    assert_eq!(world.null_calls.load(Ordering::SeqCst), 0, "quick: K = 0");
    assert!(
        o.results
            .iter()
            .all(|r| { r.holdout.as_ref().is_none_or(|h| h.null_control.is_none()) })
    );
}

/// ADR-0022 §4: the three constants that replaced 64 bits / 3 frames / width 16. A check under the
/// 8-bit width floor never solves however many frames it passes; a single hold-out frame of a
/// searched CRC-16 cannot carry 16 bits after its L; and a discovered template whose discovery
/// cost was never recorded never solves — an unknown charge is not a zero one.
#[test]
fn m9_width_floor_hard_check_floor_and_the_laundering_rule() {
    let mut narrow = World::new(FSK);
    narrow.width = 4;
    narrow.holdout_frames = 40;
    let o = run(
        &spec(standard_roots(), Profile::Standard),
        &narrow,
        &Control::new(),
    );
    assert!(o.results.iter().all(|r| r.verdict != Verdict::Solved));
    let h = o.results[0].holdout.as_ref().expect("validated");
    assert_eq!(h.check_width, Some(4));

    let mut one = World::new(FSK);
    one.holdout_frames = 1;
    let o = run(
        &spec(standard_roots(), Profile::Standard),
        &one,
        &Control::new(),
    );
    assert!(o.results.iter().all(|r| r.verdict != Verdict::Solved));
    let h = o.results[0].holdout.as_ref().expect("validated");
    assert_eq!(h.differences, 1);
    assert!(h.check_bits.unwrap() < 16.0, "{:?}", h.check_bits);

    let world = World::new(FSK);
    let mut laundered = root(
        skeleton("generic-fsk-framed", fsk_s1(), false, &["0x8005"]),
        0.0,
        &["0x8005"],
    );
    laundered.check_origin = CheckOrigin::Discovered {
        look_elsewhere_bits: None,
    };
    let o = run(
        &spec(vec![laundered.clone()], Profile::Standard),
        &world,
        &Control::new(),
    );
    assert!(o.results.iter().all(|r| r.verdict != Verdict::Solved));
    assert_eq!(o.results[0].holdout.as_ref().unwrap().l_check, None);
    // With the discovering search's cost recorded, it is inherited as L_check and paid for.
    laundered.check_origin = CheckOrigin::Discovered {
        look_elsewhere_bits: Some(20.0),
    };
    let world = World::new(FSK);
    let o = run(
        &spec(vec![laundered], Profile::Standard),
        &world,
        &Control::new(),
    );
    let h = o.results[0].holdout.as_ref().unwrap();
    assert_eq!(h.l_check, Some(20.0));
    assert!((h.check_bits.unwrap() - (16.0 * 12.0 - 20.0)).abs() < 1e-3);
    assert!(
        h.null_control.is_some(),
        "a discovered check counts as searched"
    );
    assert_eq!(o.results[0].verdict, Verdict::Solved);
}

// ---------------------------------------------------------------------------------------------
// T-575: ADR-0022 §4.2's `differences`, and the job-total look-elsewhere never reaching the gate
// ---------------------------------------------------------------------------------------------

/// A beacon sending one payload twelve times is one fact, not twelve (ADR-0022 §4.2). The engine
/// reads `differences` from the check block's chance-corrected `raw`, not its tested support `n`,
/// and clamps the check to `width × differences − L_check` however many bits the block claims —
/// so the repeat count solves nothing. Before T-575 this solved, with `differences` = 12.
#[test]
fn t575_a_repeated_payload_beacon_does_not_solve_on_its_repeat_count() {
    let mut beacon = World::new(FSK);
    beacon.holdout_payloads = Some(1);
    let o = run(
        &spec(standard_roots(), Profile::Standard),
        &beacon,
        &Control::new(),
    );
    assert!(
        o.results.iter().all(|r| r.verdict != Verdict::Solved),
        "{:?}",
        o.results.iter().map(|r| r.verdict).collect::<Vec<_>>()
    );
    let h = o
        .results
        .iter()
        .find_map(|r| r.holdout.as_ref().filter(|h| h.check_width.is_some()))
        .expect("validated on hold-out");
    assert_eq!(h.differences, 1, "one payload, however often it repeats");
    let l_check = h.l_check.unwrap();
    assert!(
        (h.check_bits.unwrap() - (16.0 - l_check)).abs() < 1e-3,
        "clamped to width × differences − L_check: {:?}, L {l_check}",
        h.check_bits
    );

    // The same frames with distinct payloads solve, so it is the repetition that refused.
    let world = World::new(FSK);
    let o = run(
        &spec(standard_roots(), Profile::Standard),
        &world,
        &Control::new(),
    );
    assert_eq!(o.results[0].verdict, Verdict::Solved);
    assert_eq!(o.results[0].holdout.as_ref().unwrap().differences, 12);
}

/// ADR-0022 §2.3: the job-total `coverage.look_elsewhere_bits` is a reporting field, never charged
/// against the hypothesis being confirmed. One CRC-24 frame plus its sync: the paying ladder less
/// the job total is under 24 bits, yet the result solves with ≥ 24 analytic bits — each stage
/// charged its own `L_j` only. Had the job total been subtracted, it could not have.
#[test]
fn t575_the_job_total_look_elsewhere_never_reaches_the_solve_or_confirm_key() {
    let mut world = World::new(FSK);
    world.width = 24;
    world.holdout_frames = 1;
    let o = run(
        &spec(standard_roots(), Profile::Standard),
        &world,
        &Control::new(),
    );
    let top = &o.results[0];
    assert_eq!(top.verdict, Verdict::Solved);
    let h = top.holdout.as_ref().unwrap();
    let paying: f32 = h
        .stages
        .iter()
        .filter(|e| e.metric.pays_for_confirm())
        .map(|e| e.bits)
        .sum();
    let job_total = o.coverage.look_elsewhere_bits;
    assert!(h.analytic_bits >= 24.0);
    assert!(
        paying - job_total < 24.0,
        "the fixture must make the job total decisive: {paying} − {job_total}"
    );
    assert!(h.analytic_bits > paying - job_total);
}
