//! T-398: **how long** the inventory takes to decide, asserted blind.
//!
//! The user watched a strong, obvious FM broadcast station take tens of seconds to reach
//! Confirmed when a human reads it instantly, against the CLAUDE.md invariant that "a region's few
//! real signals should resolve quickly (target ~2–10 s)".
//!
//! Every other acceptance test in this suite asserts **terminal state**: it replays a whole
//! fixture and then inspects the final inventory. A 40 s time-to-confirm and a 1 s time-to-confirm
//! leave an identical inventory at the end of a 60 s recording, so the suite was blind to latency
//! by construction. These tests assert the *interval* instead, reading it from the emitter's own
//! lifecycle history against its first sighting — both capture timestamps, so the assertion is
//! independent of how fast the host replays.
//!
//! Blind throughout: nothing is looked up and tuned to. The scene is generated at an offset the
//! run never sees, the pipeline detects whatever it detects, and the emitter is matched to the
//! private truth **after** the run by measured centre and bandwidth (`matching`).
//!
//! The two cases are a matched pair, and the second is the one that keeps the first honest:
//!
//! - `t398_pilot_locked_wfm_confirms_within_seconds` — a stereo WFM station **with no RDS at all**
//!   (`rds_deviation_hz = 0`). It carries a 19 kHz pilot and no decodable identity, so it can only
//!   confirm on the T-398 verified-emission route. This is the case that previously had no live
//!   route at all: a permanently-on emitter's "continuous and trusted" evidence is only weighed
//!   when its track *closes*, so without an identity to decode it stayed a candidate until the
//!   track idled out.
//! - `t398_wfm_without_pilot_does_not_take_the_fast_route` — the same station **mono**
//!   (`stereo = false`), so 200 kHz wide and equally strong but with no pilot to lock. It must not
//!   reach Confirmed by the verified route. That is what makes the fast path a short-circuit on
//!   positive evidence rather than a lowered threshold: remove the evidence and the fast path
//!   disappears, leaving the slow routes exactly as they were.

use hk_e2e::blind::matching;
use hk_e2e::{Fixture, SynthRequest, synth_or_skip};
use hk_model::sigmf::Datatype;
use hk_model::{EmitterId, LifecycleState};

use crate::blind::{BlindSource, blind_replay, center_tol_hz};
use crate::common::*;

const T398: &str = "T-398";

/// Longest a strong, unambiguous emission may take to reach Confirmed, s, measured from its first
/// sighting. The CLAUDE.md invariant says a region's few real signals resolve in ~2–10 s; this
/// takes the fast half of that, because the evidence here (a locked pilot inside a WFM-width
/// emission) is available about a second in.
const CONFIRM_BUDGET_S: f64 = 5.0;

/// Seconds of scene. Long enough that a run which *fails* the budget still finishes and reports a
/// real number rather than timing out, and long enough for the old behaviour (wait for the track
/// to close) to be visibly outside it.
const SCENE_S: f64 = 14.0;

/// The station's blind offset from the tuned centre, Hz. The run never sees it.
const OFFSET_HZ: f64 = 500e3;

/// A stereo/mono WFM scene with RDS suppressed, so the only identity evidence available is what
/// the pipeline measures off the air.
fn wfm_scene(seed: u64, stereo: bool) -> SynthRequest {
    SynthRequest::new("fm_broadcast_rds")
        .seed(seed)
        .datatype(Datatype::Cf32Le)
        .param("sample_rate", 2.4e6)
        .param("center_hz", 100.8e6)
        .param("offset_hz", OFFSET_HZ)
        .param("duration_s", SCENE_S)
        .param("power_dbfs", -16.0)
        .param("noise_dbfs", -60.0)
        .param("stereo", stereo)
        // No RDS subcarrier: nothing to decode, so no identity route to confirm on.
        .param("rds_deviation_hz", 0.0)
}

/// The emitter whose measured extent matches the scene's single truth station, and the truth item.
fn matched_emitter(fx: &Fixture, rows: &[serde_json::Value]) -> (EmitterId, f64) {
    let stations = fx.of_kind("wfm-broadcast");
    assert_eq!(stations.len(), 1, "[{T398}] one truth station in the scene");
    let truth = stations[0];
    let matched = matching(
        truth,
        0.0,
        rows,
        |r| {
            (
                r["f_center_hz"].as_f64().unwrap_or(f64::NAN),
                r["bandwidth_hz"].as_f64().unwrap_or(0.0),
            )
        },
        center_tol_hz(truth),
    );
    assert!(
        !matched.is_empty(),
        "[{T398}] the station was detected blind: truth {:.4}..{:.4} MHz, rows {:?}",
        truth.f_lo_hz / 1e6,
        truth.f_hi_hz / 1e6,
        rows.iter()
            .map(|r| (r["f_center_hz"].clone(), r["state"].clone()))
            .collect::<Vec<_>>()
    );
    // The widest match is the station itself rather than a skirt fragment inside it.
    let row = matched
        .iter()
        .max_by(|a, b| {
            a["bandwidth_hz"]
                .as_f64()
                .unwrap_or(0.0)
                .total_cmp(&b["bandwidth_hz"].as_f64().unwrap_or(0.0))
        })
        .unwrap();
    (
        row["id"].as_str().unwrap().parse().unwrap(),
        row["bandwidth_hz"].as_f64().unwrap_or(0.0),
    )
}

