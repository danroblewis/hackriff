//! T-345 (C23): a **third** trunking protocol found blind, decoded properly, and the frequency the
//! standard itself says nobody can compute — through the mock SDR, during a normal pipeline run.
//!
//! T-271 left NXDN **identified but not decoded**: its frame sync word was verified by arithmetic
//! and then nothing was read from the air, because the CAC's coding could not be corroborated. That
//! was the honest answer at the time. This scene is the other half.
//!
//! # Why this decode is a real decode
//!
//! P25's and DMR's framings in this repo are deliberately flattened — no trellis, no BPTC, no
//! interleaving — because a synthetic fixture and a synthetic decoder can agree on a simpler frame
//! without lying about anything that matters. NXDN's is **not**: the CAC is descrambled with the
//! specification's PN generator, deinterleaved from its 25 × 12 block, depunctured against the
//! published 12-of-14 matrix, Viterbi decoded through the (23, 35) octal K = 5 code and CRC checked.
//! Nothing short of all five produces a valid block, so a run that decodes one has reproduced the
//! published coding chain rather than a convention this repo invented.
//!
//! # The trap, and why it is the harder half
//!
//! P25 announces its band plan, so T-268 resolves a grant from the air alone. DMR does not, so
//! T-271 refuses. NXDN refuses for a **stronger** reason, and the specification is what states it:
//! §6.5.31 defines Channel as a ten-bit **number**, 1 to 1023, and the air interface defines no
//! mapping from one to hertz — not one of its information elements is a frequency, and even
//! `CCH_INFO`, the message that tells a radio about its site's control channels, names them by
//! number. The map lives in the radio's configuration.
//!
//! The tempting mistake is therefore the same one DMR offers: assume the step is the 12.5 kHz LMR
//! raster and the base is wherever the radio is tuned. So the scene parks **real, followable voice
//! keyings exactly where that assumption points**, and bait a *second* assignment onto a channel
//! number whose assumed frequency carries one of the bursty NBFM neighbours. A decoder that guessed
//! would not merely print a wrong number: it would allocate the channel, find a transmission,
//! measure its boundaries and write a completely convincing call record.
//!
//! So this test asserts the refusal *in the presence of the reward for not refusing*:
//!
//! 1. the control channel is confirmed and named `nxdn-type-c` — which proves the whole coding
//!    chain was working, so the refusals below are not a broken decoder producing nothing;
//! 2. its assignments are **fully decoded** — channel number, call type, destination group or unit,
//!    source unit — because a dropped grant is the silence every M4 task exists to prevent;
//! 3. **no grant carries a frequency**, every one says `no-channel-map`, and neither plausible
//!    wrong frequency appears anywhere in the run;
//! 4. **no call exists**, because a grant with no frequency entitles no channel and no call.
//!
//! And the run still has to say what it cannot follow at all (Capacity Plus's moving rest channel,
//! NXDN Type-D's distributed trunking), because a system like that produces no grants — and so does
//! a quiet band, and so does a radio pointed somewhere else.
//!
//! Metadata only: `recordings == 0`, nothing is decrypted, and nothing says `clear`.

use hk_core::Pacing;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::{Encryption, GrantKind, Timestamp, TrunkProtocol};
use serde_json::json;

use crate::blind::{replay_config, start};
use crate::common::*;

const T345: &str = "T-345";

/// How near a baited frequency a reported one has to be to count as the mistake.
///
/// Generous on purpose, and in the one direction that matters: the assertion below is that
/// **nothing** lands near either, so a wide window can only make that assertion harder to satisfy.
/// A tight one would let a decoder that guessed and then rounded slip through.
const TRAP_WINDOW_HZ: f64 = 6_250.0;

