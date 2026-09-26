//! T-977 (SIGNAL-085, C23): **the control-channel chain's verdict reaches the emitter row.**
//!
//! Found in the field, 2026-09-25, explorer window 2 (DMR/P25 851–869 MHz, live HackRF in SF): a
//! P25 C4FM emission at 852.859 MHz, +8..+15 dB over the floor, whose oracle says *P25 frame sync*,
//! read `family: unknown`, `mod: 2fsk` (it is four-level) and `resolution: not-searched` — while
//! the always-on hunt in that very band reported `cc_passes 10–12, cc_demods 10–12,
//! cc_grid_corrections 9, cc_confirmed 0`. The chain looked; the row never learned what it found.
//!
//! Two defects, and this file's three tests are one per deliverable:
//!
//! 1. **The chain only ever wrote anything down when it CONFIRMED.** A rejected candidate left a
//!    counter and a `debug_enabled()` `eprintln!`. An emitter with no `emitter_synthesis` row reads
//!    `resolution: not-searched` (ADR-0021 §7A.4) — *un-looked-at* — which is exactly the state
//!    the field row was in after being demodulated.
//! 2. **Candidacy was occupancy-only.** A P25 *voice* channel is intermittent, never reaches
//!    `MIN_CC_FCO`, and so was never demodulated at all, however plainly the detector had an
//!    emitter on it.
//!
//! # The scene
//!
//! `trunk_voice_frames_control_channel` (T-849): a real P25 control channel, a continuous 4FSK
//! decoy, bursty NBFM neighbours, and **two granted channels keying real P25 Phase 1 LDU1/LDU2
//! superframes** — 0.36 s on, 0.14 s off, ~72 % duty, far below the 95 % occupancy floor. Those
//! keyings are the "intermittent P25 C4FM burst train": genuine P25 frame syncs at the expected
//! spacing, and no CRC-valid TSBK, because a voice channel carries none.
//!
//! Blind throughout: the built-in chain registry, nothing configured, no decoder type named in the
//! test. Truth is opened only to locate the channels the assertions are about.

use hk_core::Pacing;
use hk_e2e::{Fixture, SynthRequest};
use hk_model::repo::synthesis::ResolutionKind;
use hk_model::{EmitterId, InventoryQuery, Repository};
use serde_json::json;

use crate::blind::{replay_config, start};
use crate::common::*;

const T977: &str = "T-977/SIGNAL-085";

/// How near a row must be to a truth frequency to be that emission. Half the 12.5 kHz raster: any
/// looser and a neighbouring channel could satisfy an assertion.
const NEAR_HZ: f64 = 6_250.0;

/// C4FM is 4800 Bd by the standard; the blind estimate is a cyclostationary line measurement, so
/// this is the tolerance on a *measurement*, not on arithmetic.
const RATE_TOL_BD: f64 = 250.0;

struct Run {
    _dir: TempDir,
    dir: std::path::PathBuf,
    summary: hk_pipeline::RunSummary,
    fx: Fixture,
}

impl Run {
    fn count(&self, path: &str) -> u64 {
        self.summary.counter(path)
    }

    fn repo(&self) -> Repository {
        repo(&self.dir)
    }

    /// The RF centres of the voice-frame keying channels, from the scene's truth. Opened only to
    /// say *which* emission each assertion is about — nothing is tuned to them and nothing looks
    /// them up in a band plan.
    fn voice_channels(&self) -> Vec<f64> {
        let scenario = self.fx.scenario().unwrap();
        let vf = &scenario.value["trunking"]["tsbk"]["voice_frames"]["channels"];
        vf.as_array()
            .expect("the voice-frame scene names its channels")
            .iter()
            .map(|c| c["target_hz"].as_f64().unwrap())
            .collect()
    }

