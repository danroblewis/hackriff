//! T-849 (C23): P25 Phase 1 voice frames on a **followed** channel — LDU1 link control and LDU2
//! encryption sync read blind from IQ, through the mock SDR, during a normal pipeline run.
//!
//! T-270 left the one authoritative encryption statement unread: the ALGID lives in the voice
//! frames of the granted channel, and nothing demodulated those. This test asks whether it is now
//! *reachable* — the way T-268/T-269/T-270 ask their questions: no decoder types anywhere in the
//! test, the built-in chain registry, nothing configured, then read the repository.
//!
//! # The scene, and why the grants are silent
//!
//! Two granted channels carry real voice frames. Both grants carry service options `0`, so the
//! control channel states **nothing** about either call's encryption — `unknown` for both. The only
//! place the difference lives is each call's own LDU2: ALGID `0x80` on one, AES-256 and a key id
//! on the other. So an ALGID that comes out right can only have come from demodulating the granted
//! channel, and one that came from anywhere else — the grant, a default, the other channel — is
//! caught by the pair disagreeing.
//!
//! Each channel's LDU1 names that call's own talkgroup and source. Matching them to the grant that
//! sent the follower there is the check that frames are attributed to the right channel.
//!
//! The control: the ordinary followed channel (T-269's) keys unframed 4FSK. It must yield **no**
//! voice frames at all, so "found an LDU" cannot be satisfied by a decoder that finds one anywhere.
//!
//! # What this test does NOT assert
//!
//! It does not assert what the call's `encryption` column says: that the ALGID decides it, and
//! what that means for a `VoicePermit`, is T-330's and asserted in `t330_algid`. This proves the
//! statement is read, recorded on the call's own `call-start` event in capture time, and correct.
//!
//! Metadata only: the IMBE codewords are skipped, `recordings == 0`, and nothing is decrypted.

use hk_core::Pacing;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::{Encryption, GrantKind, Timestamp};
use serde_json::{Value, json};

use crate::blind::{recording_start, replay_config, start};
use crate::common::*;

const T849: &str = "T-849";

/// Grant frequencies are integer arithmetic on decoded fields (T-268).
const MAP_TOLERANCE_HZ: f64 = 1.0;

/// How far a decoded frame's capture time may sit from where the scene put its first sync symbol.
///
/// A priori: the time comes from the demodulator's symbol index through the DDC's source map, so
/// its error is the timing phase within one symbol (208 µs at 4800 Bd) plus the discriminator's
/// integrate window — well under a millisecond. 5 ms is a tenth of a frame's 180 ms, so a frame
/// attributed one LDU early or late (180 ms out) cannot pass, and nor can a time taken from the
/// window's start or the call's.
const FRAME_TIME_TOL_S: f64 = 0.005;

/// Half an LDU (864 symbols at 4800 Bd), ns.
const LDU_HALF_NS: i64 = 90_000_000;

