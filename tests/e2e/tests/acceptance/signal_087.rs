//! `SIGNAL-087` — blind auto-discovery and auto-decode of a trunked control channel (T-545).
//!
//! Phase 2 of the BART target. Read the suite header in `acceptance_mauto.rs` first: it states
//! why the fixture is synthetic, what a real capture would change, and the T-299 circularity
//! caveat. This file holds the six tests and, for each, **what a passing run would prove** and
//! **what the red message tells T-546 to build**.
//!
//! One run serves every test in a process ([`run`]), so the six assertions are six readings of the
//! *same* blind run rather than six differently-lucky ones.

use std::sync::{Arc, OnceLock};

use hk_e2e::{Fixture, SynthRequest, TruthItem};
use hk_model::TrunkProtocol;
use hk_pipeline::RunSummary;

use crate::blind::{BlindSource, TOP_K, blind_config, recording_start, start};
use crate::common::*;

const T545: &str = "T-545/SIGNAL-087";

/// **The no-lookup-and-tune rule, and the one place it is arguable.**
///
/// Nothing in this file passes a frequency, a modulation or a protocol to the system. The run is
/// configured with `json!({})` — the built-in chain registry and nothing else — and the only
/// frequency anything downstream sees is the centre the mock device reports, exactly as a real
/// HackRF reports where it is tuned.
///
/// The arguable part, flagged by `docs/19 §3.1` for this ticket to decide: the built-in
/// `trunk-cc-hunt` chain carries a trigger band `[851 MHz, 869 MHz]`
/// (`crates/hk-pipeline/src/chains/spec.rs`). **This suite's reading is that a band gate is not a
/// frequency lookup** — the system is told "digital LMR lives in this stretch of spectrum", which
/// is band-plan prior knowledge of the kind ADR-0017 permits as an *explainer*, not
/// "the control channel is at 851.0500 MHz". It never narrows the search inside the band, never
/// pre-populates the inventory and never commands a tune.
///
/// **But it is band-gated rather than measurement-driven, and that is a real limit**: the same
/// chain would not fire on a VHF (150–174 MHz), UHF (450–470 MHz) or 700 MHz trunked system, so
/// `SIGNAL-085`'s actual claim — find a 100 %-duty narrowband four-level emission *anywhere* —
/// is not what runs today. T-546 should make the occupancy trigger fire from the measured
/// structure (continuous + on a narrowband raster + four-level) with the band as a *prior on the
/// ranking* rather than a gate on the search. That is recorded here rather than asserted, because
/// it is a separate capability from the five this ticket scopes.
pub const NO_LOOKUP: &str = "band gate, not a frequency lookup; see the doc comment";

/// The a-priori tolerances. **Set from the standards figures in `docs/19 §2.1` and from the
/// physics, before any run, and never moved to make a fixture pass.** Each says where it comes
/// from; none of them is a number read off a previous run.
pub mod apriori {
    /// P25 Phase 1 symbol rate, Bd (`docs/19 §2.1`, sigidwiki + RR wiki + GopherTrunk agreeing).
    pub const SYMBOL_RATE_BD: f64 = 4800.0;
    /// Tolerance on a blind symbol-rate estimate, Bd. **1 %.** A symbol-rate estimator that
    /// cannot separate 4800 Bd from 4752/4848 cannot separate P25 Phase 1 (4800) from P25
    /// Phase 2 (6000) either — which is the discrimination the whole target rests on — so a
    /// looser bound would make the assertion vacuous. 1 % is far coarser than the ±3 Bd
    /// `docs/19 §5.2` expects an offline estimator to reach.
    pub const SYMBOL_RATE_TOL_BD: f64 = 48.0;
    /// C4FM outer deviation, Hz (`docs/19 §2.1`; VIAVI and GopherTrunk agree).
    pub const OUTER_DEVIATION_HZ: f64 = 1800.0;
    /// Tolerance on a blind deviation estimate, Hz. **±20 %.** The slicer thresholds sit at
    /// ±1200 Hz, i.e. 33 % below the outer level, so an estimate inside ±20 % still decides every
    /// symbol correctly; beyond it the four-level structure stops being a claim about C4FM.
    pub const DEVIATION_TOL_HZ: f64 = 360.0;
    /// The 12.5 kHz narrowband LMR raster (`docs/19 §1.2`; US 800 MHz, mandated 2013).
    pub const RASTER_HZ: f64 = 12_500.0;
    /// Centre tolerance on a detection, Hz: half a raster channel. Wider than this and the
    /// detection names the wrong channel, which in a trunked system is a different emitter.
    pub const CENTER_TOL_HZ: f64 = RASTER_HZ / 2.0;
    /// Occupied-bandwidth tolerance, as a ratio either way. A 12.5 kHz C4FM channel occupies
    /// ~8–10 kHz (`docs/19 §2.1`); ±60 % spans that whole documented range and the Carson figure
    /// the generator places, without admitting a neighbouring channel.
    pub const OBW_TOL_RATIO: f64 = 0.6;
    /// Time-extent tolerance, s. A control channel is continuous for the whole recording and a
    /// granted voice channel keys for ~100 ms (`docs/19 §2.1`, "control channel transmits
    /// continuously; voice channels key up per call"), so 50 ms is a fifth of the shortest event
    /// the fixture contains.
    pub const TIME_TOL_S: f64 = 0.05;
    /// The receiver clock error to impose, Hz at ~851 MHz. **Measured, not chosen**: −9.6 ppm on
    /// this project's own HackRF One (`docs/19 §7.6a`, a constant −8200 Hz correction at
    /// 852.456 MHz fitting all nine observed emissions onto the raster with a 390 Hz median
    /// residual). It is 5.5× `hk_detect::trunk::RASTER_TOLERANCE_HZ` and ⅔ of a channel.
    pub const CLOCK_ERROR_HZ: f64 = -8200.0;
    /// The HackRF One's fractional clock error, ppm (`docs/19 §7.6a`). Recorded so the figure
    /// above can be re-derived at another centre rather than copied.
    pub const CLOCK_ERROR_PPM: f64 = -9.6;
}

