//! T-967 (SIGNAL-062): the inventory list's decoded-identity label for an RDS station is the
//! decoder's **voted** session label — the most frequent PS frame of the latest committed session
//! — and its own frame share, read back from rows the **real** writer (`write_session` →
//! `rds_decodes`) committed. Never the newest per-frame fragment: a station scrolling
//! song/artist text through PS (the SF stations of T-926) sends many fragments, one of which is
//! always the latest row.
//!
//! The session is a real `AnalogReceiver` run (on noise, which is cheap) whose RDS report is then
//! replaced with a scripted one, so the frame log is exact and the rows are the writer's own.

mod common;

use common::*;
use hk_demod::{
    AnalogReceiver, AnalogSession, MpxTimeMap, RecordContext, WfmReport, write_session,
};
use hk_estimate::SnippetRequest;
use hk_model::{AnnotationTarget, DecodedIdentity, IdentityScheme, Repository, Timestamp};
use serde_json::json;

const FS: f64 = 250e3;
const PI: u16 = 0x1694;

/// A real receiver session over 1 s of noise, then given `frames` (in air order) as its RDS
/// report, starting at host time `t0_s`.
fn session_with_ps(frames: &[&str], t0_s: i64) -> AnalogSession {
    let iq = noise_only(FS, 1.0);
    let prov = provenance(100e6, FS);
    let req = SnippetRequest {
        start_index: 0,
        end_index: iq.len() as u64,
        center_offset_hz: 50e3,
        bandwidth_hz: 150e3,
    };
    let mut s = AnalogReceiver::default()
        .run(info(0, &prov), &iq, &req)
        .unwrap();
    s.anchor.host_time = Timestamp::from_unix_nanos(t0_s * 1_000_000_000);
    s.mpx_time = Some(MpxTimeMap {
        source_index0: 0.0,
        source_per_output: 1.0,
    });
    // Counts, most frequent first, ties first-seen first — `RdsReport::ps_frames`' own order.
    let mut counts: Vec<(String, u32)> = Vec::new();
    for f in frames {
        match counts.iter_mut().find(|(t, _)| t == f) {
            Some((_, n)) => *n += 1,
            None => counts.push(((*f).to_owned(), 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1));
    let step = FS * 0.9 / frames.len() as f64;
    let frame_log: Vec<_> = frames
        .iter()
        .enumerate()
        .map(|(k, text)| json!({"position": 1000.0 + k as f64 * step, "pi": PI, "text": text}))
        .collect();
    let wfm: WfmReport = serde_json::from_value(json!({
        "mpx_rate_hz": FS, "mpx_samples": iq.len(), "carrier_offset_hz": 0.0,
        "peak_deviation_hz": 75e3,
        "pilot": {"present": true, "locked": true, "locked_fraction": 1.0, "frequency_hz": 19e3,
                  "sigma_hz": 0.1, "deviation_hz": 7e3, "lock_quality": 0.99,
                  "phase_error_rms_rad": 0.05, "first_lock_s": 0.01},
        "stereo": true, "deemphasis_tau_s": 75e-6, "audio_rate_hz": 48e3,
        "rds": {
            // T-962: a committed PI (97 in-window votes, far over the commit bar); a
            // provisional one is not an identity and would carry no label.
            "pi": {"pi": PI, "votes": 97, "total_votes": 100, "share": 0.97,
                   "window_votes": 97, "provisional": false},
            "pi_abstain": null, "pi_votes": {"1694": 97, "1695": 3},
            "ps_frames": counts, "frame_log": frame_log,
            "pty": 10, "tp": true, "ta": false, "group_types": {"0A": 40, "2A": 20},
            "bits": 12000, "blocks_total": 400, "blocks_ok": 396, "block_error_rate": 0.01,
            "groups_total": 100, "groups_ok": 97, "group_error_rate": 0.03,
            "groups_rejected": 0, "sync_acquisitions": 1, "sync_losses": 0, "bit_slips": 0,
        },
        "rds_timing_contrast": 3.0,
    }))
    .unwrap();
    s.wfm = Some(wfm);
    s
}

#[test]
fn t_967_rds_label_is_the_voted_session_ps_not_the_latest_scrolling_fragment() {
    let mut repo = Repository::open_in_memory().unwrap();
    let pi = DecodedIdentity {
        scheme: IdentityScheme::RdsPi,
        value: "1694".into(),
    };
    // Session 1: the station name six times, a song/artist scroll five times, ending on a
    // fragment — 11 PS frames, more than any "recent N rows" window.
    let first = [
        "KROQ    ", "NOW PLAY", "KROQ    ", "ING: SON", "KROQ    ", "G TITLE ", "KROQ    ",
        "BY ARTIS", "KROQ    ", "KROQ    ", "T NAME  ",
    ];
    let s1 = session_with_ps(&first, 1_789_000_000);
    let w1 = write_session(&mut repo, &s1, &RecordContext::default()).unwrap();
    assert_eq!(
        w1.decode_ids.len(),
        1 + first.len(),
        "one summary row + one row per frame"
    );
    let sum = repo
        .latest_decode_identity_summary(&pi)
        .unwrap()
        .expect("an RDS identity with a voted PS has a summary");
    assert_eq!(sum.label, "KROQ", "the most frequent PS, trimmed: {sum:?}");
    assert_eq!(
        sum.label_share,
        Some(6.0 / 11.0),
        "the label's own share of the session's PS frames: {sum:?}"
    );

    // Session 2, 30 s later: nine station-name frames and a scroll, again ending on a fragment.
    // The label follows the LATEST session's vote and share.
    let second = [
        "KROQ    ", "KROQ    ", "KROQ    ", "KROQ    ", "KROQ    ", "KROQ    ", "KROQ    ",
        "KROQ    ", "KROQ    ", "T NAME  ",
    ];
    let s2 = session_with_ps(&second, 1_789_000_030);
    let w2 = write_session(&mut repo, &s2, &RecordContext::default()).unwrap();
    let sum = repo.latest_decode_identity_summary(&pi).unwrap().unwrap();
    assert_eq!(sum.label, "KROQ", "{sum:?}");
    assert_eq!(sum.label_share, Some(0.9), "{sum:?}");

    // The served share is the same figure the decoder's own Label annotation states as its
    // confidence — one definition, not two.
    let e = repo.emitter_by_identity(&pi).unwrap().expect("emitter");
    let labels = repo
        .annotations_for(&AnnotationTarget::Emitter(e.id))
        .unwrap();
    let latest = labels
        .iter()
        .find(|a| Some(a.id) == w2.label_id)
        .expect("session 2's label annotation");
    assert_eq!(latest.value, sum.label);
    assert_eq!(Some(latest.confidence), sum.label_share);
}
