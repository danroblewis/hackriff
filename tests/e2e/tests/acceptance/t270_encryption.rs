//! T-270 (C23): the **encryption check before any voice path** — blind, through the mock SDR,
//! during a normal pipeline run.
//!
//! T-269 followed grants onto channels and measured call boundaries, and every call it wrote said
//! `unknown` about encryption because nothing had read an encryption indication. This reads one.
//! It asks the question C23 §Methods puts before the vocoder — *may this be listened to?* — and it
//! asks it the way T-268 and T-269 do: no decoder types anywhere in the test, the built-in chain
//! registry, nothing configured, then read the repository.
//!
//! # The two halves, and why the control is the harder one
//!
//! The scene stages three granted voice channels **inside** the window, placed by one function so
//! they are indistinguishable in the samples — same power, same duty, same modulation, same kind of
//! payload. Nothing about the IQ says which is which. Only what the control channel *announced*
//! differs:
//!
//! 1. **The property.** One channel's grant carries the service-options encryption bit. Its call
//!    must be flagged `encrypted`, and must produce no audio.
//! 2. **The control.** One channel is announced **only by a grant update** — late entry, joined
//!    with no header. A real grant update carries no service-options octet at all, so nothing ever
//!    stated this channel's encryption state and it must record `unknown`. Never `clear`, however
//!    ordinary its traffic looks.
//!
//! The control is the one that can pass for the wrong reason: `unknown` is also what a *broken*
//! parser returns for everything. So the two halves are asserted in the **same run** — the
//! encrypted channel being correctly flagged is what proves the parser was working when it said
//! `unknown` about the other one. Either half alone would be worthless.
//!
//! # Nothing is decrypted, and nothing says "clear"
//!
//! No decryption is attempted, implemented or possible: the workspace contains no cipher. And no
//! row in this run may say `clear` — saying it requires an ALGID, an ALGID lives in the voice
//! frames on a granted channel (docs/04 §8.3), and nothing in M4 demodulates those. A `clear` row
//! here would mean something had claimed an encryption state from evidence it never had, which is
//! the T-266 invariant this whole chain of tasks exists to protect.
//!
//! M4 stays metadata-only: `recordings == 0`, and there is no `CallAudio` type or column that could
//! hold one.

use hk_core::Pacing;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::{Encryption, EncryptionEvidence, GrantKind, Timestamp};
use serde_json::json;

use crate::blind::{replay_config, start};
use crate::common::*;

const T270: &str = "T-270";

/// The mapping is integer arithmetic on decoded fields, so a hertz is already generous (T-268).
const MAP_TOLERANCE_HZ: f64 = 1.0;

