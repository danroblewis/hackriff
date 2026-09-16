//! T-271 (C23): a **second** trunking protocol found blind, and the frequency it refuses to
//! invent — through the mock SDR, during a normal pipeline run.
//!
//! Everything M4 has confirmed so far has been P25, and a hunt that only knows one framing passes
//! every P25 test whether or not it can find anything else. This scene puts a **DMR Tier III**
//! control channel on the air instead: a different frame sync, a different block, a different CRC,
//! narrower deviations. Nothing tells the run which it is. The hunt has to try what it knows and
//! confirm on what matches, with the same continuous unframed 4FSK decoy present to be rejected.
//!
//! # The trap, and why it is the harder half
//!
//! P25 announces its band plan: an `IDEN_UP` carries a base and a spacing, so a grant's channel
//! number becomes hertz from messages the receiver actually heard (T-268). **DMR Tier III does
//! not** — no channel-parameter announcement could be corroborated against an independent
//! reference — so a Logical Physical Channel Number resolves to *nothing*.
//!
//! The tempting mistake is to assume the obvious band plan: base = wherever the radio is tuned,
//! step = the 12.5 kHz LMR raster. So the scene parks **real, followable voice keyings exactly
//! where that assumption points.** A decoder that guessed would not merely print a wrong number:
//! it would allocate the channel, find a transmission, measure its boundaries and write a
//! completely convincing call record. Nothing in the run would look wrong.
//!
//! So this test asserts the refusal *in the presence of the reward for not refusing*:
//!
//! 1. the control channel is confirmed and named `dmr-tier3` — which proves the decoder was
//!    working, so the refusals below are not a broken parser producing nothing;
//! 2. its grants are **fully decoded** — logical channel, timeslot, target and source — because a
//!    dropped grant is the silence every M4 task exists to prevent;
//! 3. **no grant carries a frequency**, every one says `no-channel-parameters`, and the plausible
//!    wrong frequency appears nowhere in the run;
//! 4. **no call exists**, because a grant with no frequency entitles no channel and no call.
//!
//! # And the systems that cannot be followed at all
//!
//! The run also has to *say* what it cannot do. Capacity Plus trunks on a moving rest channel and
//! NXDN Type-D distributes its trunking into the traffic channels; neither has a dedicated control
//! channel, so a control-channel decoder cannot reach them however good it gets. A system like
//! that produces no grants — and so does a quiet band, and so does a radio pointed somewhere else.
//! The run summary must name them, with the reason, or those three pictures are the same picture.
//!
//! Metadata only: `recordings == 0`, nothing is decrypted, and nothing says `clear`.

use hk_core::Pacing;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::{Encryption, GrantKind, Timestamp, TrunkProtocol};
use serde_json::json;

use crate::blind::{replay_config, start};
use crate::common::*;

const T271: &str = "T-271";

/// How near the trap frequency a reported frequency has to be to count as the mistake.
///
/// Generous on purpose, and in the one direction that matters: the assertion below is that
/// **nothing** lands near it, so a wide window can only make that assertion harder to satisfy. A
/// tight one would let a decoder that guessed and then rounded slip through.
const TRAP_WINDOW_HZ: f64 = 6_250.0;