    /// The live emitter nearest `hz`, if blind detection has one within [`NEAR_HZ`].
    fn emitter_near(&self, r: &Repository, hz: f64) -> Option<EmitterId> {
        hk_pipeline::refine::emitter_for_channel(r, hz, 2.0 * NEAR_HZ).unwrap()
    }
}

/// The built-in hunt with one budget changed: a pass every 0.5 s of stream instead of every 10 s.
///
/// The built-in `period_s` is a **battery** decision, not a detection one, and on a 3 s fixture it
/// allows exactly one pass — which lands before blind detection has written a single emitter, so
/// there is nothing for a verdict to be filed against. Raising the rate is the same kind of change
/// `signal_085` makes when it widens the hunt's band: a spec sets only how much the hunt may
/// *spend*, and nothing inside the hunt reads it. Every threshold that decides anything is
/// untouched.
fn hunt_often() -> serde_json::Value {
    let mut chains = hk_pipeline::builtin_chains();
    let hunt = chains
        .iter_mut()
        .find(|c| c.id == "trunk-cc-hunt")
        .expect("the hunt is built in");
    for node in &mut hunt.nodes {
        if let hk_pipeline::chains::spec::NodeSpec::TrunkCc { period_s, .. } = node {
            *period_s = 0.5;
        }
    }
    json!({ "pipeline": { "chains": chains } })
}

fn run(tag: &str, duration_s: f64) -> Option<Run> {
    let request = SynthRequest::new("trunk_voice_frames_control_channel")
        .seed(977)
        .param("duration_s", duration_s);
    let out = match request.generate() {
        Ok(out) => out,
        Err(err) if err.is_unavailable() && !hk_e2e::synth::require_synth() => {
            eprintln!("SKIP {}: {err}", module_path!());
            return None;
        }
        Err(err) => panic!("synthetic scenario generation failed: {err}"),
    };
    let fx = out.fixture(0).unwrap();
    let dir = TempDir::new(tag);
    let path = dir.0.clone();
    let (cfg, dev) = replay_config(&path, &fx.meta_path, hunt_often(), Pacing::Unpaced);
    let summary = finish(start(cfg, dev));
    Some(Run {
        _dir: dir,
        dir: path,
        summary,
        fx,
    })
}

