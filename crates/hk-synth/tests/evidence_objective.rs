//! `EvidenceObjective` over T-070's `RefinementLoop` (ADR-0015 §2.3; T-858 = MAUTO M-7), and the
//! recipe-schema-3 objective `{"evidence": "deepest"}` a running synthesized pipeline refines with.
//!
//! Every case builds a capture window holding a **hidden** 2-FSK emission (centre, deviation and
//! symbol rate known only to the test), hands the loop a coarse start box, and asserts on what
//! the loop reports — `RefinementOutcome` (locked, validated, quality, tuning, trace) and each
//! `Measurement` — never on objective internals. The truth is used only to check the answer:
//! the refined channel must hold both tones, and nothing may lock on noise.
//!
//! Use case: RESEARCH-002 (a never-seen FSK sensor decoded blind): the synthesis search's local
//! refinement step (§3.1 step 6), and SIGNAL-062's "tune from the processed output" loop
//! (T-070/C19) driven by evidence rather than a mode-specific objective.

use std::collections::BTreeMap;
use std::sync::Arc;

use hk_blocks::Registry;
use hk_core::ProvenanceHandle;
use hk_demod::refine::{EvalDepth, IqWindow, Objective, RefineStart, RefinementLoop, Tuning};
use hk_dsp::InputInfo;
use hk_model::{SampleTime, Timestamp};
use hk_recipe::Recipe;
use hk_synth::candidate::{Candidate, Domain, FloatDomain, FreeParam, Scale};
use hk_synth::{
    CalibrationSet, EVIDENCE_OBJECTIVE, EvidenceContext, EvidenceObjective, Fill, ObjectiveError,
    Stage, WindowSplit,
};
use num_complex::Complex32;
use serde_json::{Value, json};

/// Capture rate.
const FS: f64 = 240_000.0;
/// The analysis window: 0.6 s.
const TOTAL: usize = 144_000;
/// Hidden truth: the emission's centre (baseband, the capture is tuned to 0 Hz), deviation and
/// symbol rate.
const TRUE_CENTER_HZ: f64 = 30_000.0;
const TRUE_DEV_HZ: f64 = 2_400.0;
const TRUE_BAUD: f64 = 4_800.0;
/// The prefix's own channel filter (`chan`): passband edge, and transition width (its support
/// ends at `CHAN_CUTOFF_HZ + CHAN_TRANSITION_HZ`).
const CHAN_CUTOFF_HZ: f64 = 6_000.0;
const CHAN_TRANSITION_HZ: f64 = 3_000.0;
/// Per-component noise σ of the capture: the emission (amplitude 0.5) sits ~5 dB above the noise
/// over the whole 240 kHz, so its unshaped-CPFSK sidelobes fall below the noise within a few kHz
/// and "which tuning holds the emission" has an answer.
const NOISE: f64 = 0.3;
/// The recorded fill of the synthetic capture: nominal (σ 8 LSB, no clipping), §13.3.
const NOMINAL: Fill = Fill::new(8.0, 0.0);

/// A reproducible generator (xorshift64*).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn uniform(&mut self) -> f64 {
        ((self.next() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }
    fn gauss(&mut self) -> f64 {
        (-2.0 * self.uniform().ln()).sqrt() * (std::f64::consts::TAU * self.uniform()).cos()
    }
}

/// `len` capture samples: complex noise of σ `noise`, plus the hidden 2-FSK emission over
/// `[0, signal_until)`.
fn capture(len: usize, signal_until: usize, noise: f64, seed: u64) -> Vec<Complex32> {
    let mut rng = Rng(seed);
    let sps = FS / TRUE_BAUD;
    let mut ph = 0.0f64;
    let mut sym = 1.0;
    let mut next_symbol = 0.0f64;
    (0..len)
        .map(|i| {
            if i as f64 >= next_symbol {
                sym = if rng.next() >> 63 == 1 { 1.0 } else { -1.0 };
                next_symbol += sps;
            }
            let f = TRUE_CENTER_HZ + sym * TRUE_DEV_HZ;
            ph = (ph + std::f64::consts::TAU * f / FS) % std::f64::consts::TAU;
            let s = if i < signal_until {
                Complex32::new(0.5 * ph.cos() as f32, 0.5 * ph.sin() as f32)
            } else {
                Complex32::new(0.0, 0.0)
            };
            s + Complex32::new((noise * rng.gauss()) as f32, (noise * rng.gauss()) as f32)
        })
        .collect()
}