#[test]
fn t271_a_normal_run_finds_a_dmr_control_channel_and_refuses_to_invent_its_grant_frequencies() {
    let out = synth_or_skip!(
        SynthRequest::new("trunk_dmr_control_channel")
            .seed(271)
            .param("duration_s", 2.0)
    );
    let fx = out.fixture(0).unwrap();

    let dir = TempDir::new("t271");
    let (cfg, dev) = replay_config(&dir.0, &fx.meta_path, json!({}), Pacing::Unpaced);
    let summary = finish(start(cfg, dev));

    let count = |p: &str| summary.counter(p);
    let (csbks, dmr_grants) = (count("/chains/cc_csbks"), count("/chains/cc_dmr_grants"));
    let (mapped, unmapped) = (
        count("/chains/cc_grants_mapped"),
        count("/chains/cc_grants_unmapped"),
    );
    eprintln!(
        "[{T271}] {} CC confirmed, {csbks} CSBK(s), {dmr_grants} DMR grant(s), {mapped} mapped / \
         {unmapped} unmapped, {} call(s), {} follow(s)",
        count("/chains/cc_confirmed"),
        count("/chains/cc_calls"),
        count("/chains/cc_follows"),
    );

    // ---- M4 is metadata-only, and adding a protocol did not add a content path.
    assert_eq!(
        count("/chains/recordings"),
        0,
        "[{T271}] M4 is metadata-only: nothing may be recorded"
    );
    for suffix in [".wav", ".sigmf-data"] {
        assert!(
            files_with_suffix(&dir.0, suffix).is_empty(),
            "[{T271}] the DMR path wrote {suffix} content; M4 records metadata only"
        );
    }

    // ---- 0. THE STATEMENT ABOUT WHAT CANNOT BE FOLLOWED AT ALL.
    //
    // Asserted first and unconditionally, because it is the half that has to hold even in a run
    // that found nothing. A decoder that silently produces no grants for a Capacity Plus system is
    // indistinguishable from one pointed at empty spectrum.
    let support = summary
        .counters
        .pointer("/trunking/support")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("[{T271}] the run reports no trunking support table at all"));
    for (name, must_say) in [
        ("Motorola Capacity Plus", "rest channel"),
        ("NXDN Type-D", "distributed"),
    ] {
        let row = support
            .iter()
            .find(|r| r["name"] == name)
            .unwrap_or_else(|| {
                panic!(
                    "[{T271}] {name} is missing from the run's trunking support table, which reads \
                     exactly like nobody having considered it"
                )
            });
        assert_eq!(
            row["level"], "unsupported",
            "[{T271}] {name} is reported {} — it has no dedicated control channel at all, so it is \
             not merely unimplemented",
            row["level"]
        );
        let reason = row["reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains("no dedicated control channel"),
            "[{T271}] {name}'s reason does not state the structural fact: {reason}"
        );
        assert!(
            reason.contains(must_say),
            "[{T271}] {name}'s reason does not say how it trunks instead: {reason}"
        );
    }
    // And a person reading the run's own summary sees them named, not merely counted.
    let text = summary.to_text();
    assert!(
        text.contains("Capacity Plus") && text.contains("NXDN Type-D"),
        "[{T271}] the run summary does not name the systems it cannot follow:\n{text}"
    );

    // ---- Truth, opened only now, and only to check the answer.
    let scenario = fx.scenario().unwrap();
    let d = &scenario.value["trunking"]["dmr"];
    let want_lpcn = d["grant_lpcn"].as_u64().unwrap();
    let want_slot = d["grant_timeslot"].as_u64().unwrap() as u8;
    let want_target = d["grant_target_id"].as_u64().unwrap().to_string();
    let want_source = d["grant_source_id"].as_u64().unwrap().to_string();
    let second_lpcn = d["second_lpcn"].as_u64().unwrap();
    let second_slot = d["second_timeslot"].as_u64().unwrap() as u8;
    let trap_hz = d["wrong_frequency_if_lpcn_assumed_hz"].as_f64().unwrap();

    // The scene really does bait the trap: without traffic there, refusing costs nothing and this
    // test would prove nothing.
    assert!(
        !d["trap_keyings_s"].as_array().unwrap().is_empty(),
        "[{T271}] the trap channel carries no traffic, so a guessing decoder would gain nothing \
         and the refusal below is untested"
    );
    assert!(
        d["trap_offset_hz"].as_f64().unwrap().abs() < 0.4 * d["sample_rate_hz"].as_f64().unwrap(),
        "[{T271}] the trap channel is outside the window this run holds, so a guessing decoder \
         could not have followed it anyway"
    );
    assert_ne!(want_lpcn, second_lpcn);

    // ---- 1. THE POSITIVE CONTROL. A DMR control channel was found blind and named.
    let repo = repo(&dir.0);
    let rows = repo.trunk_systems().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "[{T271}] expected exactly one control channel on file, got {:?}",
        rows.iter()
            .map(|r| (r.protocol, r.cc_freq_hz))
            .collect::<Vec<_>>()
    );
    let system = &rows[0];
    assert_eq!(
        system.protocol,
        TrunkProtocol::DmrTier3,
        "[{T271}] the control channel was confirmed but read as {:?}. A DMR frame sync alone is \
         not enough to name a protocol — corroborated Tier III CSBKs are — so this failing means \
         either the wrong framing matched or no trunking message was decoded.",
        system.protocol
    );
    assert!(
        system.protocol.is_tdma(),
        "[{T271}] DMR Tier III is two-slot TDMA and the model must say so"
    );
    assert!(
        csbks >= 2,
        "[{T271}] only {csbks} CSBK(s) decoded; naming the protocol needs corroboration"
    );

    // The band plan stays empty: DMR announces none, and nothing may put one there.
    let plan = repo.channel_plan(system.id).unwrap();
    assert!(
        plan.is_empty(),
        "[{T271}] a DMR system acquired {} channel-plan entr(ies). DMR Tier III announces no \
         channel parameters this build can corroborate, so any entry here was invented: {:?}",
        plan.len(),
        plan
    );

    // ---- 2. THE GRANTS ARE DECODED, not dropped. Everything the message said is on file.
    let grants = repo
        .grants_for_system(system.id, Timestamp::UNIX_EPOCH, 10_000)
        .unwrap();
    assert!(
        !grants.is_empty(),
        "[{T271}] the run wrote no grant at all. A decoder that cannot resolve a channel number \
         must still record the grant — dropping it is the silence this milestone exists to remove."
    );
    assert!(
        dmr_grants >= 1,
        "[{T271}] no DMR grant was counted during the run"
    );

    let named = grants
        .iter()
        .find(|g| {
            g.channel.as_deref() == Some(want_lpcn.to_string().as_str())
                && g.slot == Some(want_slot)
        })
        .unwrap_or_else(|| {
            panic!(
                "[{T271}] no grant for logical channel {want_lpcn} on timeslot {want_slot}; grants \
                 were {:?}",
                grants
                    .iter()
                    .map(|g| (g.channel.clone(), g.slot, g.f_hz))
                    .collect::<Vec<_>>()
            )
        });
    assert_eq!(
        named.kind,
        GrantKind::UnmappedChannel,
        "[{T271}] a DMR grant was recorded as {:?}",
        named.kind
    );
    assert_eq!(
        named.talkgroup.as_deref(),
        Some(want_target.as_str()),
        "[{T271}] the broadcast grant's target group was not decoded"
    );
    assert_eq!(
        named.unit_id.as_deref(),
        Some(want_source.as_str()),
        "[{T271}] the grant's source radio was not decoded"
    );
    assert_eq!(
        named.detail["opcode"].as_str(),
        Some("btv-grant"),
        "[{T271}] the grant row does not name the opcode that issued it: {}",
        named.detail
    );
    assert_eq!(named.detail["lpcn"].as_u64(), Some(want_lpcn));
    assert_eq!(named.detail["protocol"].as_str(), Some("dmr-tier3"));
    named.validate().expect("a writable row");

    // Both timeslots are attributed, which is C23's slot mix-up pitfall as an assertion: a grant
    // whose slot is dropped is under-attributed, and on a two-slot system that is half the traffic.
    assert!(
        grants.iter().any(
            |g| g.channel.as_deref() == Some(second_lpcn.to_string().as_str())
                && g.slot == Some(second_slot)
        ),
        "[{T271}] the grant on logical channel {second_lpcn} / slot {second_slot} was not recorded \
         with its slot; grants were {:?}",
        grants
            .iter()
            .map(|g| (g.channel.clone(), g.slot))
            .collect::<Vec<_>>()
    );

    // ---- 3. THE REFUSAL. No grant carries a frequency, and every one says why.
    let with_frequency: Vec<_> = grants.iter().filter(|g| g.f_hz.is_some()).collect();
    assert!(
        with_frequency.is_empty(),
        "[{T271}] {} DMR grant(s) resolved to a frequency, but DMR Tier III announces no channel \
         parameters this build could corroborate — so every one of these was invented from an \
         assumed band plan, which is C23's stale-plan pitfall committed deliberately: {:?}",
        with_frequency.len(),
        with_frequency
            .iter()
            .map(|g| (g.channel.clone(), g.f_hz))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        mapped, 0,
        "[{T271}] the run counted {mapped} mapped grant(s) on a system with no band plan"
    );
    assert!(unmapped >= dmr_grants, "[{T271}] grants went uncounted");
    for g in &grants {
        assert_eq!(
            g.detail["reason"].as_str(),
            Some("no-channel-parameters"),
            "[{T271}] a grant gives no reason for having no frequency: {}",
            g.detail
        );
        // The uncorroborated payload bits are recorded and interpreted by nothing.
        assert!(
            g.detail.get("unverified_flag_bits").is_some(),
            "[{T271}] the grant row drops the payload bits whose meaning is unverified: {}",
            g.detail
        );
    }

    // The specific number that must appear nowhere: real traffic sits on it, so a guessing decoder
    // would have been rewarded with a call record that looked entirely correct.
    let near_trap: Vec<_> = grants
        .iter()
        .filter(|g| {
            g.f_hz
                .is_some_and(|f| (f - trap_hz).abs() <= TRAP_WINDOW_HZ)
        })
        .collect();
    assert!(
        near_trap.is_empty(),
        "[{T271}] {} grant(s) landed on {:.6} MHz — the frequency an ASSUMED 12.5 kHz band plan \
         would give for this logical channel, and where the scene parked real keyings for exactly \
         this reason",
        near_trap.len(),
        trap_hz / 1e6
    );

    // ---- 4. NO CALL. A grant with no frequency entitles no channel and no call.
    let calls = repo.calls_for_system(system.id, 1000).unwrap();
    assert!(
        calls.is_empty(),
        "[{T271}] the run wrote {} call(s) on a system whose grants resolve to no frequency. A \
         call means a channel was allocated and an envelope measured, which cannot have happened \
         without a frequency to allocate it on: {:?}",
        calls.len(),
        calls
            .iter()
            .map(|c| (c.f_hz, c.talkgroup.clone()))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        count("/chains/cc_follows"),
        0,
        "[{T271}] a channel was followed on a system with no resolvable channel numbers"
    );

    // ---- 5. Nothing claimed an encryption state, and nothing said `clear`.
    //
    // A DMR privacy indication lives in a PI header on the traffic channel, which nothing here
    // demodulates — the same position P25's ALGID is in — and the grant payload bits that might
    // carry one could not be corroborated, so no claim may be built on them in either direction.
    for g in &grants {
        assert_eq!(
            g.encryption,
            Encryption::Unknown,
            "[{T271}] a DMR grant claimed the encryption state {:?} from a payload layout this \
             build could not verify",
            g.encryption
        );
        assert!(!g.encryption.is_clear(), "[{T271}] unknown is never clear");
    }

    eprintln!(
        "[{T271}] DMR Tier III confirmed blind on {:.6} MHz: {csbks} CSBK(s), {} grant(s) fully \
         decoded (lpcn {want_lpcn}/slot {want_slot} -> group {want_target}, radio {want_source}), \
         none resolved to a frequency, {:.6} MHz never reported despite carrying real traffic, no \
         calls, no audio",
        system.cc_freq_hz.unwrap_or(f64::NAN) / 1e6,
        grants.len(),
        trap_hz / 1e6,
    );
}