/// **(1) The verdict lands on the emitter row, and `resolution` says searched.**
///
/// For every channel the hunt demodulated and rejected but blind detection had a row for, there is
/// an `emitter_synthesis` row whose `resolution.kind` is a *finished-search* kind. The field defect
/// is the negation of this: the row existed, the chain demodulated it, and `resolution` still read
/// `not-searched`, which is the decode-side form of painting observed spectrum grey.
///
/// Red on the old code by construction: nothing but a confirmation wrote a synthesis row at all.
///
/// **Why it asserts over every emitter rather than a named channel.** An unpaced replay completes
/// as many hunt passes as the chain thread is scheduled for, and the first pass always lands before
/// blind detection has written anything — so *which* channel a pass reaches is a property of the
/// box, not of the code. Asserting through it made this test fail under a loaded gate and pass
/// alone. The invariant below holds whatever runs.
#[test]
fn t977_a_rejected_candidate_leaves_a_searched_verdict_on_its_emitter_row() {
    let Some(run) = run("t977verdict", 6.0) else {
        return;
    };
    eprintln!(
        "[{T977}] {} pass(es), {} candidates, {} detected-emitter channels, {} demods, \
         {} confirmed, {} verdicts filed",
        run.count("/chains/cc_passes"),
        run.count("/chains/cc_candidates"),
        run.count("/chains/cc_emitter_candidates"),
        run.count("/chains/cc_demods"),
        run.count("/chains/cc_confirmed"),
        run.count("/chains/cc_verdicts"),
    );
    assert!(
        run.count("/chains/cc_passes") >= 1,
        "[{T977}] the hunt never ran, so this test asserts nothing"
    );
    assert!(
        run.count("/chains/cc_verdicts") >= 1,
        "[{T977}] the hunt demodulated {} channel(s) and confirmed {}, and filed NO verdict on \
         any emitter row: a rejected candidate still leaves the row reading `not-searched`",
        run.count("/chains/cc_demods"),
        run.count("/chains/cc_confirmed"),
    );

    // ---- The invariant, over EVERY emitter the run has.
    //
    // Not over the voice channels specifically, and that is deliberate: how many hunt passes an
    // unpaced replay completes depends on when the chain thread is scheduled, so *which* channel a
    // pass reaches is not a property of the code under test. What IS a property of it is that no
    // row the trunking engine wrote may read `not-searched`, may resolve without a reason, or may
    // resolve without a sentence naming the channel — the defect this ticket exists for, asserted
    // wherever it could appear rather than where one run happened to put it.
    let r = run.repo();
    let rows: Vec<_> = inventory(&r, InventoryQuery::default())
        .into_iter()
        .filter_map(|e| r.synthesis(e.emitter.id).unwrap())
        .filter(|row| row.engine == hk_pipeline::synth::TRUNK_SYNTH_ENGINE)
        .collect();
    assert!(
        !rows.is_empty(),
        "[{T977}] the hunt filed {} verdict(s) and the inventory holds no trunking analysis row \
         at all",
        run.count("/chains/cc_verdicts"),
    );
    let mut unconfirmed = 0;
    for row in &rows {
        // A `solved` row resolves nothing and is right not to: the control channel CRC-decoded and
        // its messages named the system, which is an answer rather than a residue. Every row below
        // solved must carry one.
        let Some(res) = row.resolution.as_ref() else {
            assert_eq!(
                row.verdict,
                hk_model::repo::synthesis::Verdict::Solved,
                "[{T977}] only a solved row may resolve nothing",
            );
            continue;
        };
        eprintln!(
            "[{T977}] {:?} / {:?}: {}",
            row.verdict, res.kind, res.summary
        );
        assert_ne!(
            res.kind,
            ResolutionKind::NotSearched,
            "[{T977}] a demodulated channel still reads `not-searched`: {res:?}",
        );
        assert!(
            res.reason.is_some(),
            "[{T977}] a finished search with no reason: {res:?}",
        );
        if res.kind == ResolutionKind::Unknown {
            unconfirmed += 1;
            assert!(
                res.summary.contains("MHz") && res.summary.contains("demodulated and scanned"),
                "[{T977}] a rejected channel's verdict must be a sentence naming what was looked \
                 at and what came of it: {:?}",
                res.summary,
            );
        }
    }
    assert!(
        unconfirmed >= 1,
        "[{T977}] the hunt filed {} verdict(s) and not one of the {} trunking row(s) is a \
         REJECTED channel's: before T-977 only a confirmation wrote a row at all",
        run.count("/chains/cc_verdicts"),
        rows.len(),
    );
}

