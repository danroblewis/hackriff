//! T-398: **how long** the inventory takes to decide, asserted blind.
//!
//! The user watched a strong, obvious FM broadcast station take tens of seconds to reach
//! Confirmed when a human reads it instantly, against the CLAUDE.md invariant that "a region's few
//! real signals should resolve quickly (target ~2–10 s)".
//!
//! The measurement, the budgets and the scenes live in [`crate::latency`] (T-401), which
//! generalised what started here: every latency in this file is a difference of two **capture**
//! timestamps, so it says when in the *signal* the system decided and is unaffected by how loaded
//! the host is. These two tests are the pair T-398 needed, now also bounding the detection and
//! family latencies that were previously unasserted on the same runs:
//!
//! - `t398_pilot_locked_wfm_confirms_within_seconds` — a stereo WFM station **with no RDS at all**
//!   ([`Degradation::NoRds`]). It carries a 19 kHz pilot and no decodable identity, so it can only
//!   confirm on the T-398 verified-emission route. This is the case that previously had no live
//!   route at all: a permanently-on emitter's "continuous and trusted" evidence is only weighed
//!   when its track *closes*, so without an identity to decode it stayed a candidate until the
//!   track idled out.
//! - `t398_wfm_without_pilot_does_not_take_the_fast_route` — the same station **mono**
//!   ([`Degradation::NoPilotNoRds`]), so equally strong but with no pilot to lock. It must not
//!   reach Confirmed by the verified route. That is what makes the fast path a short-circuit on
//!   positive evidence rather than a lowered threshold: remove the evidence and the fast path
//!   disappears, leaving the slow routes exactly as they were.

use hk_e2e::{Fixture, synth_or_skip};
use hk_model::EmitterId;

use crate::blind::{BlindSource, blind_replay, recording_start};
use crate::common::*;
use crate::latency::{
    CONFIRM_BUDGET_S, DETECT_BUDGET_S, Degradation, FAMILY_BUDGET_S, Latencies, Outcome, SCENE_S,
    matched_emitter, wfm_scene,
};

const T398: &str = "T-398";

/// The emitter whose measured extent matches the scene's single truth station, and the truth item.
fn matched(fx: &Fixture, rows: &[serde_json::Value]) -> (EmitterId, f64) {
    matched_emitter(T398, fx, "wfm-broadcast", rows)
}

#[test]
fn t398_pilot_locked_wfm_confirms_within_seconds() {
    let out = synth_or_skip!(wfm_scene(3981, Degradation::NoRds));
    let fx = out.fixture(0).unwrap();
    let t0 = recording_start(&fx);
    let run = blind_replay(&out.recordings[0], "t398-pilot", BlindSource::default());
    let (id, bw) = matched(&fx, &run.api_rows);
    let l = Latencies::measure(&repo(&run.dir.0), id, t0);
    eprintln!("[{T398}] pilot-locked WFM ({:.0} kHz wide): {l}", bw / 1e3);

    // A stereo WFM station with a 19 kHz pilot must be found, named and confirmed within seconds
    // of appearing, not when its track finally closes at the end of the scene.
    l.require(T398, Outcome::FirstDetection, DETECT_BUDGET_S);
    l.require(T398, Outcome::Family, FAMILY_BUDGET_S);
    l.require(T398, Outcome::Confirmed, CONFIRM_BUDGET_S);
    // The fast route is the one that ran: no RDS exists in this scene, so an identity-route
    // confirmation would mean something decoded an identity that is not on the air.
    let reason = l.confirmed_reason.as_deref().unwrap_or_default();
    assert!(
        reason.contains("verified"),
        "[{T398}] with no RDS on air the only available route is the verified emission; got: \
         {reason}"
    );
}

#[test]
fn t398_wfm_without_pilot_does_not_take_the_fast_route() {
    let out = synth_or_skip!(wfm_scene(3982, Degradation::NoPilotNoRds));
    let fx = out.fixture(0).unwrap();
    let t0 = recording_start(&fx);
    let run = blind_replay(&out.recordings[0], "t398-nopilot", BlindSource::default());
    let (id, bw) = matched(&fx, &run.api_rows);
    let r = repo(&run.dir.0);
    let l = Latencies::measure(&r, id, t0);
    eprintln!(
        "[{T398}] mono WFM, no pilot ({:.0} kHz wide): {l}",
        bw / 1e3
    );

    // Detection and family are bounded here too: losing the pilot costs the fast confirmation
    // route, not the ability to see the station or name it.
    l.require(T398, Outcome::FirstDetection, DETECT_BUDGET_S);
    l.require(T398, Outcome::Family, FAMILY_BUDGET_S);

    // Time-to-Confirmed is **reported, not bounded**, for this variant only, and this is the one
    // place in the suite where that is deliberate. T-398 short-circuited the *pilot-locked* case;
    // the continuous-and-trusted route it left alone is still weighed only when a track closes, so
    // a station with no pilot and no identity confirms at the end of the recording however long
    // the recording is. Bounding it here would make this test red for a defect this ticket does
    // not fix, and loosening the bound to fit would enshrine it. Printing it keeps the number in
    // the log of every run, which is exactly what the suite lacked before T-401.
    if let Some(confirmed) = l.confirmed_s {
        eprintln!(
            "[{T398}] REPORTED, NOT ASSERTED: no-pilot no-identity WFM confirmed at {confirmed:.2} \
             s of capture in a {SCENE_S:.0} s scene — the continuous-and-trusted route is still \
             close-only, so this number tracks the recording length rather than the evidence."
        );
    }

    // It may still confirm — a continuous carrier is allowed to earn it the slow way — but never
    // on evidence it does not carry. Nothing may be confirmed as a family the measurement did not
    // support (the exploration-first rule).
    if let Some(reason) = l.confirmed_reason.as_deref() {
        assert!(
            !reason.contains("verified"),
            "[{T398}] a mono WFM station has no 19 kHz pilot to lock, so the verified-emission \
             route must not fire on it; got: {reason}"
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
