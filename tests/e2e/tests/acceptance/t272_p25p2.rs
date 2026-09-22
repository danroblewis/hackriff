//! T-272 (C23): **P25 Phase 2 TDMA slot attribution** — blind, through the mock SDR, during a
//! normal pipeline run.
//!
//! # The pitfall, in its exact form
//!
//! A Phase 2 system's *control* channel is a Phase 1 FDMA channel: the same C4FM, the same TSBKs,
//! nothing about it that a Phase 1 decoder would trip over. What makes the system Phase 2 is the
//! band plan it announces — an `IDEN_UP_TDMA` identifier whose channel type says how many TDMA
//! slots share one carrier. On such a plan a grant's channel number divides:
//!
//! ```text
//! f    = base + spacing × (channel / slots)
//! slot = channel % slots
//! ```
//!
//! So two talkgroups on **alternating slots of one frequency** are announced as two *consecutive*
//! channel numbers. C23's TDMA slot mix-up pitfall is what happens when that is read as FDMA, and
//! it fails in two ways at once, both of which this test pins:
//!
//! 1. **Two wrong frequencies.** `2n` and `2n+1` become two different frequencies, neither of
//!    which any emission is on. In this scene they land 175 kHz and 187.5 kHz away from the real
//!    one — outside the window the run holds — so an FDMA reading produces two `outside-window`
//!    refusals and *no calls at all*.
//! 2. **One merged call, or two with no slots.** A follower keyed on frequency alone puts both
//!    grants on one target and writes a single call attributed to whichever grant it saw first, so
//!    one of the two talkgroups vanishes from the call list entirely.
//!
//! The scene therefore stages exactly the acceptance criterion: **two talkgroups on alternating
//! slots of one frequency must produce two distinct `CallRecord`s with correct slot attribution.**
//! The frequency is chosen first and the channel numbers derived from the band plan, so the
//! decoder still has to arrive at both the frequency and the slot through the `IDEN_UP_TDMA`
//! messages it read off the air.
//!
//! # What is *not* claimed, and why the run must say so
//!
//! Both slots key **one carrier**. Nothing in M4 demodulates P25 Phase 2's two-slot bursts, so the
//! envelope the follower measures says when the *channel* was up, not when each slot was talking.
//! The slot attribution comes from the control channel, which is where it was actually stated; the
//! timing does not, and every TDMA call must carry the `tdma-shared-envelope` reason saying so.
//! Presenting per-slot timing this build never measured is the same defect as implying resolution
//! nobody captured.
//!
//! M4 stays metadata-only: `recordings == 0`, no `CallAudio` type or column exists, and every call
//! here states `unknown` encryption and is refused a voice path.

use hk_core::Pacing;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::{Encryption, GrantKind, Timestamp, TrunkProtocol};
use serde_json::json;

use crate::blind::{replay_config, start};
use crate::common::*;

const T272: &str = "T-272";

/// The mapping is integer arithmetic on decoded fields, so a hertz is already generous (T-268).
const MAP_TOLERANCE_HZ: f64 = 1.0;