fn provenance() -> ProvenanceHandle {
    let p: hk_model::Provenance = serde_json::from_value(json!({
        "device_id": "synthetic:t858", "tune": { "center_hz": 0.0, "sample_rate_hz": FS,
        "lna_db": 0.0, "vga_db": 0.0, "amp_on": false, "bandwidth_hz": FS },
        "overload": false, "quantisation_limited": false, "clock_source": "internal",
        "clock_locked": true, "timestamp_method": "synthetic", "timestamp_error_budget_ns": 0,
    }))
    .unwrap();
    ProvenanceHandle::new(p)
}

fn window<'a>(p: &'a ProvenanceHandle, x: &'a [Complex32]) -> IqWindow<'a, Complex32> {
    IqWindow::new(
        InputInfo {
            time: SampleTime {
                sample_index: 0,
                host_time: Timestamp::from_unix_nanos(0),
            },
            discontinuity: hk_core::Discontinuity::NONE,
            dropped_before: 0,
            provenance: p,
        },
        x,
    )
}

/// The S0→S2 FSK prefix the search would hold at a beam node: channel filter, FSK
/// discriminator, clock recovery (seeded 1 % off the true rate).
fn prefix_nodes() -> Value {
    json!([
        { "id": "chan", "block": "lowpass",
          "params": { "cutoff_hz": CHAN_CUTOFF_HZ, "transition_hz": CHAN_TRANSITION_HZ } },
        { "id": "fsk", "block": "fsk_demod" },
        { "id": "clock", "block": "clock_recovery",
          "params": { "symbol_rate_bd": 4752.0, "pulse": "nrz", "algorithm": "gardner" } }
    ])
}

fn prefix(schema_version: u32, refine: Option<Value>) -> Recipe {
    let mut doc = json!({
        "schema": "hackriff.recipe", "schema_version": schema_version,
        "id": "synth-candidate", "version": 1, "name": "fsk prefix",
        "input": { "port": "iq", "sample_rate_hz": 48000, "bandwidth_hz": 12000 },
        "nodes": prefix_nodes(),
        "outputs": [ { "id": "tail", "kind": "stage", "from": "clock" } ],
        "output_policy": { "content_class": "metadata-only" }
    });
    if let Some(r) = refine {
        doc["refine"] = r;
    }
    serde_json::from_value(doc).expect("prefix parses")
}

fn candidate() -> Candidate {
    Candidate {
        skeleton: "generic-fsk@1".into(),
        choices: BTreeMap::from([
            (Stage::S0, "lowpass".to_owned()),
            (Stage::S1, "fsk".to_owned()),
            (Stage::S2, "gardner".to_owned()),
        ]),
        recipe: prefix(hk_recipe::RECIPE_SCHEMA_VERSION, None),
        free: Vec::new(),
    }
}

/// The root's declared parameters: the symbol rate (bound in this prefix) and a later stage's
/// sync word (not bound yet: never refined).
fn root_free() -> Vec<FreeParam> {
    vec![
        FreeParam {
            path: "nodes[clock].params.symbol_rate_bd".into(),
            domain: Domain::Float(FloatDomain {
                lo: 1_200.0,
                hi: 19_200.0,
                scale: Scale::Log,
                resolution: Some(0.001),
            }),
            seed: Some(json!(4752.0)),
            source: None,
        },
        serde_json::from_value(json!({
            "path": "nodes[sync].params.sync_word", "domain": { "proposal": "assist.sync" }
        }))
        .unwrap(),
    ]
}

fn ctx(fill: Fill) -> EvidenceContext {
    EvidenceContext {
        registry: Arc::new(Registry::builtin()),
        calibration: Arc::new(CalibrationSet::builtin().expect("shipped tables load")),
        fill,
        split: WindowSplit::of_window(TOTAL),
    }
}