/// One blind run and everything read off it.
pub struct Run {
    /// Data directory, kept alive for the API server.
    pub dir: TempDir,
    /// Run summary (counters).
    pub summary: RunSummary,
    /// `/api/inventory` rows.
    pub rows: Vec<serde_json::Value>,
    /// The fixture, truth included — **only this test process may read it**.
    pub fx: Fixture,
}

impl Run {
    /// The truth annotation of the control channel.
    pub fn cc_truth(&self) -> &TruthItem {
        self.truth_of("trunk-control-channel")
    }

    fn truth_of(&self, kind: &str) -> &TruthItem {
        self.fx
            .truth
            .iter()
            .find(|t| t.kind == kind)
            .unwrap_or_else(|| panic!("[{T545}] the fixture carries no {kind:?} truth"))
    }

    /// The scenario-level truth object (`hackriff:truth` with `role: "scenario"`).
    pub fn scenario(&self) -> serde_json::Value {
        self.fx.scenario().unwrap().value.clone()
    }

    /// `/api/inventory` rows within half a raster channel of `f_hz`.
    pub fn rows_near(&self, f_hz: f64) -> Vec<&serde_json::Value> {
        self.rows
            .iter()
            .filter(|r| {
                r["f_center_hz"]
                    .as_f64()
                    .is_some_and(|f| (f - f_hz).abs() <= apriori::CENTER_TOL_HZ)
            })
            .collect()
    }
}

/// Runs the scene once per process, blind, through the mock SDR.
///
/// `clock_error` imposes the measured HackRF LO error of `docs/19 §7.6a` as an IQ shift — which
/// is what an LO error *is*: every emission moves by the same constant while the device still
/// reports the centre it believes it is tuned to.
fn blind_run(tag: &'static str, seed: u64, clock_error: bool) -> Option<Run> {
    // The BART-shaped scene: a continuous C4FM control channel issuing real TSBK traffic (band
    // plan, group voice grants, an encrypted grant, a late-entry grant), a continuous unframed
    // 4FSK decoy, bursty analogue FM neighbours on the same 12.5 kHz raster -- which is what
    // `docs/19 §7.4` actually measured on the air at 800 MHz -- and the granted voice channels
    // radiating where the grants say they are.
    //
    // 3 s: long enough for several complete keyings of the granted channel (0.10 s on / 0.15 s
    // off) so a time extent can be measured rather than fragmented, and for the control channel
    // to repeat its identity and band-plan messages many times over.
    let request = SynthRequest::new("trunk_encrypted_control_channel")
        .seed(seed)
        .param("duration_s", 3.0);
    // `synth_or_skip!` expands to a bare `return`, which this `Option`-returning helper cannot
    // use; the skip rule is otherwise identical (no `uv` skips unless HK_E2E_REQUIRE_SYNTH=1).
    let out = match request.generate() {
        Ok(out) => out,
        Err(err) if err.is_unavailable() && !hk_e2e::synth::require_synth() => {
            eprintln!("SKIP {T545}: {err}");
            return None;
        }
        Err(err) => panic!("synthetic scenario generation failed: {err}"),
    };
    let fx = out.fixture(0).unwrap();
    let source = BlindSource {
        iq_shift_hz: if clock_error {
            apriori::CLOCK_ERROR_HZ
        } else {
            0.0
        },
        ..BlindSource::default()
    };
    let cfg = blind_config(&fx.meta_path, tag, source, serde_json::json!({}));
    let dir = cfg.dir;
    let handle = start(cfg.cfg, cfg.replay);
    let counters = handle.counters();
    let summary = finish(handle);
    let server = serve_api(&dir.0, counters);
    let (_, rows) = api_inventory(server.local_addr());
    Some(Run {
        dir,
        summary,
        rows,
        fx,
    })
}

static RUN: OnceLock<Option<Arc<Run>>> = OnceLock::new();

/// The shared clean-clock run; `None` skips (no `uv`).
fn run() -> Option<Arc<Run>> {
    RUN.get_or_init(|| blind_run("s087", 545, false).map(Arc::new))
        .clone()
}

/// Prints everything the run produced, before any truth is opened. Every failure message below is
/// read together with this, so a red test always shows what the system *did* see.
fn report(run: &Run) {
    let c = |p: &str| run.summary.counter(p);
    eprintln!(
        "[{T545}] run produced: {} inventory rows, detections {}, cc passes {} candidates {} \
         demods {} confirmed {} systems {}, demod sessions {}, recordings {}",
        run.rows.len(),
        c("/detect/detections"),
        c("/chains/cc_passes"),
        c("/chains/cc_candidates"),
        c("/chains/cc_demods"),
        c("/chains/cc_confirmed"),
        c("/chains/cc_systems"),
        c("/demod/sessions"),
        c("/chains/recordings"),
    );
    for r in &run.rows {
        eprintln!(
            "[{T545}]   emitter {:.4} MHz bw {:.1} kHz  mod {:?}  estimated_params {}  \
             explanations {:?}",
            r["f_center_hz"].as_f64().unwrap_or(f64::NAN) / 1e6,
            r["bandwidth_hz"].as_f64().unwrap_or(f64::NAN) / 1e3,
            (r["family"].as_str(), r["lifecycle"].as_str()),
            if r["estimated_params"].is_null() {
                "null".to_owned()
            } else {
                r["estimated_params"].to_string()
            },
            r["explanations"]
                .as_array()
                .map(|a| a
                    .iter()
                    .take(TOP_K)
                    .filter_map(|e| e["service"].as_str())
                    .collect::<Vec<_>>())
                .unwrap_or_default(),
        );
    }
}

// ---------------------------------------------------------------------------------------------
// (A) THE CONTROL. Not `#[ignore]`d: it passes today and must keep passing.
// ---------------------------------------------------------------------------------------------

