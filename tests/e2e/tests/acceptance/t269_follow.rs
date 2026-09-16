//! T-269 (C23): **following** a grant onto a channelizer output inside the dwell window, and
//! refusing — visibly — the grant that falls outside it. Blind, through the mock SDR, during a
//! normal pipeline run.
//!
//! T-268 stopped where a grant became a frequency. That left the frequency doing nothing: a run
//! could resolve every grant perfectly and still record no call, and no test could tell the
//! difference. This asks the next question — what did the receiver *do* about the grant? — and it
//! asks it the way T-287 and T-268 do: no `Ddc`, no raster, no decoder types anywhere in the test.
//! It starts a run with the **built-in** chain registry, nothing configured, and reads the
//! repository.
//!
//! # The two halves, and why one recording carries both
//!
//! A trunked system's voice channels routinely fall outside the ≤20 MHz a HackRF can hold at once
//! (C23 §Platform constraints). So the scene stages **both** outcomes on one control channel:
//!
//! 1. a grant onto 851.075 MHz, +62.5 kHz from the tuned centre and inside the window, carrying
//!    real keyings with silence between them — which must become a `CallRecord` whose boundaries
//!    were *measured*, not assumed;
//! 2. a grant onto 851.7375 MHz, +725 kHz and outside the 400 kHz usable window — which must be
//!    recorded as `outside-window`, **carrying the frequency it resolved to**, and must never be
//!    silently dropped. A dropped grant is indistinguishable from a system with no traffic, and
//!    that silence is exactly what teaches a person to trust a picture that is wrong.
//!
//! The scene picks both frequencies *first* and derives the channel numbers a grant has to carry
//! from the band plan, so the decoder still has to arrive at them through the IDEN_UP messages.
//! Truth is opened at the end, and only to check the answer.
//!
//! # M4 is metadata-only
//!
//! Nothing here attempts `CallAudio`. There is no such type in the workspace and no column that
//! could hold one (docs/07 §2.29); the run records nothing, and every call states
//! `Encryption::Unknown`, because T-270 owns the service-options bit and "nothing said" is never
//! "clear" (T-266).

use hk_core::Pacing;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::{Encryption, GrantKind, Timestamp};
use serde_json::json;

use crate::blind::{recording_start, replay_config, start};
use crate::common::*;

const T269: &str = "T-269";

/// Frequencies are compared at the precision the mapping arithmetic produces (T-268's rule): the
/// resolution is integer arithmetic on decoded fields, so a hertz is already generous.
const MAP_TOLERANCE_HZ: f64 = 1.0;

/// Tolerance on each call boundary against fixture truth, s. **Fixed a priori, before the scene
/// was run.**
///
/// A boundary is placed at the envelope frame that crosses the threshold, so the error is frame
/// quantisation and nothing else: one frame for the crossing, and one more where a keying begins
/// part-way through a frame and that frame reads occupied anyway. Two frames of 2 ms per edge,
/// doubled to four for margin: **8 ms**.
///
/// The channelizer contributes nothing measurable — `hk_dsp`'s channel time map already has the
/// filter's group delay removed, leaving under one input sample (2 µs at 500 kHz).
///
/// **Too tight** and a correct follower fails on quantisation alone. **Too loose** and a follower
/// that simply reported the whole buffered window as one call would pass — which is why the
/// scene's keying (0.10 s) is a fifth of the window the hunt buffers (0.5 s): a "whole window"
/// answer is 400 ms out, fifty times this tolerance, and cannot sneak through.
const BOUNDARY_TOL_S: f64 = 0.008;