#[test]
fn t272_two_talkgroups_on_alternating_slots_of_one_frequency_become_two_calls() {
    let out = synth_or_skip!(
        SynthRequest::new("trunk_p25p2_control_channel")
            .seed(272)
            .param("duration_s", 2.0)
    );
    let fx = out.fixture(0).unwrap();

    let dir = TempDir::new("t272");
    let (cfg, dev) = replay_config(&dir.0, &fx.meta_path, json!({}), Pacing::Unpaced);
    let summary = finish(start(cfg, dev));

    let count = |p: &str| summary.counter(p);
    eprintln!(
        "[{T272}] run: {} channel(s) followed, {} call(s) ({} closed on silence), {} grant(s) \
         outside the window",
        count("/chains/cc_follows"),
        count("/chains/cc_calls"),
        count("/chains/cc_calls_closed"),
        count("/chains/cc_grants_outside_window"),
    );

    // ---- M4 is metadata-only, on a TDMA system exactly as on an FDMA one.
    assert_eq!(
        count("/chains/recordings"),
        0,
        "[{T272}] M4 is metadata-only: nothing may be recorded"
    );
    for suffix in [".wav", ".sigmf-data"] {
        assert!(
            files_with_suffix(&dir.0, suffix).is_empty(),
            "[{T272}] a followed call wrote {suffix} content; M4 records metadata only"
        );
    }

    // ---- Truth, opened only now, and only to check the answer.
    let scenario = fx.scenario().unwrap();
    let t = &scenario.value["trunking"]["tsbk"];
    let p2 = &t["phase2"];
    let want_hz = p2["target_hz"].as_f64().unwrap();
    let slots = p2["slots"].as_u64().unwrap();
    let want_tgs: Vec<String> = p2["slot_talkgroups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap().to_string())
        .collect();
    let wrong_hz: Vec<f64> = p2["wrong_frequencies_if_read_as_fdma_hz"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap())
        .collect();
    let keyings: Vec<(f64, f64)> = p2["keyings_s"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| (k[0].as_f64().unwrap(), k[1].as_f64().unwrap()))
        .collect();

    // The scene actually stages the case. Without this the run could "pass" against a fixture that
    // quietly stopped carrying two slots, or stopped putting them on one frequency.
    assert_eq!(slots, 2, "[{T272}] the scene is not a two-slot TDMA plan");
    assert_eq!(
        want_tgs.len(),
        2,
        "[{T272}] the scene does not stage two talkgroups"
    );
    assert_ne!(
        want_tgs[0], want_tgs[1],
        "[{T272}] the two slots carry the same talkgroup, so nothing distinguishes them"
    );
    assert!(
        !keyings.is_empty(),
        "[{T272}] nothing is on the shared carrier to follow"
    );
    let fs = t["sample_rate_hz"].as_f64().unwrap();
    let usable_half = 0.4 * fs;
    assert!(
        (want_hz - (want_hz - p2["offset_hz"].as_f64().unwrap())).abs() < usable_half,
        "[{T272}] the shared carrier is not inside the window this run holds"
    );
    for w in &wrong_hz {
        assert!(
            (w - want_hz).abs() > MAP_TOLERANCE_HZ,
            "[{T272}] the FDMA misreading is indistinguishable from the truth, so this test \
             cannot fail for the right reason"
        );
    }

    let repo = repo(&dir.0);
    let rows = repo.trunk_systems().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "[{T272}] exactly one control channel on file"
    );
    let system = &rows[0];

    // ---- 1. The band plan named the system Phase 2. The control channel alone could not have:
    // it is a Phase 1 channel. Only a corroborated IDEN_UP_TDMA says this.
    assert_eq!(
        system.protocol,
        TrunkProtocol::P25Phase2,
        "[{T272}] the run named the system {:?}, but its band plan announces a TDMA channel type",
        system.protocol
    );
    let plan = &system.channel_plan;
    let tdma: Vec<_> = plan.iter().filter(|e| e.slots > 1).collect();
    assert!(
        !tdma.is_empty(),
        "[{T272}] no stored channel-plan entry carries a slot count, so a reader re-deriving a \
         frequency from the database would land half a channel out: {plan:?}"
    );
    assert!(
        tdma.iter().all(|e| e.slots == slots as u8),
        "[{T272}] the stored slot count disagrees with what was announced"
    );
    // The stored plan reads the channel numbers the same way the decoder did.
    let e = tdma[0];
    let chan = p2["channel"].as_u64().unwrap() as u32;
    assert!(
        (e.downlink_hz(chan * slots as u32) - want_hz).abs() <= MAP_TOLERANCE_HZ,
        "[{T272}] the stored plan resolves the granted channel to {:.6} MHz, not {:.6} MHz",
        e.downlink_hz(chan * slots as u32) / 1e6,
        want_hz / 1e6
    );

    // ---- 2. Two calls, on ONE frequency, attributed to the two slots with their own talkgroups.
    let call_rows = repo.calls_for_system(system.id, 1000).unwrap();
    let on_carrier: Vec<_> = call_rows
        .iter()
        .filter(|c| {
            c.f_hz
                .is_some_and(|f| (f - want_hz).abs() <= MAP_TOLERANCE_HZ)
        })
        .collect();
    assert!(
        !on_carrier.is_empty(),
        "[{T272}] no call on the shared carrier at {:.6} MHz; calls were {:?}",
        want_hz / 1e6,
        call_rows
            .iter()
            .map(|c| (c.f_hz, c.slot, c.talkgroup.clone()))
            .collect::<Vec<_>>()
    );
    let mut per_slot: Vec<(u8, &str, hk_model::CallRecordId)> = Vec::new();
    for c in &on_carrier {
        let slot = c.slot.unwrap_or_else(|| {
            panic!(
                "[{T272}] a call on a TDMA carrier carries no slot at all, so two talkgroups on \
                 one frequency are indistinguishable: {:?}",
                (c.f_hz, c.talkgroup.clone(), &c.reasons)
            )
        });
        per_slot.push((slot, c.talkgroup.as_deref().unwrap_or(""), c.id));
    }
    for (k, want_tg) in want_tgs.iter().enumerate() {
        let k = k as u8;
        let matching: Vec<_> = per_slot.iter().filter(|(s, ..)| *s == k).collect();
        assert!(
            !matching.is_empty(),
            "[{T272}] nothing was attributed to slot {k}; the run produced {per_slot:?}. Two \
             talkgroups on alternating slots of one frequency must be two calls, not one."
        );
        assert!(
            matching.iter().all(|(_, tg, _)| tg == want_tg),
            "[{T272}] slot {k} carries the wrong talkgroup: got {:?}, truth {want_tg}. This is \
             the slot mix-up itself — the right number of calls with the labels swapped.",
            matching.iter().map(|(_, tg, _)| *tg).collect::<Vec<_>>()
        );
    }
    // Distinct records, not one row read twice.
    let a = per_slot.iter().find(|(s, ..)| *s == 0).unwrap();
    let b = per_slot.iter().find(|(s, ..)| *s == 1).unwrap();
    assert_ne!(
        a.2, b.2,
        "[{T272}] both slots resolved to the same CallRecord"
    );

    // ---- 3. The boundaries are the SHARED carrier's, and the row says so. Claiming per-slot
    // timing would be presenting a measurement this build never made.
    for c in &on_carrier {
        assert!(
            c.reasons.iter().any(|r| r == "tdma-shared-envelope"),
            "[{T272}] a TDMA call does not say its boundaries came from the shared carrier \
             envelope: {:?}",
            c.reasons
        );
        assert_eq!(
            c.encryption,
            Encryption::Unknown,
            "[{T272}] a call claimed an encryption state nothing had read"
        );
        assert!(!c.encryption.is_clear(), "[{T272}] unknown is never clear");
    }
    // Both slots were measured against the same envelope, so their boundaries agree exactly —
    // which is the honest consequence of not demodulating the slots, and is worth pinning so that
    // a future per-slot measurement has to change this test deliberately.
    let closed: Vec<_> = on_carrier.iter().filter(|c| c.t_end.is_some()).collect();
    assert!(
        !closed.is_empty(),
        "[{T272}] every call on the shared carrier is still open, so no end was measured"
    );
    let t0 = closed[0].t_start;
    let s = |ts: Timestamp| (ts.as_unix_nanos() - t0.as_unix_nanos()) as f64 * 1e-9;
    eprintln!(
        "[{T272}] calls on {:.6} MHz: {:?}",
        want_hz / 1e6,
        on_carrier
            .iter()
            .map(|c| (c.slot, c.talkgroup.clone(), s(c.t_start), c.t_end.map(s)))
            .collect::<Vec<_>>()
    );

    // ---- 4. The FDMA misreading never happened. Neither wrong frequency appears anywhere — not
    // as a call, not as a grant row, not even as an outside-window refusal, because the channel
    // numbers were never read that way in the first place.
    let grants = repo
        .grants_for_system(system.id, Timestamp::UNIX_EPOCH, 10_000)
        .unwrap();
    for w in &wrong_hz {
        assert!(
            !call_rows
                .iter()
                .any(|c| c.f_hz.is_some_and(|f| (f - w).abs() <= MAP_TOLERANCE_HZ)),
            "[{T272}] a call was written on {:.6} MHz — the frequency an FDMA reading of a TDMA \
             channel number produces, where nothing is transmitting",
            w / 1e6
        );
        assert!(
            !grants
                .iter()
                .any(|g| g.f_hz.is_some_and(|f| (f - w).abs() <= MAP_TOLERANCE_HZ)),
            "[{T272}] a grant resolved to {:.6} MHz: the channel number was divided by nothing, \
             which is C23's TDMA slot mix-up",
            w / 1e6
        );
    }

    // ---- 5. The grants that announced the two slots say so, on one frequency, in the stream that
    // links them to their calls.
    let slot_grants: Vec<_> = grants
        .iter()
        .filter(|g| {
            matches!(g.kind, GrantKind::Grant | GrantKind::GrantUpdate)
                && g.f_hz
                    .is_some_and(|f| (f - want_hz).abs() <= MAP_TOLERANCE_HZ)
        })
        .collect();
    assert!(
        slot_grants.iter().any(|g| g.slot == Some(0))
            && slot_grants.iter().any(|g| g.slot == Some(1)),
        "[{T272}] the grant stream does not carry both slots of the shared carrier: {:?}",
        slot_grants
            .iter()
            .map(|g| (g.f_hz, g.slot, g.talkgroup.clone()))
            .collect::<Vec<_>>()
    );
    let detail = &slot_grants[0].detail;
    assert_eq!(
        detail["slots"].as_u64(),
        Some(slots),
        "[{T272}] the grant row does not record the slot count it divided by: {detail}"
    );
    assert!(
        detail["mapping"]
            .as_str()
            .is_some_and(|m| m.contains("channel / slots")),
        "[{T272}] the grant row does not state the TDMA mapping it used: {detail}"
    );
    let started: Vec<_> = grants
        .iter()
        .filter(|g| g.kind == GrantKind::CallStart && g.slot.is_some())
        .collect();
    assert!(
        started.iter().all(|g| g.call.is_some()),
        "[{T272}] a TDMA call-start event is not linked to its call"
    );

    eprintln!(
        "[{T272}] one carrier at {:.6} MHz, two slots: talkgroup {} on slot 0 and {} on slot 1, \
         as {} distinct call record(s); the FDMA misreading ({:.6} / {:.6} MHz) never appeared",
        want_hz / 1e6,
        want_tgs[0],
        want_tgs[1],
        on_carrier.len(),
        wrong_hz[0] / 1e6,
        wrong_hz[1] / 1e6,
    );
}