/// **Ticket assertion (1): the emission is detected as a time–frequency region.**
///
/// What a passing run proves: the mock device, the truth-stripping vault and the blind detector
/// together find the control channel's centre, occupied bandwidth **and time extent** from the IQ
/// alone, with no frequency ever handed in — a `docs/07` Detection, not a carrier on a list.
///
/// This is the suite's **control**. It is the reason the other five failing means something: if
/// this went red too, the honest reading would be "the harness is broken", not "the capability is
/// missing" — the same distinction `docs/19 §5.4` draws about the capture itself. Keep it green.
#[test]
fn a_the_emission_is_detected_blind_as_a_time_frequency_region() {
    let Some(run) = run() else { return };
    report(&run);

    // ---- Truth, opened only now, and only to check the answer.
    let truth = run.cc_truth();
    let f_truth = 0.5 * (truth.f_lo_hz + truth.f_hi_hz);
    let bw_truth = truth.f_hi_hz - truth.f_lo_hz;
    let t0 = recording_start(&run.fx);
    let repo = repo(&run.dir.0);
    let dets = repo
        .detections_in_region(&hk_model::Region::new(
            hk_model::FreqRange::new(0.0, 7.0e9),
            ever(),
        ))
        .unwrap();

    let matching: Vec<_> = dets
        .iter()
        .filter(|d| (d.f_center_hz - f_truth).abs() <= apriori::CENTER_TOL_HZ)
        .collect();
    assert!(
        !matching.is_empty(),
        "[{T545}] (1) DETECTION: nothing was detected within {:.1} kHz of the control channel at \
         {:.4} MHz. The run made {} detections in all: {:?}. T-546 must make blind detection find \
         a continuous narrowband emission in a band full of bursty analogue FM neighbours.",
        apriori::CENTER_TOL_HZ / 1e3,
        f_truth / 1e6,
        dets.len(),
        dets.iter()
            .map(|d| format!("{:.4} MHz", d.f_center_hz / 1e6))
            .collect::<Vec<_>>(),
    );

    // Bandwidth: a measurement, within the documented 8-10 kHz for a 12.5 kHz C4FM channel.
    let best = matching
        .iter()
        .min_by(|a, b| {
            (a.f_center_hz - f_truth)
                .abs()
                .total_cmp(&(b.f_center_hz - f_truth).abs())
        })
        .unwrap();
    let lo = bw_truth * (1.0 - apriori::OBW_TOL_RATIO);
    let hi = bw_truth * (1.0 + apriori::OBW_TOL_RATIO);
    assert!(
        (lo..=hi).contains(&best.obw_hz),
        "[{T545}] (1) BANDWIDTH: measured {:.2} kHz, outside {:.2}-{:.2} kHz around the truth's \
         {:.2} kHz. A detection that names the wrong width names a different emitter.",
        best.obw_hz / 1e3,
        lo / 1e3,
        hi / 1e3,
        bw_truth / 1e3,
    );

    // Time extent: a control channel is continuous, so the union of its detections must span
    // essentially the whole recording. This is the half a "persistent carrier" model would get
    // for free and a time-frequency-region model has to actually measure (ADR-0017).
    let span_start = matching.iter().map(|d| d.time.start).min().unwrap();
    let span_end = matching.iter().map(|d| d.time.end).max().unwrap();
    let want_end = t0.saturating_add_nanos((truth.t_end_s * 1e9) as i64);
    let covered_s = (span_end.as_unix_nanos() - span_start.as_unix_nanos()) as f64 / 1e9;
    let truth_s = truth.t_end_s - truth.t_start_s;
    eprintln!(
        "[{T545}] (1) control channel {:.4} MHz: {} detections, obw {:.2} kHz, extent {:.3} s of \
         the truth's {:.3} s",
        best.f_center_hz / 1e6,
        matching.len(),
        best.obw_hz / 1e3,
        covered_s,
        truth_s,
    );
    assert!(
        covered_s >= truth_s - 2.0 * apriori::TIME_TOL_S,
        "[{T545}] (1) TIME EXTENT: the detections cover {covered_s:.3} s of a {truth_s:.3} s \
         continuous emission. A signal is a time-frequency region (ADR-0017): a detection with no \
         extent is not one. Latest detection ends {:.3} s before the truth does.",
        (want_end.as_unix_nanos() - span_end.as_unix_nanos()) as f64 / 1e9,
    );
}

// ---------------------------------------------------------------------------------------------
// (B) RED. Blind parameter estimation.
// ---------------------------------------------------------------------------------------------