#[test]
fn t269_a_normal_run_follows_a_grant_inside_the_window_and_logs_the_one_outside_it() {
    let out = synth_or_skip!(
        SynthRequest::new("trunk_tsbk_control_channel")
            .seed(269)
            .param("duration_s", 2.0)
    );
    let fx = out.fixture(0).unwrap();
    let t0 = recording_start(&fx);

    let dir = TempDir::new("t269");
    let (cfg, dev) = replay_config(&dir.0, &fx.meta_path, json!({}), Pacing::Unpaced);
    let summary = finish(start(cfg, dev));

    let count = |p: &str| summary.counter(p);
    let (follows, calls) = (count("/chains/cc_follows"), count("/chains/cc_calls"));
    let (closed, outside) = (
        count("/chains/cc_calls_closed"),
        count("/chains/cc_grants_outside_window"),
    );
    eprintln!(
        "[{T269}] follow during the run: {follows} channel(s) followed ({} refused by the cap, \
         {} silent, {} pass(es) with no noise reference), {calls} call(s) of which {closed} \
         closed on silence, {outside} grant(s) outside the window",
        count("/chains/cc_follow_refused"),
        count("/chains/cc_follow_silent"),
        count("/chains/cc_follow_no_reference"),
    );

    // ---- M4 is metadata-only. `CallAudio` is not attempted, and there is nothing on disk that
    // could be one.
    assert_eq!(
        count("/chains/recordings"),
        0,
        "[{T269}] M4 is metadata-only: nothing may be recorded"
    );
    for suffix in [".wav", ".sigmf-data"] {
        assert!(
            files_with_suffix(&dir.0, suffix).is_empty(),
            "[{T269}] a followed call wrote {suffix} content; M4 records metadata only"
        );
    }

    // ---- Truth, opened only now, and only to check the answer.
    let scenario = fx.scenario().unwrap();
    let t = &scenario.value["trunking"]["tsbk"];
    let want_follow = t["follow_target_hz"].as_f64().unwrap();
    let want_follow_tg = t["follow_talkgroup"].as_u64().unwrap().to_string();
    let want_outside = t["grant_target_hz"].as_f64().unwrap();
    let on_s = t["follow_on_s"].as_f64().unwrap();
    let fs = t["sample_rate_hz"].as_f64().unwrap();
    let usable_half = 0.4 * fs;
    let keyings: Vec<(f64, f64)> = t["follow_keyings_s"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| (k[0].as_f64().unwrap(), k[1].as_f64().unwrap()))
        .collect();

    // The scene actually stages both cases. Without this the run could "pass" against a fixture
    // that quietly stopped carrying one of them.
    assert!(
        t["follow_offset_hz"].as_f64().unwrap().abs() < usable_half,
        "[{T269}] the followed grant is not inside the window this run holds"
    );
    assert!(
        t["grant_target_offset_hz"].as_f64().unwrap().abs() > usable_half,
        "[{T269}] the other grant is not outside the window, so nothing tests the span limit"
    );
    assert!(
        !keyings.is_empty(),
        "[{T269}] nothing to follow in the scene"
    );

    let repo = repo(&dir.0);
    let rows = repo.trunk_systems().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "[{T269}] exactly one control channel on file"
    );
    let system = &rows[0];
    let grants = repo
        .grants_for_system(system.id, Timestamp::UNIX_EPOCH, 10_000)
        .unwrap();
    let call_rows = repo.calls_for_system(system.id, 1000).unwrap();
    assert!(
        !call_rows.is_empty(),
        "[{T269}] the run wrote no call at all"
    );

    // ---- Nothing read an encryption bit, so nothing claims one — on any row of either kind.
    assert!(
        call_rows
            .iter()
            .all(|c| c.encryption == Encryption::Unknown),
        "[{T269}] a call claimed an encryption state nothing had read"
    );
    assert!(
        call_rows.iter().all(|c| !c.encryption.is_clear()),
        "[{T269}] unknown is never clear"
    );
    assert!(
        grants.iter().all(|g| g.encryption == Encryption::Unknown),
        "[{T269}] a grant event claimed an encryption state nothing had read"
    );

    // ---- 1. The grant inside the window was FOLLOWED, and its boundaries were measured.
    assert!(
        follows >= 1,
        "[{T269}] no granted channel was ever followed"
    );
    let s = |ts: Timestamp| (ts.as_unix_nanos() - t0.as_unix_nanos()) as f64 * 1e-9;
    let followed: Vec<_> = call_rows
        .iter()
        .filter(|c| {
            c.f_hz
                .is_some_and(|f| (f - want_follow).abs() <= MAP_TOLERANCE_HZ)
        })
        .collect();
    assert!(
        !followed.is_empty(),
        "[{T269}] no call on the granted channel at {:.6} MHz; calls were {:?}",
        want_follow / 1e6,
        call_rows.iter().map(|c| c.f_hz).collect::<Vec<_>>()
    );
    assert!(
        followed
            .iter()
            .any(|c| c.talkgroup.as_deref() == Some(want_follow_tg.as_str())),
        "[{T269}] the followed call carries the wrong talkgroup"
    );

    // A call that ENDED: the silence after it was observed, not assumed at the window's edge.
    let ended: Vec<_> = followed.iter().filter(|c| c.t_end.is_some()).collect();
    assert!(
        !ended.is_empty() && closed >= 1,
        "[{T269}] every call on the followed channel is still open, so no end was ever measured \
         — the silence timeout never fired: {:?}",
        followed
            .iter()
            .map(|c| (s(c.t_start), &c.reasons))
            .collect::<Vec<_>>()
    );
    let matched: Vec<(f64, f64, f64, f64)> = ended
        .iter()
        .filter_map(|c| {
            let (a, b) = (s(c.t_start), s(c.t_end.unwrap()));
            keyings
                .iter()
                .find(|(x, y)| (a - x).abs() <= BOUNDARY_TOL_S && (b - y).abs() <= BOUNDARY_TOL_S)
                .map(|&(x, y)| (a, b, a - x, b - y))
        })
        .collect();
    for (a, b, da, db) in &matched {
        eprintln!(
            "[{T269}] call {a:.4}-{b:.4} s: start {:+.1} ms, end {:+.1} ms against truth \
             (tolerance +/-{:.0} ms)",
            da * 1e3,
            db * 1e3,
            BOUNDARY_TOL_S * 1e3
        );
    }
    assert!(
        !matched.is_empty(),
        "[{T269}] no ended call landed within {:.0} ms of a real keying. Measured {:?}; truth \
         {:?}",
        BOUNDARY_TOL_S * 1e3,
        ended
            .iter()
            .map(|c| (s(c.t_start), s(c.t_end.unwrap())))
            .collect::<Vec<_>>(),
        keyings
    );
    // The duration is right too, so a call cannot pass by being shifted whole. This is also what
    // stops "the entire buffered window" reading as one call: the window is 0.5 s and a keying is
    // 0.10 s.
    let (a, b, ..) = matched[0];
    assert!(
        ((b - a) - on_s).abs() <= 2.0 * BOUNDARY_TOL_S,
        "[{T269}] the call lasted {:.1} ms; the keying lasted {:.1} ms",
        (b - a) * 1e3,
        on_s * 1e3
    );
    assert!(
        ended
            .iter()
            .any(|c| c.reasons.iter().any(|r| r == "silence-timeout")),
        "[{T269}] a call ended without saying what ended it"
    );
    // The call is linked into the grant stream, so the record and the messages are one story.
    let started: Vec<_> = grants
        .iter()
        .filter(|g| g.kind == GrantKind::CallStart)
        .collect();
    assert!(
        !started.is_empty()
            && grants.iter().any(|g| g.kind == GrantKind::CallEnd)
            && started.iter().all(|g| g.call.is_some()),
        "[{T269}] the followed call has no call-start/call-end events linking it to its grants"
    );

    // ---- 2. The grant outside the window was LOGGED, with the frequency it resolved to.
    let refused: Vec<_> = grants
        .iter()
        .filter(|g| g.kind == GrantKind::OutsideWindow)
        .collect();
    assert!(
        outside >= 1 && !refused.is_empty(),
        "[{T269}] the grant onto {:.6} MHz — {:.0} kHz outside the {:.0} kHz window this run \
         holds — produced no outside-window row. A dropped grant is indistinguishable from a \
         system with no traffic (C23 span limit).",
        want_outside / 1e6,
        (want_outside - (want_outside - t["grant_target_offset_hz"].as_f64().unwrap())) / 1e3,
        2.0 * usable_half / 1e3
    );
    let row = refused
        .iter()
        .find(|g| {
            g.f_hz
                .is_some_and(|f| (f - want_outside).abs() <= MAP_TOLERANCE_HZ)
        })
        .unwrap_or_else(|| {
            panic!(
                "[{T269}] the outside-window row does not carry the frequency the grant resolved \
                 to ({:.6} MHz): {:?}",
                want_outside / 1e6,
                refused.iter().map(|g| g.f_hz).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        row.detail["reason"].as_str(),
        Some("grant-outside-window"),
        "[{T269}] the refusal does not say why: {}",
        row.detail
    );
    assert!(
        row.detail["beyond_usable_hz"].as_f64().unwrap_or(0.0) > 0.0,
        "[{T269}] the row does not say how far outside the window the grant fell: {}",
        row.detail
    );
    // ...and it reaches the call list, so a person reading "what happened on this system" sees the
    // traffic it could not observe instead of a gap.
    let unobserved: Vec<_> = call_rows
        .iter()
        .filter(|c| c.reasons.iter().any(|r| r == "grant-outside-window"))
        .collect();
    assert!(
        !unobserved.is_empty(),
        "[{T269}] the refused grant left no call row, so a call list shows nothing where the \
         traffic was"
    );
    assert!(
        unobserved.iter().all(|c| c.t_end.is_none()),
        "[{T269}] an unfollowable call claimed an end nobody observed"
    );
    assert!(
        unobserved.iter().all(|c| c
            .f_hz
            .is_some_and(|f| (f - want_outside).abs() <= MAP_TOLERANCE_HZ)),
        "[{T269}] the unobservable call does not say which frequency it was on"
    );

    // ---- 3. The two are not confused: nothing was followed on the out-of-window frequency, and
    // the followed one was never recorded as out of reach.
    assert!(
        !call_rows.iter().any(|c| c.t_end.is_some()
            && c.f_hz
                .is_some_and(|f| (f - want_outside).abs() <= MAP_TOLERANCE_HZ)),
        "[{T269}] a call outside the window was given measured boundaries the radio never saw"
    );
    assert!(
        !refused.iter().any(|g| g
            .f_hz
            .is_some_and(|f| (f - want_follow).abs() <= MAP_TOLERANCE_HZ)),
        "[{T269}] the channel inside the window was refused as outside it"
    );

    eprintln!(
        "[{T269}] followed {:.6} MHz (truth {:.6} MHz): call {:.4}-{:.4} s against a keying of \
         {:.0} ms; {:.6} MHz is {:.0} kHz outside the {:.0} kHz window and was logged as \
         grant-outside-window, not dropped",
        followed[0].f_hz.unwrap_or_default() / 1e6,
        want_follow / 1e6,
        a,
        b,
        on_s * 1e3,
        want_outside / 1e6,
        t["grant_target_offset_hz"].as_f64().unwrap() / 1e3,
        2.0 * usable_half / 1e3,
    );
}
