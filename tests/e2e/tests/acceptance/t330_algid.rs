//! T-330 (C23): a call's **own** ALGID, read blind from IQ off its granted channel's LDU2s,
//! decides the call's encryption state — and is the only thing that can earn a `VoicePermit`.
//!
//! T-270 proved the ALGID table where it was reachable (unit tests) and rested its e2e half on
//! service options. T-849 made the ALGID reachable from IQ and recorded it on the call-start event
//! without deciding anything. This test asks the remaining question, through the mock SDR with
//! the built-in chains and nothing configured: does the call row now say what the call itself said?
//!
//! # The scene
//!
//! T-849's: two granted channels carry real P25 Phase 1 voice frames, and both grants carry
//! service options `0` — the control channel states **nothing** about either call. One call's
//! LDU2s carry ALGID `0x80`, the other's AES-256 and a key id. So a call row that comes out
//! `clear` or `encrypted` can only have been decided by that call's own voice frames, and the pair
//! disagreeing catches an answer taken from anywhere else. A third followed channel keys unframed
//! 4FSK: no LDU2, so its calls must stay exactly what their grants said — `unknown`, never clear.
//!
//! The grant-`Encrypted` × header-clear contradiction is not staged here (the scene's grants say
//! nothing); `CallHeader`'s unit tests pin that it stays encrypted.
//!
//! Metadata only: nothing is recorded, nothing decrypted, no audio exists — a permit is an answer,
//! not a voice path.

use hk_core::Pacing;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::{Encryption, EncryptionEvidence, GrantKind, Timestamp};
use serde_json::json;

use crate::blind::{recording_start, replay_config, start};
use crate::common::*;

const T330: &str = "T-330";

/// Grant frequencies are integer arithmetic on decoded fields (T-268).
const MAP_TOLERANCE_HZ: f64 = 1.0;