/// **Ticket assertion (2): the parameters are estimated from the signal.**
///
/// What a passing run would prove: the run measured *this emission's* modulation family, symbol
/// rate and deviation from the IQ and **persisted them on the emitter**, so a downstream chooser
/// (and the UI's "Use" suggestions, ADR-0013 §4.6) has something measured to act on. Measuring
/// them inside a chain and throwing them away is not the capability: `docs/api.md` is explicit
/// that `estimated_params` is measured values only, never a fabricated default — so a null here
/// is honest, and honest is also *absent*.
///
/// What its red message tells T-546 to build, and the measured shape of the gap is worse than
/// "not done yet" — it is **inverted**. On this scene the run *does* record `estimated_params`
/// for the bursty analogue-FM neighbours, and records **none** for the one emission it confirmed
/// as a digital control channel. And what it records for the neighbours reads
/// `"modulation": "2fsk", "mod_order": 2, "deviation_hz": 2397` — a two-level label on analogue
/// FM. **There is no four-level answer available for the estimator to give**, so even reaching
/// the control channel would not produce one. Two things therefore have to change: the digital
/// emission must get a session at all, and `mod_order` must be able to say 4.
#[test]
fn b_the_modulation_symbol_rate_and_deviation_are_estimated_from_the_signal() {
    let Some(run) = run() else { return };
    report(&run);

    let truth = run.cc_truth();
    let f_truth = 0.5 * (truth.f_lo_hz + truth.f_hi_hz);
    let near = run.rows_near(f_truth);
    assert!(
        !near.is_empty(),
        "[{T545}] (2) no inventory emitter within {:.1} kHz of the control channel at {:.4} MHz, \
         so there is nothing to carry parameters. Fix assertion (1) first.",
        apriori::CENTER_TOL_HZ / 1e3,
        f_truth / 1e6,
    );

    let with_params: Vec<_> = near
        .iter()
        .filter(|r| !r["estimated_params"].is_null())
        .collect();
    assert!(
        !with_params.is_empty(),
        "[{T545}] (2) PARAMETER ESTIMATION IS MISSING. {} emitter(s) match the control channel \
         and every one has `estimated_params: null` -- the run never recorded a demodulation \
         session for a digital emission, so nothing it measured became a docs/07 object.\n\
         \n\
         AND THE GAP IS INVERTED, WHICH IS WORSE THAN NOT DONE YET: the same run DOES record \
         estimated_params for the bursty ANALOGUE FM neighbours on this raster, and records none \
         for the one emission it confirmed as a digital control channel. Read the rows above.\n\
         \n\
         WHAT T-546 MUST BUILD:\n\
         - a blind estimator for a DIGITAL emission (the four-level deviation histogram and the \
           symbol-rate search of docs/19 §4.4 step 3) whose result is persisted as \
           hk_model::EstimatedParams, reachable on /api/inventory;\n\
         - covering at least: modulation family (4-level FM vs a linear modulation), symbol rate, \
           outer deviation, occupied bandwidth;\n\
         - a four-level ANSWER: every estimated_params row this run wrote says mod_order 2 and \
           modulation \"2fsk\", including on analogue FM. There is no 4-level outcome available, \
           so reaching the control channel would still not produce one;\n\
         - the C4FM demodulator that already measures outer_deviation_hz and residual_cfo_hz \
           (crates/hk-demod/src/fsk/c4fm.rs) is the input, not the gap: the gap is that \
           trunk-cc-hunt consumes its dibits and discards its measurements.\n\
         \n\
         EXPECTED VALUES, a priori (docs/19 §2.1, standards-quoted, NOT read off a run): \
         symbol rate {:.0} +/- {:.0} Bd, outer deviation {:.0} +/- {:.0} Hz, four levels.\n\
         \n\
         WHY IT MATTERS: 4800 Bd four-level FM vs 6000 Bd linear is the ONE clean discriminator \
         between P25 Phase 1 and Phase 2 (docs/19 §2.2), and Phase 2 is out of reach for this \
         build (SupportLevel::Unimplemented). Without this measurement the system cannot tell a \
         target it can decode from one it cannot.",
        near.len(),
        apriori::SYMBOL_RATE_BD,
        apriori::SYMBOL_RATE_TOL_BD,
        apriori::OUTER_DEVIATION_HZ,
        apriori::DEVIATION_TOL_HZ,
    );

    // Past the gap: the measurements themselves, against the a-priori figures.
    let p = &with_params[0]["estimated_params"];
    let sym = p["symbol_rate_hz"].as_f64();
    assert!(
        sym.is_some_and(|s| (s - apriori::SYMBOL_RATE_BD).abs() <= apriori::SYMBOL_RATE_TOL_BD),
        "[{T545}] (2) SYMBOL RATE: measured {sym:?} Bd, wanted {:.0} +/- {:.0} Bd \
         (docs/19 §2.1). {p}",
        apriori::SYMBOL_RATE_BD,
        apriori::SYMBOL_RATE_TOL_BD,
    );
    let dev = p["deviation_hz"].as_f64();
    assert!(
        dev.is_some_and(
            |d| (d.abs() - apriori::OUTER_DEVIATION_HZ).abs() <= apriori::DEVIATION_TOL_HZ
        ),
        "[{T545}] (2) DEVIATION: measured {dev:?} Hz, wanted +/-{:.0} +/- {:.0} Hz \
         (docs/19 §2.1). {p}",
        apriori::OUTER_DEVIATION_HZ,
        apriori::DEVIATION_TOL_HZ,
    );
    let modulation = p["modulation"].as_str().unwrap_or("");
    assert!(
        modulation.contains("c4fm") || modulation.contains("4fsk") || modulation.contains("4-fsk"),
        "[{T545}] (2) MODULATION: the estimator called it {modulation:?}. Four discrete \
         frequency modes at +/-600 and +/-1800 Hz clocked at 4800 Bd is C4FM and nothing else \
         (docs/19 §2.1). {p}",
    );
    assert_eq!(
        p["mod_order"].as_u64(),
        Some(4),
        "[{T545}] (2) MODULATION ORDER: the estimator reports mod_order {:?} for a FOUR-level \
         emission. Every estimated_params row this run wrote says 2 -- including on analogue FM \
         -- so this is not a miss on one signal, it is an answer the estimator cannot give. {p}",
        p["mod_order"],
    );
}

// ---------------------------------------------------------------------------------------------
// (C) RED. The headline gap: nothing auto-selects a decoding pipeline.
// ---------------------------------------------------------------------------------------------