#[test]
fn t270_a_normal_run_flags_an_encrypted_call_and_records_late_entry_as_unknown() {
    let out = synth_or_skip!(
        SynthRequest::new("trunk_encrypted_control_channel")
            .seed(270)
            .param("duration_s", 2.0)
    );
    let fx = out.fixture(0).unwrap();

    let dir = TempDir::new("t270");
    let (cfg, dev) = replay_config(&dir.0, &fx.meta_path, json!({}), Pacing::Unpaced);
    let summary = finish(start(cfg, dev));

    let count = |p: &str| summary.counter(p);
    let (calls, encrypted) = (
        count("/chains/cc_calls"),
        count("/chains/cc_calls_encrypted"),
    );
    let refused = count("/chains/cc_voice_refused");
    eprintln!(
        "[{T270}] {calls} call(s), {encrypted} flagged encrypted, {refused} refused a voice path \
         ({} channel(s) followed)",
        count("/chains/cc_follows")
    );

    // ---- M4 is metadata-only, and an encrypted call cannot produce audio because NO call does.
    assert_eq!(
        count("/chains/recordings"),
        0,
        "[{T270}] M4 is metadata-only: nothing may be recorded"
    );
    for suffix in [".wav", ".sigmf-data"] {
        assert!(
            files_with_suffix(&dir.0, suffix).is_empty(),
            "[{T270}] a followed call wrote {suffix} content; M4 records metadata only"
        );
    }

    // ---- Truth, opened only now, and only to check the answer.
    let scenario = fx.scenario().unwrap();
    let t = &scenario.value["trunking"]["tsbk"];
    let e = &t["encryption"];
    let want_enc_hz = e["encrypted_target_hz"].as_f64().unwrap();
    let want_enc_tg = e["encrypted_talkgroup"].as_u64().unwrap().to_string();
    let want_late_hz = e["late_entry_target_hz"].as_f64().unwrap();
    let want_late_tg = e["late_entry_talkgroup"].as_u64().unwrap().to_string();
    let want_svc = e["encrypted_service_options"].as_u64().unwrap();
    let fs = t["sample_rate_hz"].as_f64().unwrap();
    let usable_half = 0.4 * fs;

    // The scene actually stages both cases, inside the window, with traffic on each. Without this
    // the run could "pass" against a fixture that quietly stopped carrying one of them.
    for (what, off_key, keyings_key) in [
        ("encrypted", "encrypted_offset_hz", "encrypted_keyings_s"),
        ("late entry", "late_entry_offset_hz", "late_entry_keyings_s"),
    ] {
        assert!(
            e[off_key].as_f64().unwrap().abs() < usable_half,
            "[{T270}] the {what} channel is not inside the window this run holds"
        );
        assert!(
            !e[keyings_key].as_array().unwrap().is_empty(),
            "[{T270}] the {what} channel carries no traffic, so nothing would be followed"
        );
    }
    assert_ne!(
        want_enc_hz, want_late_hz,
        "[{T270}] the two channels must be distinct"
    );

    let repo = repo(&dir.0);
    let rows = repo.trunk_systems().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "[{T270}] exactly one control channel on file"
    );
    let system = &rows[0];
    let grants = repo
        .grants_for_system(system.id, Timestamp::UNIX_EPOCH, 10_000)
        .unwrap();
    let call_rows = repo.calls_for_system(system.id, 1000).unwrap();
    assert!(
        !call_rows.is_empty(),
        "[{T270}] the run wrote no call at all"
    );

    let at = |hz: f64| -> Vec<&hk_model::CallRecord> {
        call_rows
            .iter()
            .filter(|c| c.f_hz.is_some_and(|f| (f - hz).abs() <= MAP_TOLERANCE_HZ))
            .collect()
    };

    // ---- 1. THE PROPERTY. The grant that carried the encryption bit produced a flagged call.
    let enc_calls = at(want_enc_hz);
    assert!(
        !enc_calls.is_empty(),
        "[{T270}] no call on the encrypted channel at {:.6} MHz; calls were {:?}",
        want_enc_hz / 1e6,
        call_rows.iter().map(|c| c.f_hz).collect::<Vec<_>>()
    );
    let enc_call = enc_calls
        .iter()
        .find(|c| c.encryption.is_encrypted())
        .unwrap_or_else(|| {
            panic!(
                "[{T270}] the call on {:.6} MHz was granted with the encryption bit set but is \
                 recorded as {:?}. An encrypted call that reads as anything else is the failure \
                 this task exists to prevent.",
                want_enc_hz / 1e6,
                enc_calls.iter().map(|c| c.encryption).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        enc_call.encryption.evidence(),
        Some(EncryptionEvidence::ServiceOptions),
        "[{T270}] the flag does not name what said so"
    );
    assert!(
        !enc_call.encryption.is_clear(),
        "[{T270}] an encrypted call answered is_clear()"
    );
    assert!(
        enc_call.talkgroup.as_deref() == Some(want_enc_tg.as_str()),
        "[{T270}] the encrypted call carries the wrong talkgroup"
    );
    assert!(
        enc_call.reasons.iter().any(|r| r == "encrypted-no-audio"),
        "[{T270}] the encrypted call does not record that it was refused a voice path: {:?}",
        enc_call.reasons
    );
    assert!(
        encrypted >= 1,
        "[{T270}] no call was counted as encrypted during the run"
    );

    // ---- 2. THE CONTROL. Late entry — announced only by a grant update, so no header was ever
    // seen — records `unknown`. This is the assertion that a default-to-clear would fail.
    let late_calls = at(want_late_hz);
    assert!(
        !late_calls.is_empty(),
        "[{T270}] no call on the late-entry channel at {:.6} MHz; calls were {:?}",
        want_late_hz / 1e6,
        call_rows.iter().map(|c| c.f_hz).collect::<Vec<_>>()
    );
    for c in &late_calls {
        assert_eq!(
            c.encryption,
            Encryption::Unknown,
            "[{T270}] a channel announced only by a grant update — no header, nothing ever said — \
             claimed the encryption state {:?}. Note the encrypted channel in this SAME run was \
             correctly flagged, so the parser was working: this is a real claim from no evidence.",
            c.encryption
        );
        assert!(
            !c.encryption.is_clear(),
            "[{T270}] unknown is never clear (C23's late-entry pitfall)"
        );
        assert!(
            c.late_entry,
            "[{T270}] a call joined through a grant update is not marked late entry"
        );
        assert!(
            c.reasons.iter().any(|r| r == "encryption-unknown-no-audio"),
            "[{T270}] the late-entry call does not record that it was refused a voice path: {:?}",
            c.reasons
        );
    }
    assert!(
        late_calls
            .iter()
            .any(|c| c.talkgroup.as_deref() == Some(want_late_tg.as_str())),
        "[{T270}] the late-entry call carries the wrong talkgroup"
    );

    // ---- 3. THE ASSERTION THIS TEST EXISTS FOR. Nothing anywhere says "clear".
    //
    // Saying it needs an ALGID; an ALGID lives in the voice frames of a granted channel and nothing
    // in M4 demodulates those. So a `clear` row could only have come from evidence that was never
    // read — which is precisely the default this chain of tasks forbids.
    let clear_calls: Vec<_> = call_rows
        .iter()
        .filter(|c| c.encryption.is_clear())
        .collect();
    assert!(
        clear_calls.is_empty(),
        "[{T270}] {} call(s) claimed to be in the clear, but no ALGID is reachable in M4 and \
         nothing else may say it: {:?}",
        clear_calls.len(),
        clear_calls
            .iter()
            .map(|c| (c.f_hz, c.encryption))
            .collect::<Vec<_>>()
    );
    let clear_grants: Vec<_> = grants.iter().filter(|g| g.encryption.is_clear()).collect();
    assert!(
        clear_grants.is_empty(),
        "[{T270}] {} grant event(s) claimed to be in the clear: {:?}",
        clear_grants.len(),
        clear_grants
            .iter()
            .map(|g| (g.f_hz, g.encryption))
            .collect::<Vec<_>>()
    );

    // ---- 4. Every **followed** call was refused a voice path — encrypted and unknown alike. The
    // check sits where a vocoder would, so "no audio" is structural here rather than incidental.
    //
    // A grant that fell outside the window is deliberately excluded: no channel was ever allocated
    // for it, so there was no voice path to refuse. Its row records why it could not be observed
    // (T-269), not an encryption decision that was never reached.
    assert!(
        refused >= calls.min(1),
        "[{T270}] no call was put through the encryption check at all"
    );
    let followed_calls: Vec<_> = call_rows
        .iter()
        .filter(|c| !c.reasons.iter().any(|r| r == "grant-outside-window"))
        .collect();
    assert!(
        !followed_calls.is_empty(),
        "[{T270}] no call was followed, so the encryption check was never exercised"
    );
    assert!(
        followed_calls
            .iter()
            .all(|c| c.reasons.iter().any(|r| r.ends_with("-no-audio"))),
        "[{T270}] a call reached the follower without the encryption check recording a decision: \
         {:?}",
        followed_calls
            .iter()
            .map(|c| (c.f_hz, &c.reasons))
            .collect::<Vec<_>>()
    );

    // ---- 5. The grant stream carries the octet that decided it, so the row is a measurement.
    let enc_grant = grants
        .iter()
        .filter(|g| matches!(g.kind, GrantKind::Grant))
        .find(|g| {
            g.f_hz
                .is_some_and(|f| (f - want_enc_hz).abs() <= MAP_TOLERANCE_HZ)
        })
        .unwrap_or_else(|| panic!("[{T270}] no grant event for the encrypted channel"));
    assert!(
        enc_grant.encryption.is_encrypted(),
        "[{T270}] the grant event did not state the encryption its octet carried"
    );
    assert_eq!(
        enc_grant.detail["service_options"].as_u64(),
        Some(want_svc),
        "[{T270}] the grant row does not record the octet verbatim: {}",
        enc_grant.detail
    );
    assert_eq!(
        enc_grant.detail["service_options_encrypted"].as_bool(),
        Some(true),
        "[{T270}] the grant row does not say the encryption bit was set: {}",
        enc_grant.detail
    );
    // And the late-entry channel's events carry no octet at all, because the message has none.
    assert!(
        grants
            .iter()
            .filter(|g| g.kind == GrantKind::GrantUpdate
                && g.f_hz
                    .is_some_and(|f| (f - want_late_hz).abs() <= MAP_TOLERANCE_HZ))
            .all(|g| g.detail.get("service_options").is_none()
                && g.encryption == Encryption::Unknown),
        "[{T270}] a grant update reported a service-options octet it does not carry"
    );

    eprintln!(
        "[{T270}] {:.6} MHz granted with service options {want_svc:#04x} -> encrypted \
         (evidence {:?}), no audio; {:.6} MHz announced only by a grant update -> unknown, never \
         clear; {} call(s) total, none clear, all refused a voice path",
        want_enc_hz / 1e6,
        enc_call.encryption.evidence().unwrap(),
        want_late_hz / 1e6,
        call_rows.len(),
    );
}