#[test]
fn t330_a_calls_own_algid_decides_its_encryption_and_only_clear_by_algid_earns_a_permit() {
    let out = synth_or_skip!(
        SynthRequest::new("trunk_voice_frames_control_channel")
            .seed(849)
            .param("duration_s", 2.0)
    );
    let fx = out.fixture(0).unwrap();
    let _t0 = recording_start(&fx);

    let dir = TempDir::new("t330");
    let (cfg, dev) = replay_config(&dir.0, &fx.meta_path, json!({}), Pacing::Unpaced);
    let summary = finish(start(cfg, dev));
    let count = |p: &str| summary.counter(p);

    assert_eq!(
        count("/chains/recordings"),
        0,
        "[{T330}] M4 is metadata-only: a permit is not a recording"
    );
    for suffix in [".wav", ".sigmf-data"] {
        assert!(
            files_with_suffix(&dir.0, suffix).is_empty(),
            "[{T330}] a permitted call wrote {suffix} content"
        );
    }

    // ---- Truth, opened only now, and only to check the answer.
    let scenario = fx.scenario().unwrap();
    let t = &scenario.value["trunking"]["tsbk"];
    let channels = t["voice_frames"]["channels"].as_array().unwrap();
    let plain_hz = t["follow_target_hz"].as_f64().unwrap();

    let repo = repo(&dir.0);
    let systems = repo.trunk_systems().unwrap();
    assert_eq!(
        systems.len(),
        1,
        "[{T330}] exactly one control channel on file"
    );
    let system = &systems[0];
    let grants = repo
        .grants_for_system(system.id, Timestamp::UNIX_EPOCH, 10_000)
        .unwrap();
    let calls = repo.calls_for_system(system.id, 1000).unwrap();
    let on = |hz: f64| {
        calls
            .iter()
            .filter(move |c| c.f_hz.is_some_and(|f| (f - hz).abs() <= MAP_TOLERANCE_HZ))
    };
    let call_start = |id| {
        grants
            .iter()
            .find(|g| g.kind == GrantKind::CallStart && g.call == Some(id))
    };

    let mut by_algid = 0u64;
    for ch in channels {
        let name = ch["name"].as_str().unwrap();
        let hz = ch["target_hz"].as_f64().unwrap();
        let want_algid = ch["algid"].as_u64().unwrap() as u8;
        let want_kid = ch["key_id"].as_u64().unwrap() as u16;
        let want_clear = want_algid == hk_model::P25_ALGID_CLEAR;

        let rows: Vec<_> = on(hz).collect();
        assert!(
            !rows.is_empty(),
            "[{T330}] no call was followed onto the {name} channel at {:.6} MHz",
            hz / 1e6
        );
        let mut decided = 0;
        for call in &rows {
            let ev = call_start(call.id).expect("a new call has its call-start event");
            let heard = ev.detail["voice_frames"]["ldu2"].as_u64().unwrap_or(0);
            eprintln!(
                "[{T330}] {name}: call {} {:?} ({heard} LDU2) reasons {:?}; header {}",
                call.id, call.encryption, call.reasons, ev.detail["header_encryption"]
            );
            // Never `clear` on anything but the call's own ALGID — the T-266 invariant, on the
            // row a person reads.
            if call.encryption.is_clear() {
                assert_eq!(
                    call.encryption.evidence(),
                    Some(EncryptionEvidence::Algid),
                    "[{T330}] {name}: a call is clear on something other than its ALGID"
                );
            }
            if heard == 0 {
                // No LDU2 of this transmission decoded: what the grant said stands.
                assert_eq!(
                    call.encryption,
                    Encryption::Unknown,
                    "[{T330}] {name}: a call with no LDU2 claimed {:?}",
                    call.encryption
                );
                continue;
            }
            decided += 1;
            // ---- THE ASSERTION THIS TICKET EXISTS FOR: the row says what the call said.
            assert_eq!(
                call.encryption.evidence(),
                Some(EncryptionEvidence::Algid),
                "[{T330}] {name}: {heard} LDU2 heard but the call was not decided by its ALGID: \
                 {:?}",
                call.encryption
            );
            assert_eq!(
                call.encryption.algid(),
                Some(want_algid),
                "[{T330}] {name}: ALGID"
            );
            assert_eq!(
                call.encryption.key_id(),
                Some(want_kid),
                "[{T330}] {name}: key id"
            );
            assert_eq!(
                call.encryption.is_clear(),
                want_clear,
                "[{T330}] {name}: state"
            );
            assert_eq!(
                ev.detail["encryption"],
                call.encryption.state(),
                "[{T330}] {name}: event and row disagree"
            );
            assert_eq!(ev.detail["encryption_evidence"], "algid");
            assert_eq!(ev.detail["header_encryption"]["contradicts_grant"], false);

            // ---- The permit: asked for, and earned only by clear-by-ALGID.
            let refusals = [
                "encrypted-no-audio",
                "encryption-unknown-no-audio",
                "clear-unconfirmed-no-audio",
            ];
            let refused: Vec<_> = call
                .reasons
                .iter()
                .filter(|r| refusals.contains(&r.as_str()))
                .collect();
            if want_clear {
                assert_eq!(
                    ev.detail["voice"], "permitted",
                    "[{T330}] {name}: clear by its own ALGID and still refused"
                );
                assert!(
                    refused.is_empty(),
                    "[{T330}] {name}: stale refusal {refused:?}"
                );
            } else {
                assert_eq!(ev.detail["voice"], "encrypted-no-audio", "[{T330}] {name}");
                assert_eq!(refused, ["encrypted-no-audio"], "[{T330}] {name}");
            }
        }
        assert!(
            decided >= 1,
            "[{T330}] {name}: no call on the channel had an LDU2 attributed to it"
        );
        by_algid += decided;
    }

    // ---- The control: unframed 4FSK carries no ALGID, so its calls stay what the grant said.
    let plain: Vec<_> = on(plain_hz).collect();
    assert!(
        !plain.is_empty(),
        "[{T330}] the control channel produced no call"
    );
    for call in plain {
        assert_eq!(
            call.encryption,
            Encryption::Unknown,
            "[{T330}] a call with no voice frames claimed {:?}",
            call.encryption
        );
        assert!(
            call.reasons
                .iter()
                .any(|r| r == "encryption-unknown-no-audio"),
            "[{T330}] unknown must still be refused: {:?}",
            call.reasons
        );
    }

    assert_eq!(
        count("/chains/cc_calls_algid"),
        by_algid,
        "[{T330}] the counter disagrees with the rows"
    );
}