/// **Ticket assertion (3): the demod + decode pipeline is auto-selected, without being told.**
///
/// What a passing run would prove: the north-star workflow's fourth step — the system, given only
/// a blindly-detected emission, chooses *which* demodulator and *which* decoder to run on it from
/// what it measured, and says why in terms of the measurements. That is MAUTO's whole claim
/// (ADR-0015, `docs/15 §8`).
///
/// What its red message tells T-546 to build: the engine. `POST /api/analyze` validates and
/// resolves its target and then answers `501 not_implemented` by design (`docs/api.md`,
/// `crates/hk-api/src/analyze.rs`); `hk_model::classify::seed` describes itself as
/// *"an interface, not an engine"*; `hk_recipe::matching` ranks four hand-written recipes and
/// tunes nothing. **There is no code path in this repository that picks a demod+decode chain for
/// a digital emission from measurements**, so this assertion cannot fail for any other reason.
#[test]
fn c_the_demod_and_decode_pipeline_is_auto_selected_from_the_measurements() {
    let Some(run) = run() else { return };
    report(&run);

    let truth = run.cc_truth();
    let f_truth = 0.5 * (truth.f_lo_hz + truth.f_hi_hz);
    let near = run.rows_near(f_truth);
    assert!(
        !near.is_empty(),
        "[{T545}] (3) no inventory emitter within {:.1} kHz of {:.4} MHz to analyze. Fix \
         assertion (1) first.",
        apriori::CENTER_TOL_HZ / 1e3,
        f_truth / 1e6,
    );
    let emitter_id = near[0]["id"].as_str().expect("an emitter row has an id");

    // The request names ONLY the emitter the system found by itself. No frequency, no modulation,
    // no protocol crosses this boundary -- the whole point of the blind rule.
    let server = serve_api_with_control(&run.dir.0);
    let (status, body) = api_post(
        server.local_addr(),
        "/api/analyze",
        &serde_json::json!({ "emitter_id": emitter_id }).to_string(),
    );
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::String(
        String::from_utf8_lossy(&body).into_owned(),
    ));

    assert_eq!(
        status,
        200,
        "[{T545}] (3) AUTO-SELECTION DOES NOT EXIST. POST /api/analyze {{emitter_id}} on the \
         blindly-detected control channel at {:.4} MHz answered {status}: {v}\n\
         \n\
         WHAT T-546 MUST BUILD (ADR-0015, docs/15 §8):\n\
         - an engine behind POST /api/analyze that, from the emitter's MEASUREMENTS alone, \
           searches demod/framing/FEC structure and parameters and returns the best decoding \
           pipeline with its evidence;\n\
         - the decision must be INSPECTABLE: the answer says WHY that pipeline was chosen, in \
           terms of what was measured (a black box that happens to decode P25 is not the \
           capability);\n\
         - the pieces that exist and should be reused rather than re-invented: \
           hk_recipe::matching (ranks recipes against measured parameters), \
           hk_model::classify::seed (the interface), hk_demod::mode::AnalogAuto (the working \
           analogue auto-selector and the shape to follow), and the M4 P25 decoder \
           (T-272/T-299) -- the user was explicit that MAUTO REUSES the existing P25 decoder \
           rather than rebuilding it;\n\
         - nothing in the answer may come from the band plan deciding what the signal IS. The \
           allocation may rank an explanation (assertion 5); it may not choose the decoder.\n\
         \n\
         NOTE: 501 here is the DESIGNED answer, not a bug -- docs/api.md says the route validates \
         its target and returns not_implemented until MAUTO lands. This test is the thing that \
         goes green when it does.",
        f_truth / 1e6,
    );

    // Past the gap: the answer has to name a pipeline and justify it from measurements.
    let pipeline = v["pipeline"].as_object();
    assert!(
        pipeline.is_some(),
        "[{T545}] (3) /api/analyze answered 200 with no `pipeline`: {v}",
    );
    let evidence = v["evidence"].as_array().map(Vec::len).unwrap_or(0);
    assert!(
        evidence > 0,
        "[{T545}] (3) the chosen pipeline carries no evidence. An auto-selection that cannot say \
         why is not inspectable (T-546's acceptance): {v}",
    );
    let chain = v["pipeline"]["demod"].as_str().unwrap_or("");
    assert!(
        chain.contains("c4fm") || chain.contains("4fsk"),
        "[{T545}] (3) the engine chose demodulator {chain:?} for a four-level 4800 Bd FM \
         emission: {v}",
    );
}

// ---------------------------------------------------------------------------------------------
// (D) RED. The decode, what it already reaches, and where it stops.
// ---------------------------------------------------------------------------------------------