/// The start box a coarse detection would give: wide enough to hold the emission, centred
/// 14 kHz off it — so far that the prefix's channel filter (stopband from 9 kHz) holds neither
/// tone at the start.
fn start() -> RefineStart {
    RefineStart {
        center_hz: TRUE_CENTER_HZ + 14_000.0,
        bandwidth_hz: 28_000.0,
        warm: false,
    }
}

/// How many hidden tones sit inside the prefix's channel filter's support (passband and
/// transition) at `center_hz`, for a filter `bandwidth_hz` wide (two-sided).
fn tones_held(center_hz: f64, bandwidth_hz: f64) -> usize {
    [TRUE_CENTER_HZ - TRUE_DEV_HZ, TRUE_CENTER_HZ + TRUE_DEV_HZ]
        .iter()
        .filter(|f| (*f - center_hz).abs() < 0.5 * bandwidth_hz + CHAN_TRANSITION_HZ)
        .count()
}

fn refine(
    obj: EvidenceObjective,
    x: &[Complex32],
    start: &RefineStart,
) -> hk_demod::refine::RefinementOutcome {
    let p = provenance();
    let mut cfg = obj.loop_config(x.len(), FS, 96);
    // The measurement count is the budget; the wall-clock backstop must not decide a test
    // outcome on a loaded box (a debug-build refinement runs tens of seconds under load).
    cfg.termination.time_budget = std::time::Duration::from_secs(24 * 3600);
    RefinementLoop::new(obj, cfg).run(window(&p, x), start)
}

#[test]
fn refinement_from_a_coarse_box_locks_on_hold_out_evidence_and_holds_the_emission() {
    let x = capture(TOTAL, TOTAL, NOISE, 0x7858);
    let mut obj = EvidenceObjective::for_candidate(&candidate(), &root_free(), ctx(NOMINAL))
        .expect("an iq prefix");
    assert_eq!(obj.target(), Stage::S2);
    assert_eq!(obj.lock_stage(), Stage::S2);
    // Only the bound parameter is an axis; the unbound sync word is not refined.
    let axes: Vec<&str> = obj.axes().iter().map(|a| a.path.as_str()).collect();
    assert_eq!(axes, ["nodes[clock].params.symbol_rate_bd"]);
    // The bandwidth axis (§2.3) is the S0 channel filter's width, searched around its own
    // 12 kHz and never so wide that the filter's support leaves the flat channeliser.
    let space = obj.space(&start(), FS);
    assert_eq!(space.nominal_bandwidth_hz, 2.0 * CHAN_CUTOFF_HZ);
    assert!(space.bandwidth_hz.0 < space.bandwidth_hz.1, "{space:?}");
    assert!(
        space.bandwidth_hz.1 + 2.0 * CHAN_TRANSITION_HZ <= obj.channel_width(FS) + 1e-6,
        "{space:?}"
    );

    // At the start box the prefix loses a tone: measured, it does not lock.
    let p = provenance();
    let st = start();
    let at_start = obj
        .evaluate(
            window(&p, &x),
            &Tuning {
                center_hz: st.center_hz,
                bandwidth_hz: 2.0 * CHAN_CUTOFF_HZ,
                mode: BTreeMap::new(),
            },
            EvalDepth::Track,
        )
        .unwrap();
    assert_eq!(tones_held(st.center_hz, 2.0 * CHAN_CUTOFF_HZ), 0);
    assert!(!at_start.locked, "{at_start:?}");

    let o = refine(obj, &x, &st);
    assert_eq!(o.objective, EVIDENCE_OBJECTIVE);
    assert_eq!(o.mode, "generic-fsk@1");
    assert!(o.validated && o.locked, "{o:#?}");
    // The refined channel holds the emission. (Not necessarily centred on it: S1–S2 evidence
    // saturates at the calibrated claim cap as soon as one tone passes the filter, so the loop
    // keeps the first saturating centre nearest the start; ranking centres more finely than
    // that is the analytic stages' job, S4/S5, whose bits grow with the evidence.)
    assert!(
        tones_held(o.tuning.center_hz, o.tuning.bandwidth_hz) > 0,
        "refined centre {} Hz holds neither tone",
        o.tuning.center_hz
    );
    // The loop's quality is evidence bits, and the validated result clears the S2 floor (6).
    assert!(
        o.quality > at_start.quality,
        "{} vs {}",
        o.quality,
        at_start.quality
    );
    assert!(o.mode_params["b_S2"] >= 6.0, "{:?}", o.mode_params);
    assert_eq!(o.labels["lock_stage"], "S2");
    assert_eq!(o.labels["window"], "holdout");
    // The symbol-rate axis was searched and its answer bound on the result.
    assert!(
        o.tuning
            .mode
            .contains_key("nodes[clock].params.symbol_rate_bd"),
        "{:?}",
        o.tuning.mode
    );
}