#[test]
fn t398_pilot_locked_wfm_confirms_within_seconds() {
    let out = synth_or_skip!(wfm_scene(3981, true));
    let fx = out.fixture(0).unwrap();
    let run = blind_replay(&out.recordings[0], "t398-pilot", BlindSource::default());
    let (id, bw) = matched_emitter(&fx, &run.api_rows);
    let r = repo(&run.dir.0);

    let emitter = r.emitter(id).unwrap();
    let history = r.emitter_lifecycle_history(id).unwrap();
    let confirmed = history
        .iter()
        .find(|c| c.state == LifecycleState::Confirmed);

    let elapsed =
        confirmed.map(|c| (c.t.as_unix_nanos() - emitter.first_seen.as_unix_nanos()) as f64 / 1e9);
    eprintln!(
        "[{T398}] pilot-locked WFM ({:.0} kHz wide): confirmed after {:?} s — {:?}",
        bw / 1e3,
        elapsed,
        confirmed.map(|c| c.reason.as_str())
    );

    let c = confirmed.unwrap_or_else(|| {
        panic!(
            "[{T398}] a stereo WFM station with a 19 kHz pilot must reach Confirmed; it stayed a \
             candidate for the whole {SCENE_S} s scene. Lifecycle history: {history:?}"
        )
    });
    let elapsed = elapsed.unwrap();
    assert!(
        elapsed <= CONFIRM_BUDGET_S,
        "[{T398}] time-to-confirm {elapsed:.2} s exceeds the {CONFIRM_BUDGET_S} s budget \
         (CLAUDE.md: a region's few real signals resolve in ~2–10 s). Reason: {}",
        c.reason
    );
    assert!(
        elapsed >= 0.0,
        "[{T398}] confirmation cannot precede the first sighting (got {elapsed:.2} s)"
    );
    // The fast route is the one that ran: no RDS exists in this scene, so an identity-route
    // confirmation would mean something decoded an identity that is not on the air.
    assert!(
        c.reason.contains("verified"),
        "[{T398}] with no RDS on air the only available route is the verified emission; got: {}",
        c.reason
    );
}

#[test]
fn t398_wfm_without_pilot_does_not_take_the_fast_route() {
    let out = synth_or_skip!(wfm_scene(3982, false));
    let fx = out.fixture(0).unwrap();
    let run = blind_replay(&out.recordings[0], "t398-nopilot", BlindSource::default());
    let (id, bw) = matched_emitter(&fx, &run.api_rows);
    let r = repo(&run.dir.0);

    let history = r.emitter_lifecycle_history(id).unwrap();
    let confirmed = history
        .iter()
        .find(|c| c.state == LifecycleState::Confirmed);
    eprintln!(
        "[{T398}] mono WFM, no pilot ({:.0} kHz wide): {:?}",
        bw / 1e3,
        confirmed.map(|c| c.reason.as_str())
    );

    // It may still confirm — a continuous carrier is allowed to earn it the slow way — but never
    // on evidence it does not carry. Nothing may be confirmed as a family the measurement did not
    // support (the exploration-first rule).
    if let Some(c) = confirmed {
        assert!(
            !c.reason.contains("verified"),
            "[{T398}] a mono WFM station has no 19 kHz pilot to lock, so the verified-emission \
             route must not fire on it; got: {}",
            c.reason
        );
    }

    // Refusing to confirm is not the same as discarding the evidence. What the demodulator
    // measured is recorded either way — mode, bandwidth, and the absent lock, with the window it
    // measured them over — so short evidence leaves a log rather than a promotion, and the
    // latency question stays answerable for a signal that never takes the fast route.
    let demod = r
        .latest_linked_demodulation_for_emitter(id)
        .unwrap()
        .unwrap_or_else(|| {
            panic!("[{T398}] the measurement that declined to confirm is still recorded")
        });
    eprintln!(
        "[{T398}] logged instead of confirmed early: mode {}, bandwidth {:?}, lock_quality {:?}, \
         pilot {:?}, over {:?}..{:?}",
        demod.mode,
        demod.params.bandwidth_hz,
        demod.lock_quality,
        demod.params.pilot_hz,
        demod.time.start,
        demod.time.end
    );
    assert!(
        demod.params.pilot_hz.is_none(),
        "[{T398}] a mono station has no pilot to report; got {:?}",
        demod.params.pilot_hz
    );
}