/// **Ticket assertion (4): it decodes to the expected output, checked against the hidden truth.**
///
/// This one needs its parts separated, because **two thirds of it already work** and saying so is
/// the only way the remaining third is visible. Measured on this scene, a normal blind run:
///
/// - confirms the control channel, names it `P25Phase1`, reads `IDEN_UP` into a band plan and
///   resolves seventeen grants to 851.0750 / 851.1250 / 851.7375 MHz — all asserted below as
///   **preconditions**, and already covered by `acceptance_m4::t268_tsbk`;
/// - and then **stops**. The decoded system is a `trunk_system` row. The emission the run detected
///   blind at the same frequency is an inventory emitter. **Nothing connects them**: that emitter
///   has `family: null`, `estimated_params: null`, and no classification. The two `docs/07` object
///   graphs never meet.
///
/// That last part is the gap, and it is the product vision's step 4 exactly: *"successful decode
/// confirms it"*. A decode that names a system in a side table, while the signal the user is
/// looking at in the inventory stays an unexplained blob, has not confirmed anything the user can
/// see.
///
/// **The circularity caveat applies to the decode half and cannot be avoided here (T-299).** This
/// fixture's framing and this repo's P25 decoder were written from the same reading of the same
/// references — `py/hkpy/synth/trunking.py` declares `"coding": "none"` and
/// `crates/hk-detect/src/trunk/confirm.rs:980-1035` lists five ways the decoder is non-compliant
/// (CRC variant, no trellis, no deinterleaver, no status symbols, no NID) — so **a shared
/// misreading passes both**, and everything the preconditions below assert is therefore a claim
/// about the *plumbing*, never about standards compliance. Only an independent oracle
/// (SDRTrunk / OP25 / DSD+ offline over the same IQ, behind the ADR-0010 plugin boundary,
/// hardware tier only) over a real capture closes that, and neither exists (`docs/19 §7`).
///
/// The one assertion here that is **not** circular is precondition 3: a grant names a channel
/// number, the announced band plan turns it into a frequency, and RF energy has to be radiating
/// there. The arithmetic comes from the protocol decoder and the energy comes from the FFT, and
/// those two were not written from the same source (`docs/19 §5.3`).
#[test]
fn d_the_decode_reaches_the_emission_the_run_detected() {
    let Some(run) = run() else { return };
    report(&run);

    let repo = repo(&run.dir.0);
    let systems = repo.trunk_systems().unwrap();
    eprintln!(
        "[{T545}] (4) trunk systems on file: {:?}",
        systems
            .iter()
            .map(|s| (
                s.protocol,
                s.cc_freq_hz.map(|f| f / 1e6),
                s.channel_plan.len(),
            ))
            .collect::<Vec<_>>(),
    );

    // ---- PRECONDITION 1 (passes today; t268_tsbk is its home). The protocol was named from the
    // messages, not left Unknown as an unread control channel must be (T-287).
    assert_eq!(systems.len(), 1, "[{T545}] (4) exactly one control channel");
    let sys = &systems[0];
    assert_eq!(
        sys.protocol,
        TrunkProtocol::P25Phase1,
        "[{T545}] (4) PRECONDITION: TSBKs are decoded during a normal run, so the protocol is \
         named. A failure here is a regression in T-268's ground, not this ticket's gap.",
    );

    // ---- PRECONDITION 2 (passes today). The announced band plan was read; without it a grant's
    // channel number is just a number.
    assert!(
        !sys.channel_plan.is_empty(),
        "[{T545}] (4) PRECONDITION: the IDEN_UP band plan was not read, so no grant below could \
         resolve. Regression in T-268's ground.",
    );

    // ---- PRECONDITION 3 (passes today) — THE NON-CIRCULAR ONE. Grant -> band plan -> frequency,
    // and RF energy has to be radiating there. Protocol decoder and FFT, independently written,
    // agreeing (docs/19 §5.3).
    let scenario = run.scenario();
    let granted_hz = scenario["trunking"]["tsbk"]["follow_target_hz"]
        .as_f64()
        .expect("the scene grants a voice channel, chosen as a frequency first");
    let grants = repo
        .grants_for_system(sys.id, hk_model::Timestamp::UNIX_EPOCH, 10_000)
        .unwrap();
    let resolved: Vec<f64> = grants.iter().filter_map(|g| g.f_hz).collect();
    eprintln!(
        "[{T545}] (4) {} grant(s) resolved to {:?} MHz",
        grants.len(),
        resolved.iter().map(|f| f / 1e6).collect::<Vec<_>>(),
    );
    assert!(
        resolved
            .iter()
            .any(|f| (f - granted_hz).abs() <= apriori::CENTER_TOL_HZ),
        "[{T545}] (4) PRECONDITION, AND THE ONLY NON-CIRCULAR CLAIM IN THIS TEST: resolving a \
         grant through the announced band plan must land within {:.1} kHz of {:.4} MHz, where the \
         recording independently shows RF energy keying up. Resolved: {:?} MHz. This is the one \
         Tier-C claim a shared misreading of the P25 references cannot fake (docs/19 §5.3) -- if \
         it ever goes red, believe it over everything else in this file.",
        apriori::CENTER_TOL_HZ / 1e3,
        granted_hz / 1e6,
        resolved.iter().map(|f| f / 1e6).collect::<Vec<_>>(),
    );

    // ---- THE GAP. The decode must reach the emission the run detected.
    let cc_hz = sys
        .cc_freq_hz
        .expect("a confirmed control channel has a frequency");
    let near = run.rows_near(cc_hz);
    assert!(
        !near.is_empty(),
        "[{T545}] (4) THE DECODE AND THE INVENTORY DESCRIBE DIFFERENT WORLDS. A P25 control \
         channel was decoded at {:.4} MHz and NO inventory emitter exists within {:.1} kHz of it. \
         The run detected the emission (assertion 1 passes) and decoded it (preconditions above \
         pass) and the two never met.",
        cc_hz / 1e6,
        apriori::CENTER_TOL_HZ / 1e3,
    );

    let described: Vec<_> = near
        .iter()
        .filter(|r| !r["family"].is_null() || !r["classification"].is_null())
        .collect();
    assert!(
        !described.is_empty(),
        "[{T545}] (4) THE DECODE NEVER CONFIRMS THE SIGNAL. A P25 Phase 1 control channel was \
         fully decoded at {:.4} MHz -- protocol named, band plan read, {} grants resolved -- and \
         the inventory emitter at that same frequency still reads family null, classification \
         null, estimated_params null. Rows there: {:?}\n\
         \n\
         WHAT T-546 MUST BUILD:\n\
         - feed the decode back onto the EMITTER as evidence. hk_pipeline::family already maps \
           Evidence::Decoder(id) and Evidence::Label(..) to a service family; the trunking chain \
           calls neither, so the strongest evidence the run possesses -- a CRC-valid decode of a \
           trunked control channel -- contributes nothing to what the inventory says the signal \
           IS;\n\
         - `docs/api.md` is explicit that only a CRC-valid decode confirms what a signal is \
           (ADR-0016 §4.7). Here one happened and confirmed nothing;\n\
         - this is the product vision's step 4 in the user's own words: \"successful decode \
           confirms it\". A system named in a side table while the signal in the inventory stays \
           an unexplained blob is not a confirmation the user can see;\n\
         - the trunk_system row and the emitter must be LINKED, so the control channel's \
           frequency is one object, not two coincident ones.\n\
         \n\
         WHAT THIS CANNOT PROVE, SAID PLAINLY (T-299/T-300): going green here proves the plumbing \
         exists. It does NOT prove the decoder is standards-compliant -- this fixture's framing \
         is not compliant either (py/hkpy/synth/trunking.py: \"coding\": \"none\"), and the five \
         gaps T-300 recorded in confirm.rs:980-1035 (augmented CRC-CCITT, rate-1/2 trellis, \
         98-dibit deinterleave, status symbols every 35 dibits, the 64-bit NID) are what will \
         stop this reading REAL off-air P25. Budget all five; T-300's finding is that fixing one \
         buys no compliance and hides the rest.",
        cc_hz / 1e6,
        grants.len(),
        near.iter()
            .map(|r| (
                r["f_center_hz"].as_f64().unwrap_or(f64::NAN) / 1e6,
                r["family"].clone(),
                r["lifecycle"].clone(),
            ))
            .collect::<Vec<_>>(),
    );
}

// ---------------------------------------------------------------------------------------------
// (E) The exploration-first rule: the database EXPLAINS, never identifies. Green, and its
//     quality bar, which is red.
// ---------------------------------------------------------------------------------------------

/// **Ticket assertion (5): a sensible explanation ranks among the top suggestions.**
///
/// What a passing run proves: the fourth step of the product vision, in the right order. The
/// emission is found **blind**, and only then does the known-signal database offer a ranked,
/// reasoned suggestion for it. The bundled allocation row `public-safety-800-b` (851–869 MHz,
/// tags `public-safety;800mhz;smr`) reaches the emitter's top-{TOP_K} as `public-safety`.
///
/// **This passes today**, so it is not `#[ignore]`d: it is the second green control in this file
/// and a standing guard that the explanation path still runs. Its *quality* is the gap, and that
/// is [`e2_the_explanation_rests_on_measured_evidence_not_only_the_allocation`].
///
/// **The direction of this assertion is the whole exploration-first rule.** It checks an
/// explanation is *offered*; it never checks it is right, never looks 851.05 MHz up to decide
/// where to tune, and would be equally satisfied by an emission 150 kHz off the allocation with
/// `off-allocation` flagged — a mismatch is interesting, not an error to snap away.
#[test]
fn e_a_sensible_explanation_ranks_among_the_top_suggestions() {
    let Some(run) = run() else { return };
    report(&run);

    let truth = run.cc_truth();
    let f_truth = 0.5 * (truth.f_lo_hz + truth.f_hi_hz);
    let near = run.rows_near(f_truth);
    assert!(
        !near.is_empty(),
        "[{T545}] (5) no inventory emitter near {:.4} MHz to explain. Fix assertion (1) first.",
        f_truth / 1e6,
    );

    // Accepted explanations, set a priori from the bundled US allocation table's own vocabulary:
    // 851-869 MHz is `public-safety-800-b`, and `public-safety` is the canonical family name in
    // hk_pipeline::family::SERVICES.
    const WANT: &[&str] = &["public-safety"];
    let tops = top_services(&near);
    assert!(
        tops.iter()
            .any(|(_, top)| top.iter().any(|s| WANT.contains(&s.as_str()))),
        "[{T545}] (5) NO SENSIBLE EXPLANATION IS OFFERED. The emitter(s) at the control \
         channel's frequency carry top-{TOP_K} explanations {tops:?}, and none is {WANT:?}. The \
         allocation row EXISTS (crates/hk-context/data/us-47cfr2106-compact.csv: \
         public-safety-800-b, 851-869 MHz), so this would be a plumbing regression, not missing \
         reference data.\n\
         THE DIRECTION MATTERS: this asserts an explanation is OFFERED beside the measurement. It \
         must never become a lookup -- the database explains, never identifies, never \
         pre-populates the inventory and never overrides what was measured (ADR-0017).",
    );
}