#[test]
fn only_hold_out_evidence_vouches_for_the_result() {
    // The emission stops at the split: the search window holds it, the hold-out does not.
    let split = WindowSplit::of_window(TOTAL).holdout_from;
    let x = capture(TOTAL, split, NOISE, 0x7859);
    let obj = EvidenceObjective::for_candidate(&candidate(), &root_free(), ctx(NOMINAL)).unwrap();
    let o = refine(obj, &x, &start());
    assert!(
        o.trace.iter().any(|t| t.locked),
        "the search window's measurements lock"
    );
    assert!(o.validated, "{o:#?}");
    assert!(
        !o.locked,
        "a search-window lock must never vouch for the result: {o:#?}"
    );
}

#[test]
fn noise_never_locks() {
    let x = capture(TOTAL, 0, NOISE, 0x785a);
    let obj = EvidenceObjective::for_candidate(&candidate(), &root_free(), ctx(NOMINAL)).unwrap();
    let o = refine(obj, &x, &start());
    // The search window may lock on noise by chance (the S2 floor is a pruning floor: two
    // groups of up to 6 bits each clear 6 on noise a few per cent of the time, and the loop
    // looks at dozens of tunings); the hold-out is independent, and it alone vouches.
    assert!(!o.locked, "{o:#?}");
}

#[test]
fn an_unknown_fill_credits_no_calibrated_evidence() {
    // §13.3: unknown σ is under-filled, and a calibrated metric then scores 0 bits.
    let x = capture(TOTAL, TOTAL, NOISE, 0x785b);
    let mut obj =
        EvidenceObjective::for_candidate(&candidate(), &root_free(), ctx(Fill::default())).unwrap();
    let p = provenance();
    let m = obj
        .evaluate(
            window(&p, &x),
            &Tuning {
                center_hz: TRUE_CENTER_HZ,
                bandwidth_hz: 0.0,
                mode: BTreeMap::new(),
            },
            EvalDepth::Track,
        )
        .unwrap();
    assert!(!m.locked);
    assert_eq!(m.quality, 0.0, "{m:?}");
}

#[test]
fn every_calibrated_stage_scores_on_a_window_of_arbitrary_length() {
    // A slice of any length lands each calibrated block on its own table support; a slice
    // shorter than the smallest support scores nothing rather than borrowing a table.
    let x = capture(TOTAL, TOTAL, NOISE, 0x785c);
    let mut obj =
        EvidenceObjective::for_candidate(&candidate(), &root_free(), ctx(NOMINAL)).unwrap();
    let p = provenance();
    let at_truth = Tuning {
        center_hz: TRUE_CENTER_HZ,
        bandwidth_hz: 0.0,
        mode: BTreeMap::new(),
    };
    // 90 001 capture samples (18 000 at the prefix rate, ~1 800 symbols): no block lands on a
    // calibrated support by itself.
    let m = obj
        .evaluate(window(&p, &x[..90_001]), &at_truth, EvalDepth::Track)
        .unwrap();
    for s in ["b_S1", "b_S2"] {
        assert!(m.mode_params[s] > 0.0, "{s}: {m:?}");
    }
    assert!(m.mode_params["support_runs"] > 1.0, "{m:?}");
    // ADR-0015 §2.3 / §1.3 as accepted: quality is evidence_bits over S0 … the deepest stage.
    // (each stage here is below its cap, so capped = raw b_j).
    let summed: f64 = ["b_S0", "b_S1", "b_S2"]
        .iter()
        .map(|k| m.mode_params.get(*k).copied().unwrap_or(0.0))
        .sum();
    assert_eq!(m.quality, summed, "S0 is counted: {m:?}");
    assert!(m.locked, "{m:?}");

    // 0.01 s: under every table's smallest support.
    let m = obj
        .evaluate(window(&p, &x[..2_400]), &at_truth, EvalDepth::Track)
        .unwrap();
    assert_eq!(m.quality, 0.0, "{m:?}");
    assert!(!m.locked);
}