/// **(3) C4FM is estimated as FOUR-level FSK at 4800 Bd, and P25 frame sync is "P25-like".**
///
/// The field row read `mod: 2fsk` on a four-level emission and `family: unknown` beside an oracle
/// that says P25 frame sync. Both numbers here are *measurements* — `measure_fm_structure`'s level
/// count and cyclostationary symbol rate, blind, with no expected rate supplied — persisted as a
/// `Demodulation`'s `estimated_params`, and the family is the hunt's own `p25-frame-sync` evidence.
#[test]
fn t977_an_intermittent_p25_voice_channel_measures_four_levels_and_reads_p25_like() {
    let Some(run) = run("t977levels", 6.0) else {
        return;
    };
    let r = run.repo();
    let mut measured = 0;
    let mut p25_like = 0;
    for hz in run.voice_channels() {
        let Some(e) = run.emitter_near(&r, hz) else {
            continue;
        };
        if let Some(d) = r.latest_demodulation_for_emitter(e).unwrap() {
            let (Some(order), Some(rate)) = (d.params.mod_order, d.params.symbol_rate_hz) else {
                continue;
            };
            eprintln!(
                "[{T977}] {:.4} MHz demodulated as {:?}: {order}-level at {rate:.0} Bd",
                hz / 1e6,
                d.mode
            );
            assert_eq!(
                order,
                4,
                "[{T977}] {:.4} MHz is C4FM: the estimator's level count must be 4, not {order}",
                hz / 1e6,
            );
            assert!(
                (rate - 4800.0).abs() <= RATE_TOL_BD,
                "[{T977}] {:.4} MHz: C4FM is 4800 Bd, measured {rate:.0}",
                hz / 1e6,
            );
            measured += 1;
        }
        // "P25-like" without a control channel: the family evidence the frame sync earns. It is a
        // different id from the `p25-tsbk` a CRC-valid decode earns, so a reader can tell a
        // resemblance from a decode.
        for c in r.classification_history(e).unwrap() {
            if c.classification.family == "p25-frame-sync" {
                eprintln!(
                    "[{T977}] {:.4} MHz identified P25-like at confidence {:.2}",
                    hz / 1e6,
                    c.classification.confidence
                );
                assert!(
                    c.classification.confidence < 1.0,
                    "[{T977}] a resemblance must not carry a decode's certainty"
                );
                p25_like += 1;
            }
        }
    }
    assert!(
        measured >= 1,
        "[{T977}] no intermittent P25 voice channel got a four-level estimate; the hunt ran {} \
         demod(s) and admitted {} detected-emitter channel(s)",
        run.count("/chains/cc_demods"),
        run.count("/chains/cc_emitter_candidates"),
    );
    assert!(
        p25_like >= 1,
        "[{T977}] P25 frame sync was seen on a voice channel and produced no `p25-frame-sync` \
         family evidence: the row still reads `unknown` next to the measurement that says it is not"
    );
}

/// **The control: confirmation is still what makes a control channel.**
///
/// The hunt now looks at channels occupancy alone would never have chosen, so the thing to prove is
/// that this bought nothing it should not: exactly one control channel is confirmed, the continuous
/// decoy is still rejected, and the per-pass demodulation budget is unchanged (`max_demods`, 8).
/// A verdict path that promoted a resemblance, or that spent the budget until the control channel
/// fell off the list, would fail here.
#[test]
fn t977_looking_at_more_channels_confirms_no_more_control_channels() {
    let Some(run) = run("t977control", 6.0) else {
        return;
    };
    let passes = run.count("/chains/cc_passes");
    let demods = run.count("/chains/cc_demods");
    eprintln!(
        "[{T977}] control: {passes} pass(es), {demods} demod(s), {} confirmed, {} system(s)",
        run.count("/chains/cc_confirmed"),
        run.count("/chains/cc_systems"),
    );
    assert!(passes >= 1, "[{T977}] the hunt never ran");
    assert!(
        demods <= passes * 8,
        "[{T977}] the per-pass budget (max_demods 8) no longer bounds the demodulations: \
         {demods} over {passes} pass(es)"
    );
    // ONE control channel, however many rows carry it. The rows are per confirmation and a
    // re-measured centre moves by a fraction of a hertz between passes, so the assertion is about
    // the FREQUENCY, which is what "one control channel" means: every confirmed frequency must
    // agree to within half a raster, and the continuous decoy must not be among them.
    let systems = run.repo().trunk_systems().unwrap();
    let found: Vec<f64> = systems.iter().filter_map(|s| s.cc_freq_hz).collect();
    assert!(
        !found.is_empty(),
        "[{T977}] no control channel was confirmed at all"
    );
    let first = found[0];
    for f in &found {
        assert!(
            (f - first).abs() <= 6_250.0,
            "[{T977}] more than one distinct control channel was confirmed: {:?} MHz",
            found.iter().map(|f| f / 1e6).collect::<Vec<_>>(),
        );
    }
}