/// **The quality bar on assertion (5): the explanation must rest on what was measured.**
///
/// [`e_a_sensible_explanation_ranks_among_the_top_suggestions`] passes, and inspecting *why* it
/// passes is the finding: the only thing behind `public-safety` at 851 MHz is the **allocation
/// row**. The same suggestion, with the same score, would be offered for an empty channel, for
/// the analogue FM neighbours, and for a bare carrier — every emitter in this scene that carries
/// an explanation at all carries exactly that one. An explanation that does not discriminate is
/// not evidence; it is a map of the band.
///
/// Meanwhile the run is holding much better evidence and spending none of it: this emission is
/// **continuous at 100 % duty, narrowband, on the 12.5 kHz raster, four-level, frame-synced, and
/// its TSBKs decode**. `hk_pipeline::family` has no mapping from any of that to a service, so a
/// confirmed trunked control channel contributes `evidence_confidence` 0 and falls back to
/// allocation-only — the `allocation-only` flag on the explanation says so in as many words.
///
/// What its red message tells T-546 to build: make the measurement pay. The explanation for a
/// confirmed control channel should read *"continuous, on the narrowband LMR raster, four-level,
/// frame-synced, TSBKs CRC-valid"* and outrank the bare allocation — which is also what makes a
/// **mismatch** legible later, since an emission with strong signal evidence sitting off its
/// allocation is the interesting case ADR-0017 asks to flag rather than snap.
#[test]
fn e2_the_explanation_rests_on_measured_evidence_not_only_the_allocation() {
    let Some(run) = run() else { return };
    report(&run);

    let truth = run.cc_truth();
    let f_truth = 0.5 * (truth.f_lo_hz + truth.f_hi_hz);
    let near = run.rows_near(f_truth);
    assert!(
        !near.is_empty(),
        "[{T545}] (5b) no inventory emitter near {:.4} MHz. Fix assertion (1) first.",
        f_truth / 1e6,
    );

    let explanations: Vec<&serde_json::Value> = near
        .iter()
        .filter_map(|r| r["explanations"].as_array())
        .flatten()
        .take(TOP_K)
        .collect();
    eprintln!(
        "[{T545}] (5b) explanations in full: {:?}",
        explanations
            .iter()
            .map(|e| (
                e["service"].clone(),
                e["evidence_confidence"].clone(),
                e["flags"].clone()
            ))
            .collect::<Vec<_>>(),
    );

    // A priori: "the allocation alone" is exactly evidence_confidence == 0 and the
    // `allocation-only` flag, which is hk_pipeline::family's own vocabulary for it -- not a
    // threshold this test invented.
    let measured = explanations.iter().any(|e| {
        e["evidence_confidence"].as_f64().unwrap_or(0.0) > 0.0
            && !e["flags"]
                .as_array()
                .is_some_and(|f| f.iter().any(|x| x == "allocation-only"))
    });
    assert!(
        measured,
        "[{T545}] (5b) THE EXPLANATION IS ALLOCATION-ONLY. Every explanation offered for the \
         control channel at {:.4} MHz scores evidence_confidence 0 and carries the \
         `allocation-only` flag: the ONLY thing behind it is that 851-869 MHz is a public-safety \
         allocation. The identical suggestion is offered for the analogue FM neighbours and would \
         be offered for an empty channel -- it does not discriminate, so it is not evidence.\n\
         \n\
         WHAT THE RUN ALREADY MEASURED AND DID NOT SPEND: this emission is continuous at 100 % \
         duty, narrowband, on the 12.5 kHz LMR raster, four-level, frame-synced, and its TSBKs \
         decode.\n\
         \n\
         WHAT T-546 MUST BUILD:\n\
         - a mapping in hk_pipeline::family from trunked-control-channel evidence (a confirmed \
           CC, a decoded protocol, raster fit, continuous duty) to a service family, so the \
           ranking carries signal evidence and not just an allocation row;\n\
         - the explanation's REASONS must name the measurements, per docs/api.md's \"evidence, \
           never identity\": the user is owed \"continuous, on the narrowband raster, four-level, \
           frame-synced, TSBKs CRC-valid\", not \"this band is public safety\";\n\
         - and keep the direction: stronger evidence must make a MISMATCH more legible, not less. \
           An emission with strong signal evidence sitting off its allocation is the interesting \
           case ADR-0017 asks to flag, never to snap onto the plan.",
        f_truth / 1e6,
    );
}