#[test]
fn t849_a_normal_run_reads_link_control_and_the_algid_from_a_followed_channels_voice_frames() {
    let out = synth_or_skip!(
        SynthRequest::new("trunk_voice_frames_control_channel")
            .seed(849)
            .param("duration_s", 2.0)
    );
    let fx = out.fixture(0).unwrap();
    let t0 = recording_start(&fx);

    let dir = TempDir::new("t849");
    let (cfg, dev) = replay_config(&dir.0, &fx.meta_path, json!({}), Pacing::Unpaced);
    let summary = finish(start(cfg, dev));

    let count = |p: &str| summary.counter(p);
    let (demods, ldu1, ldu2) = (
        count("/chains/cc_voice_frame_demods"),
        count("/chains/cc_ldu1"),
        count("/chains/cc_ldu2"),
    );
    eprintln!(
        "[{T849}] {} channel(s) followed, {demods} demodulated for voice frames: {ldu1} LDU1, \
         {ldu2} LDU2; {} call(s)",
        count("/chains/cc_follows"),
        count("/chains/cc_calls"),
    );

    // ---- Metadata only: reading a voice frame's header produces no audio and records nothing.
    assert_eq!(
        count("/chains/recordings"),
        0,
        "[{T849}] M4 is metadata-only: nothing may be recorded"
    );
    for suffix in [".wav", ".sigmf-data"] {
        assert!(
            files_with_suffix(&dir.0, suffix).is_empty(),
            "[{T849}] reading voice frames wrote {suffix} content"
        );
    }

    // ---- Truth, opened only now, and only to check the answer.
    let scenario = fx.scenario().unwrap();
    let t = &scenario.value["trunking"]["tsbk"];
    let vf = &t["voice_frames"];
    let want_nac = vf["nac"].as_u64().unwrap();
    let channels = vf["channels"].as_array().unwrap();
    assert_eq!(
        channels.len(),
        2,
        "[{T849}] the scene stages a clear and an encrypted call"
    );
    let want_plain_hz = t["follow_target_hz"].as_f64().unwrap();
    let s = |ns: i64| (ns - t0.as_unix_nanos()) as f64 * 1e-9;

    let repo = repo(&dir.0);
    let rows = repo.trunk_systems().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "[{T849}] exactly one control channel on file"
    );
    let system = &rows[0];
    let grants = repo
        .grants_for_system(system.id, Timestamp::UNIX_EPOCH, 10_000)
        .unwrap();
    let calls = repo.calls_for_system(system.id, 1000).unwrap();
    let starts_at = |hz: f64| -> Vec<&hk_model::GrantEvent> {
        grants
            .iter()
            .filter(|g| g.kind == GrantKind::CallStart)
            .filter(|g| g.f_hz.is_some_and(|f| (f - hz).abs() <= MAP_TOLERANCE_HZ))
            .collect()
    };

    for ch in channels {
        let name = ch["name"].as_str().unwrap();
        let hz = ch["target_hz"].as_f64().unwrap();
        let (want_tg, want_src) = (
            ch["talkgroup"].as_u64().unwrap(),
            ch["source"].as_u64().unwrap(),
        );
        let (want_algid, want_kid) = (
            ch["algid"].as_u64().unwrap(),
            ch["key_id"].as_u64().unwrap(),
        );
        let ldus = ch["ldus"].as_array().unwrap();
        let want_mis: Vec<&str> = ldus.iter().filter_map(|u| u["mi_hex"].as_str()).collect();
        let truth_start = |duid: &str| -> Vec<f64> {
            ldus.iter()
                .filter(|u| u["duid"] == duid)
                .map(|u| u["start_s"].as_f64().unwrap())
                .collect()
        };

        // The grant said nothing: service options 0 is `unknown`, never `clear`. So whatever the
        // voice frames say below, it did not come from the control channel.
        for g in grants.iter().filter(|g| {
            matches!(g.kind, GrantKind::Grant | GrantKind::GrantUpdate)
                && g.f_hz.is_some_and(|f| (f - hz).abs() <= MAP_TOLERANCE_HZ)
        }) {
            assert_eq!(
                g.encryption,
                Encryption::Unknown,
                "[{T849}] the {name} channel's grant claimed {:?}; the scene's grants state nothing",
                g.encryption
            );
        }

        let opened = starts_at(hz);
        assert!(
            !opened.is_empty(),
            "[{T849}] no call was followed onto the {name} channel at {:.6} MHz",
            hz / 1e6
        );
        let frames: Vec<&Value> = opened.iter().map(|g| &g.detail["voice_frames"]).collect();
        eprintln!("[{T849}] {name} @ {:.6} MHz: {}", hz / 1e6, frames[0]);
        assert!(
            frames.iter().all(|v| v["attempted"] == true),
            "[{T849}] the {name} channel was followed but its voice frames were never read: \
             {frames:?}"
        );

        // ---- LDU1: the call's own link control names the grant's talkgroup and source.
        let lcs: Vec<&Value> = frames
            .iter()
            .flat_map(|v| v["link_control"].as_array().into_iter().flatten())
            .collect();
        assert!(
            !lcs.is_empty(),
            "[{T849}] no LDU1 link control decoded on the {name} channel: {frames:?}"
        );
        for lc in &lcs {
            assert_eq!(lc["lco"], 0, "[{T849}] {name}: not a group-voice LC: {lc}");
            assert_eq!(
                lc["talkgroup"], want_tg,
                "[{T849}] {name}: LC talkgroup: {lc}"
            );
            assert_eq!(lc["source"], want_src, "[{T849}] {name}: LC source: {lc}");
            assert_eq!(lc["nac"], want_nac, "[{T849}] {name}: NAC: {lc}");
            let at = s(lc["t_ns"].as_i64().unwrap());
            assert!(
                truth_start("ldu1")
                    .iter()
                    .any(|w| (at - w).abs() <= FRAME_TIME_TOL_S),
                "[{T849}] {name}: an LDU1 placed at {at:.4} s, but the scene's LDU1s start at {:?}",
                truth_start("ldu1")
            );
        }

        // ---- LDU2: THE ASSERTION THIS TICKET EXISTS FOR. The call's own ALGID and key id, from
        // IQ, through the device — and the two channels' answers differ exactly as the truth does.
        let ess: Vec<&Value> = frames
            .iter()
            .flat_map(|v| v["encryption_sync"].as_array().into_iter().flatten())
            .collect();
        assert!(
            !ess.is_empty(),
            "[{T849}] no LDU2 encryption sync decoded on the {name} channel: {frames:?}"
        );
        for es in &ess {
            assert_eq!(
                es["algid"], want_algid,
                "[{T849}] {name}: the ALGID read off the air is wrong: {es}"
            );
            assert_eq!(es["key_id"], want_kid, "[{T849}] {name}: key id: {es}");
            assert!(
                want_mis.contains(&es["mi_hex"].as_str().unwrap_or_default()),
                "[{T849}] {name}: MI {} is none the scene sent ({want_mis:?})",
                es["mi_hex"]
            );
            let at = s(es["t_ns"].as_i64().unwrap());
            assert!(
                truth_start("ldu2")
                    .iter()
                    .any(|w| (at - w).abs() <= FRAME_TIME_TOL_S),
                "[{T849}] {name}: an LDU2 placed at {at:.4} s, but the scene's LDU2s start at {:?}",
                truth_start("ldu2")
            );
        }
        let want_name = if want_algid == 0x80 {
            "clear"
        } else {
            "AES-256"
        };
        assert!(
            ess.iter().all(|es| es["algid_name"] == want_name),
            "[{T849}] {name}: ALGID not named {want_name}: {ess:?}"
        );

        // ---- One shared time axis: every frame sits inside the call it is recorded on — by its
        // midpoint, since the call's start is measured from the envelope in 2 ms steps and a
        // keying's first LDU can begin a step before it (see `VoiceFrames::detail`).
        for ev in &opened {
            let call = calls
                .iter()
                .find(|c| Some(c.id) == ev.call)
                .expect("a call-start event names its call");
            assert_eq!(
                call.talkgroup.as_deref(),
                Some(want_tg.to_string().as_str()),
                "[{T849}] {name}: the call is not the granted talkgroup's"
            );
            let (lo, hi) = (
                call.t_start.as_unix_nanos(),
                call.t_end
                    .or(call.observed_until)
                    .map_or(i64::MAX, Timestamp::as_unix_nanos),
            );
            let v = &ev.detail["voice_frames"];
            for f in v["link_control"]
                .as_array()
                .into_iter()
                .chain(v["encryption_sync"].as_array())
                .flatten()
            {
                let at = f["t_ns"].as_i64().unwrap() + LDU_HALF_NS;
                assert!(
                    (lo..hi).contains(&at),
                    "[{T849}] {name}: a frame whose midpoint is {:.4} s is recorded on a call \
                     spanning {:.4}..{:.4} s",
                    s(at),
                    s(lo),
                    s(hi.min(i64::MAX / 2))
                );
            }
        }
    }

    // ---- The two calls' ALGIDs differ, and the pair is exactly the truth's: one clear, one not.
    let algids: Vec<u64> = channels
        .iter()
        .map(|c| c["algid"].as_u64().unwrap())
        .collect();
    assert!(algids.contains(&0x80) && algids.iter().any(|&a| a != 0x80));

    // ---- The control: unframed 4FSK on the ordinary followed channel yields no voice frame.
    let plain = starts_at(want_plain_hz);
    assert!(
        !plain.is_empty(),
        "[{T849}] the ordinary followed channel produced no call, so the control tests nothing"
    );
    for ev in plain {
        let v = &ev.detail["voice_frames"];
        assert_eq!(
            v["attempted"], true,
            "[{T849}] control channel not read: {v}"
        );
        assert_eq!(
            (v["ldu1"].as_u64(), v["ldu2"].as_u64()),
            (Some(0), Some(0)),
            "[{T849}] unframed 4FSK yielded voice frames: {v}"
        );
    }
    assert!(
        ldu1 >= 2 && ldu2 >= 2 && demods >= 3,
        "[{T849}] counters disagree with the rows: {demods} demods, {ldu1} LDU1, {ldu2} LDU2"
    );
}