#[test]
fn a_schema_3_recipe_refines_what_its_tune_list_names() {
    let r = prefix(
        3,
        Some(json!({
            "objective": { "evidence": "deepest" },
            "tune": ["center_hz", "nodes[clock].params.symbol_rate_bd"]
        })),
    );
    r.validate(&Registry::builtin())
        .expect("a valid schema-3 recipe");
    let obj = EvidenceObjective::from_recipe(&r, Stage::S2, ctx(NOMINAL)).unwrap();
    let st = start();
    let space = obj.space(&st, FS);
    // `bandwidth_hz` is not in `tune`: the bandwidth axis is the prefix's own S0 channel filter
    // (2 × cutoff), fixed.
    let w = 2.0 * CHAN_CUTOFF_HZ;
    assert_eq!(space.bandwidth_hz, (w, w));
    assert!(space.center_hz.0 < space.center_hz.1);
    // The parameter axis tracks ± 3 % around the running value, current value first.
    assert_eq!(space.mode_axes.len(), 1);
    let axis = &space.mode_axes[0];
    assert_eq!(axis.name, "nodes[clock].params.symbol_rate_bd");
    assert_eq!(axis.values[0], 4752.0);
    assert!(
        axis.values
            .iter()
            .all(|v| (v / 4752.0 - 1.0).abs() <= 0.0301)
    );

    let x = capture(TOTAL, TOTAL, NOISE, 0x785d);
    let o = refine(obj, &x, &st);
    assert!(o.validated && o.locked, "{o:#?}");
    assert!(
        tones_held(o.tuning.center_hz, o.tuning.bandwidth_hz) > 0,
        "{}",
        o.tuning.center_hz
    );
    assert_eq!(o.mode, "synth-candidate");
}

#[test]
fn only_an_evidence_objective_on_an_iq_prefix_builds() {
    // A node-metric objective is not this objective's to run.
    let r = prefix(
        3,
        Some(json!({
            "objective": { "node": "clock", "metric": "quality", "goal": "max" },
            "tune": ["center_hz"]
        })),
    );
    assert_eq!(
        EvidenceObjective::from_recipe(&r, Stage::S2, ctx(NOMINAL)).err(),
        Some(ObjectiveError::NotEvidence)
    );
    assert_eq!(
        EvidenceObjective::from_recipe(&prefix(3, None), Stage::S2, ctx(NOMINAL)).err(),
        Some(ObjectiveError::NotEvidence)
    );
    // A prefix that does not read IQ at a declared rate cannot be channelised.
    let mut c = candidate();
    c.recipe.input.sample_rate_hz = None;
    assert_eq!(
        EvidenceObjective::for_candidate(&c, &[], ctx(NOMINAL)).err(),
        Some(ObjectiveError::NotIq)
    );
}

#[test]
fn s6_has_no_floor_so_a_full_chain_locks_on_s5() {
    let mut c = candidate();
    c.choices.insert(Stage::S6, "fields".into());
    let obj = EvidenceObjective::for_candidate(&c, &[], ctx(NOMINAL)).unwrap();
    assert_eq!(obj.target(), Stage::S6);
    assert_eq!(obj.lock_stage(), Stage::S5);
}

#[test]
fn an_s0_only_prefix_is_accepted_and_locks_on_s0() {
    // §2.3 as accepted refuses no prefix (the S0 exception is only proposed, pending the user).
    let mut c = candidate();
    c.choices = BTreeMap::from([(Stage::S0, "lowpass".to_owned())]);
    let obj = EvidenceObjective::for_candidate(&c, &[], ctx(NOMINAL)).unwrap();
    assert_eq!(obj.target(), Stage::S0);
    assert_eq!(obj.lock_stage(), Stage::S0);
}