/// Top-{TOP_K} explanation service names per row, with the row's frequency in MHz.
fn top_services(rows: &[&serde_json::Value]) -> Vec<(f64, Vec<String>)> {
    rows.iter()
        .map(|r| {
            (
                r["f_center_hz"].as_f64().unwrap_or(f64::NAN) / 1e6,
                r["explanations"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .take(TOP_K)
                            .filter_map(|e| e["service"].as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default(),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// (F) RED. The one thing phase 1 actually measured off the air.
// ---------------------------------------------------------------------------------------------

/// **The receiver's own clock error, from the only BART measurement that survived (`docs/19
/// §7.6a`).**
///
/// T-544's capture found no BART, but it did measure this receiver: the HackRF One's plain crystal
/// is **−9.6 ppm** off, which at 852 MHz is **−8.2 kHz** — nearly ⅔ of a 12.5 kHz channel, and
/// **5.5× `hk_detect::trunk::RASTER_TOLERANCE_HZ` (1500 Hz)**. Nine emissions appeared 29–80 kHz
/// off the published raster until one constant correction put every one of them back on it, with
/// a 390 Hz median residual. Without that correction they looked spurious.
///
/// This test reproduces exactly that on the mock device: the same scene, shifted by the measured
/// constant, so **every** emission is off-raster together. An LO error does precisely this — the
/// device still reports the centre it believes it is tuned to.
///
/// What a passing run would prove: the system finds the control channel anyway, and reports the
/// offset as a **measured** quantity rather than assuming zero. `docs/19 §7.6a` states the
/// requirement directly: *"any raster-fitting or band-plan comparison this project does at
/// 800 MHz must estimate and remove it rather than assume zero."* Blind raster fitting is
/// something the detector ought to be doing anyway (it is `docs/19 §4.4` step 1), and it falls
/// out of the data in one line.
///
/// What its red message tells T-546 to build: fit the raster offset before applying
/// `RASTER_TOLERANCE_HZ`, and record the fitted correction as provenance on the detection — it is
/// a property of the receiver, not of the signal, and the same correction should then apply to
/// every emission in the capture.
#[test]
fn f_the_receiver_clock_error_is_measured_not_assumed_zero() {
    let Some(run) = blind_run("s087clk", 545, true) else {
        return;
    };
    report(&run);

    let truth = run.cc_truth();
    // Truth moves with the shift: the emission really is where the shift put it.
    let f_truth = 0.5 * (truth.f_lo_hz + truth.f_hi_hz) + apriori::CLOCK_ERROR_HZ;

    let repo = repo(&run.dir.0);
    let systems = repo.trunk_systems().unwrap();
    eprintln!(
        "[{T545}] (F) with a {:.0} Hz ({:.1} ppm) receiver clock error, systems on file: {:?} MHz; \
         raster tolerance is {:.0} Hz",
        apriori::CLOCK_ERROR_HZ,
        apriori::CLOCK_ERROR_PPM,
        systems
            .iter()
            .filter_map(|s| s.cc_freq_hz)
            .map(|f| f / 1e6)
            .collect::<Vec<_>>(),
        hk_detect::trunk::RASTER_TOLERANCE_HZ,
    );

    let found = systems
        .iter()
        .filter_map(|s| s.cc_freq_hz)
        .any(|f| (f - f_truth).abs() <= apriori::CENTER_TOL_HZ);
    assert!(
        found,
        "[{T545}] (F) A REAL RECEIVER'S CLOCK ERROR HIDES THE CONTROL CHANNEL. The same scene \
         that is found with a perfect clock is not found once every emission is shifted by \
         {:.0} Hz ({:.1} ppm at 852 MHz) -- the error MEASURED on this project's own HackRF One \
         (docs/19 §7.6a). It is {:.1}x hk_detect::trunk::RASTER_TOLERANCE_HZ ({:.0} Hz), so the \
         raster fit rejects every channel and candidacy never happens.\n\
         \n\
         Expected the control channel at {:.4} MHz; systems on file: {:?} MHz.\n\
         \n\
         WHAT T-546 MUST BUILD:\n\
         - FIT the raster offset across the occupied channels before applying the tolerance, \
           instead of assuming the receiver is on frequency. docs/19 §7.6a: one constant \
           correction put all nine observed emissions back on the 12.5 kHz raster with a 390 Hz \
           median residual, and without it they looked like spurs;\n\
         - record the fitted correction as receiver PROVENANCE, not as a property of the signal: \
           it is the same number for every emission in the capture, which is both how you \
           recognise it and what makes it cheap;\n\
         - this is docs/19 §4.4 step 1 (\"finding the grid without being told it\") and is a \
           capability in its own right, not a workaround.\n\
         \n\
         WHY IT IS NOT OPTIONAL: a HackRF One has a plain crystal and no TCXO. Every real capture \
         this target will ever run on has an error of this order. A build that only works at \
         0 ppm works on synthetic IQ and nothing else -- which is exactly the failure mode this \
         phase-2 suite exists to expose BEFORE the fixture arrives.",
        apriori::CLOCK_ERROR_HZ,
        apriori::CLOCK_ERROR_PPM,
        apriori::CLOCK_ERROR_HZ.abs() / hk_detect::trunk::RASTER_TOLERANCE_HZ,
        hk_detect::trunk::RASTER_TOLERANCE_HZ,
        f_truth / 1e6,
        systems
            .iter()
            .filter_map(|s| s.cc_freq_hz)
            .map(|f| f / 1e6)
            .collect::<Vec<_>>(),
    );
}

/// An API server over a finished run wired for **control** routes as well as reads: the audit log
/// is present, so `POST /api/analyze` reaches its own answer instead of being refused with
/// `503 control is disabled: this server has no audit log`. Without this, assertion (3) would fail
/// for a harness reason and teach T-546 nothing — the exact trap this ticket names.
fn serve_api_with_control(dir: &std::path::Path) -> hk_api::Server {
    let config = hk_api::ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        hk_api::Token::from_config(API_TOKEN).unwrap(),
    );
    let state = hk_api::ApiState {
        inventory: Some(Arc::new(std::sync::Mutex::new(repo(dir)))),
        bookmarks: Some(Arc::new(std::sync::Mutex::new(repo(dir)))),
        audit: Some(Arc::new(
            hk_api::AuditLog::open(&dir.join("audit.jsonl")).unwrap(),
        )),
        ..hk_api::ApiState::default()
    };
    hk_api::Server::start(config, state).unwrap()
}

/// An authenticated POST; returns `(status, body)`. Mirrors [`api_get`].
fn api_post(addr: std::net::SocketAddr, path: &str, body: &str) -> (u16, Vec<u8>) {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .unwrap();
    write!(
        s,
        "POST {path} HTTP/1.1\r\nHost: test\r\nAuthorization: Bearer {API_TOKEN}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]);
    let status = head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap();
    (status, raw[split + 4..].to_vec())
}