#[test]
fn t345_a_normal_run_decodes_an_nxdn_control_channel_and_refuses_to_invent_its_channel_frequencies()
{
    let out = synth_or_skip!(
        SynthRequest::new("trunk_nxdn_control_channel")
            .seed(345)
            .param("duration_s", 2.0)
    );
    let fx = out.fixture(0).unwrap();

    let dir = TempDir::new("t345");
    let (cfg, dev) = replay_config(&dir.0, &fx.meta_path, json!({}), Pacing::Unpaced);
    let summary = finish(start(cfg, dev));

    let count = |p: &str| summary.counter(p);
    let (cacs, nxdn_grants) = (count("/chains/cc_cacs"), count("/chains/cc_nxdn_grants"));
    let (mapped, unmapped) = (
        count("/chains/cc_grants_mapped"),
        count("/chains/cc_grants_unmapped"),
    );
    eprintln!(
        "[{T345}] {} CC confirmed, {cacs} CAC(s), {nxdn_grants} NXDN assignment(s), {mapped} \
         mapped / {unmapped} unmapped, {} call(s), {} follow(s)",
        count("/chains/cc_confirmed"),
        count("/chains/cc_calls"),
        count("/chains/cc_follows"),
    );

    // ---- M4 is metadata-only, and adding a protocol did not add a content path.
    assert_eq!(
        count("/chains/recordings"),
        0,
        "[{T345}] M4 is metadata-only: nothing may be recorded"
    );
    for suffix in [".wav", ".sigmf-data"] {
        assert!(
            files_with_suffix(&dir.0, suffix).is_empty(),
            "[{T345}] the NXDN path wrote {suffix} content; M4 records metadata only"
        );
    }

    // ---- 0. THE STATEMENT ABOUT WHAT THIS BUILD CAN AND CANNOT FOLLOW.
    //
    // Asserted first and unconditionally, because it is the half that has to hold even in a run
    // that found nothing — and because this task *changes* a row in it, which is the one way a
    // support table quietly becomes a lie.
    let support = summary
        .counters
        .pointer("/trunking/support")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("[{T345}] the run reports no trunking support table at all"));
    let row = |name: &str| {
        support
            .iter()
            .find(|r| r["name"] == name)
            .unwrap_or_else(|| {
                panic!(
                    "[{T345}] {name} is missing from the run's trunking support table, which reads \
                     exactly like nobody having considered it"
                )
            })
    };
    // Type-C now decodes, and the row has to say that its grants still carry no frequency —
    // otherwise "decoded" invites a reader to expect one the run never produces.
    let type_c = row("NXDN Type-C");
    assert_eq!(
        type_c["level"], "decoded",
        "[{T345}] NXDN Type-C is still reported {}, but this build decodes its CACs",
        type_c["level"]
    );
    let reason = type_c["reason"].as_str().unwrap_or_default().to_lowercase();
    for must_say in ["not resolved to a frequency", "unmapped-channel"] {
        assert!(
            reason.contains(must_say),
            "[{T345}] the NXDN Type-C row does not say {must_say:?}: {reason}"
        );
    }
    // And Type-D is still structurally unreachable. The two NXDN rows say different things, which
    // is the distinction a person most easily loses.
    let type_d = row("NXDN Type-D");
    assert_eq!(
        type_d["level"], "unsupported",
        "[{T345}] {}",
        type_d["level"]
    );
    assert!(
        type_d["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("no dedicated control channel"),
        "[{T345}] NXDN Type-D's reason does not state the structural fact"
    );
    assert_eq!(row("Motorola Capacity Plus")["level"], "unsupported");
    let text = summary.to_text();
    assert!(
        text.contains("Capacity Plus") && text.contains("NXDN Type-D"),
        "[{T345}] the run summary does not name the systems it cannot follow:\n{text}"
    );

    // ---- Truth, opened only now, and only to check the answer.
    let scenario = fx.scenario().unwrap();
    let d = &scenario.value["trunking"]["nxdn"];
    let want_channel = d["grant_channel"].as_u64().unwrap();
    let want_destination = d["grant_destination_id"].as_u64().unwrap().to_string();
    let want_source = d["grant_source_id"].as_u64().unwrap().to_string();
    let second_channel = d["second_channel"].as_u64().unwrap();
    let trap_hz = d["wrong_frequency_if_channel_assumed_hz"].as_f64().unwrap();
    let second_trap_hz = d["wrong_frequency_for_second_channel_hz"].as_f64().unwrap();

    // The scene really does bait the trap: without traffic there, refusing costs nothing and this
    // test would prove nothing.
    assert!(
        !d["trap_keyings_s"].as_array().unwrap().is_empty(),
        "[{T345}] the trap channel carries no traffic, so a guessing decoder would gain nothing \
         and the refusal below is untested"
    );
    assert!(
        d["trap_offset_hz"].as_f64().unwrap().abs() < 0.4 * d["sample_rate_hz"].as_f64().unwrap(),
        "[{T345}] the trap channel is outside the window this run holds, so a guessing decoder \
         could not have followed it anyway"
    );
    assert_ne!(want_channel, second_channel);

    // ---- 1. THE POSITIVE CONTROL. An NXDN control channel was found blind, its CACs were decoded
    // through the whole published coding chain, and the system was named from them.
    let repo = repo(&dir.0);
    let rows = repo.trunk_systems().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "[{T345}] expected exactly one control channel on file, got {:?}",
        rows.iter()
            .map(|r| (r.protocol, r.cc_freq_hz))
            .collect::<Vec<_>>()
    );
    let system = &rows[0];
    assert_eq!(
        system.protocol,
        TrunkProtocol::NxdnTypeC,
        "[{T345}] the control channel was confirmed but read as {:?}. An NXDN frame sync alone is \
         not enough to name a protocol — corroborated RCCH-outbound messages are — so this failing \
         means either the wrong framing matched or no CAC survived the coding chain.",
        system.protocol
    );
    assert!(
        cacs >= 2,
        "[{T345}] only {cacs} CAC(s) decoded; naming the protocol needs corroboration, and a CAC \
         only exists at all if the descramble, deinterleave, depuncture, Viterbi and CRC all agreed \
         with the specification"
    );

    // The band plan stays empty: NXDN announces no frequencies, and nothing may put one there.
    let plan = repo.channel_plan(system.id).unwrap();
    assert!(
        plan.is_empty(),
        "[{T345}] an NXDN system acquired {} channel-plan entr(ies). The air interface carries no \
         frequency at all, so any entry here was invented: {:?}",
        plan.len(),
        plan
    );

    // ---- 2. THE ASSIGNMENTS ARE DECODED, not dropped. Everything the message said is on file.
    let grants = repo
        .grants_for_system(system.id, Timestamp::UNIX_EPOCH, 10_000)
        .unwrap();
    assert!(
        !grants.is_empty(),
        "[{T345}] the run wrote no grant at all. A decoder that cannot resolve a channel number \
         must still record the assignment — dropping it is the silence this milestone exists to \
         remove."
    );
    assert!(
        nxdn_grants >= 1,
        "[{T345}] no NXDN assignment was counted during the run"
    );

    let named = grants
        .iter()
        .find(|g| {
            g.channel.as_deref() == Some(want_channel.to_string().as_str())
                && g.detail["message"].as_str() == Some("vcall-assgn")
        })
        .unwrap_or_else(|| {
            panic!(
                "[{T345}] no voice assignment for channel {want_channel}; grants were {:?}",
                grants
                    .iter()
                    .map(|g| (g.channel.clone(), g.detail["message"].clone(), g.f_hz))
                    .collect::<Vec<_>>()
            )
        });
    assert_eq!(
        named.kind,
        GrantKind::UnmappedChannel,
        "[{T345}] an NXDN assignment was recorded as {:?}",
        named.kind
    );
    assert_eq!(
        named.talkgroup.as_deref(),
        Some(want_destination.as_str()),
        "[{T345}] the group call's destination group was not decoded"
    );
    assert_eq!(
        named.unit_id.as_deref(),
        Some(want_source.as_str()),
        "[{T345}] the assignment's source radio was not decoded"
    );
    assert_eq!(named.detail["channel"].as_u64(), Some(want_channel));
    assert_eq!(named.detail["protocol"].as_str(), Some("nxdn-type-c"));
    assert_eq!(
        named.detail["ran"].as_u64(),
        d["ran"].as_u64(),
        "[{T345}] the site's RAN was not read off the frame's own SR header: {}",
        named.detail
    );
    assert_eq!(named.detail["destination_is_group"].as_bool(), Some(true));
    named.validate().expect("a writable row");

    // The second assignment is an INDIVIDUAL call, so its destination is a radio and must not be
    // recorded as a talkgroup — a small lie a call list would repeat forever.
    let individual = grants
        .iter()
        .find(|g| {
            g.channel.as_deref() == Some(second_channel.to_string().as_str())
                && g.detail["call_type"].as_u64() == Some(0b100)
        })
        .unwrap_or_else(|| {
            panic!(
                "[{T345}] the individual-call assignment on channel {second_channel} was not \
                 recorded; grants were {:?}",
                grants
                    .iter()
                    .map(|g| (g.channel.clone(), g.detail["call_type"].clone()))
                    .collect::<Vec<_>>()
            )
        });
    assert_eq!(
        individual.talkgroup, None,
        "[{T345}] an individual call's destination was recorded as a talkgroup"
    );
    assert_eq!(
        individual.detail["destination"].as_u64(),
        d["second_destination_id"].as_u64()
    );

    // ---- 3. THE REFUSAL. No grant carries a frequency, and every one says why.
    let with_frequency: Vec<_> = grants.iter().filter(|g| g.f_hz.is_some()).collect();
    assert!(
        with_frequency.is_empty(),
        "[{T345}] {} NXDN assignment(s) resolved to a frequency, but the air interface carries a \
         channel NUMBER and defines no mapping from one to hertz — so every one of these was \
         invented from an assumed base and step, which is C23's stale-band-plan pitfall committed \
         deliberately: {:?}",
        with_frequency.len(),
        with_frequency
            .iter()
            .map(|g| (g.channel.clone(), g.f_hz))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        mapped, 0,
        "[{T345}] the run counted {mapped} mapped grant(s) on a system that announces no frequency"
    );
    assert!(unmapped >= nxdn_grants, "[{T345}] grants went uncounted");
    for g in &grants {
        assert_eq!(
            g.detail["reason"].as_str(),
            Some("no-channel-map"),
            "[{T345}] a grant gives no reason for having no frequency: {}",
            g.detail
        );
        // The octets whose meaning is unverified are recorded and interpreted by nothing.
        for key in ["unverified_spare_flags", "unverified_cc_option"] {
            assert!(
                g.detail.get(key).is_some(),
                "[{T345}] the grant row drops {key}, whose meaning is unverified: {}",
                g.detail
            );
        }
    }

    // The specific numbers that must appear nowhere. Real traffic sits on both — keyings on the
    // first, a bursty NBFM neighbour on the second — so a guessing decoder would have been rewarded
    // with call records that looked entirely correct.
    for (what, hz) in [
        ("the assigned channel", trap_hz),
        ("the second channel", second_trap_hz),
    ] {
        let near: Vec<_> = grants
            .iter()
            .filter(|g| g.f_hz.is_some_and(|f| (f - hz).abs() <= TRAP_WINDOW_HZ))
            .collect();
        assert!(
            near.is_empty(),
            "[{T345}] {} grant(s) landed on {:.6} MHz — the frequency an ASSUMED 12.5 kHz band plan \
             would give for {what}, and where the scene parked real emissions for exactly this \
             reason",
            near.len(),
            hz / 1e6
        );
    }

    // ---- 4. NO CALL. A grant with no frequency entitles no channel and no call.
    let calls = repo.calls_for_system(system.id, 1000).unwrap();
    assert!(
        calls.is_empty(),
        "[{T345}] the run wrote {} call(s) on a system whose assignments resolve to no frequency. \
         A call means a channel was allocated and an envelope measured, which cannot have happened \
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
        "[{T345}] a channel was followed on a system with no resolvable channel numbers"
    );

    // ---- 5. Nothing claimed an encryption state, and nothing said `clear`.
    //
    // NXDN's Cipher Type element rides in traffic-channel messages, which nothing here demodulates,
    // and an assignment's nine mandatory octets are accounted for to the bit — there is no room for
    // one. So nothing a Type-C control channel says bears on encryption at all.
    for g in &grants {
        assert_eq!(
            g.encryption,
            Encryption::Unknown,
            "[{T345}] an NXDN assignment claimed the encryption state {:?} from a control-channel \
             message that carries no Cipher Type field",
            g.encryption
        );
        assert!(!g.encryption.is_clear(), "[{T345}] unknown is never clear");
    }

    eprintln!(
        "[{T345}] NXDN Type-C confirmed blind on {:.6} MHz: {cacs} CAC(s) through the full coding \
         chain, {} assignment(s) fully decoded (channel {want_channel} -> group {want_destination}, \
         radio {want_source}), none resolved to a frequency, {:.6} and {:.6} MHz never reported \
         despite carrying real emissions, no calls, no audio",
        system.cc_freq_hz.unwrap_or(f64::NAN) / 1e6,
        grants.len(),
        trap_hz / 1e6,
        second_trap_hz / 1e6,
    );
}
