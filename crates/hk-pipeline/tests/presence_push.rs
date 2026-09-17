//! T-388, end to end through the real pipeline: a live signal's box top tracks the live edge, and
//! **stops when the emission stops**.
//!
//! The user's report, from live testing on 2026-09-16: *"a live signal box extends UP slowly."* The
//! chain that made it slow was two lazy links in series — an open track reached the inventory every
//! `LIVE_OFFER_NS` (5 s) and the UI polled `/api/inventory` every 5 s — so a box top sat 5–10 s
//! behind. This test drives the whole thing over a keyed tone and asserts on the records the
//! `presence` stream actually published, not on a helper.
//!
//! Two properties, and the second is the one that a latency test alone would miss:
//!
//! 1. **Latency, with numbers.** Successive extensions of the same emitter are no more than
//!    `MAX_STEP_S` apart in observed time, and the last one reaches within `MAX_LAG_S` of the last
//!    instant the tone was actually on the air. Both are ~1 s, against the 5–10 s before.
//! 2. **A stopped emission stops extending.** The tone is keyed off with a third of the recording
//!    still to run. **No record may name a time after it stopped** — not one, however many ticks the
//!    stream keeps taking. A push that kept the box growing to the live edge would be a claim about
//!    air nobody measured, and that is the failure this asserts against.

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use common::*;
use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
use hk_pipeline::{PRESENCE_EXTENSION_KIND, PRESENCE_MESSAGE_SCHEMA, PRESENCE_STREAM_ID};
use hk_stream::{Declared, Record, StreamKind, StreamReader};
use serde_json::json;

const FS: f64 = 1e6;
const CENTER: f64 = 433.92e6;
/// Recording length, s.
const RUN_S: f64 = 12.0;
/// The tone is keyed off here and never comes back.
const STOPS_AT_S: f64 = 8.0;
/// Key period: on for half of it. Enough bursts before `STOPS_AT_S` for the live offer's
/// `LIVE_MIN_BURSTS`, and slow enough that each is many STFT frames long.
const KEY_PERIOD_S: f64 = 1.0;

/// Longest step between successive observed ends of one emitter, s. The push period is 250 ms of
/// stream time and the detect flush that feeds it is 500 ms, so a step is a flush; the keying's own
/// off-half is what makes this a second rather than a flush.
const MAX_STEP_S: f64 = 1.5;
/// Longest lag of the final extension behind the last instant the tone was on the air, s.
const MAX_LAG_S: f64 = 1.0;
/// Slack past `STOPS_AT_S` a detection may legitimately claim: one detector frame's worth of
/// trailing energy. Anything beyond this is the box drawing ahead of the evidence.
const STOP_SLACK_S: f64 = 0.35;

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A keyed tone: on for the first half of every `KEY_PERIOD_S`, and off for good after
/// `STOPS_AT_S`. Bursts, so the tracker's live offer (which wants several) fires while the track is
/// still open — the path a repeating emitter takes into the inventory.
fn keyed_tone(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let n = (RUN_S * FS) as usize;
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut noise = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        ((state >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 12.0
    };
    let mut data = Vec::with_capacity(2 * n);
    for i in 0..n {
        let t = i as f64 / FS;
        let on = t < STOPS_AT_S && (t % KEY_PERIOD_S) < KEY_PERIOD_S / 2.0;
        let a = if on { 45.0 } else { 0.0 };
        let ph = 2.0 * std::f64::consts::PI * 120e3 * i as f64 / FS;
        let re = (a * ph.cos() + noise()).round().clamp(-128.0, 127.0) as i8;
        let im = (a * ph.sin() + noise()).round().clamp(-128.0, 127.0) as i8;
        data.push(re as u8);
        data.push(im as u8);
    }
    std::fs::write(dir.join("keyed.sigmf-data"), data).unwrap();
    let mut meta = SigmfMeta::new(Datatype::Ci8);
    meta.global.sample_rate = Some(FS);
    meta.captures.push(Capture {
        sample_start: 0,
        frequency: Some(CENTER),
        datetime: Some("2026-09-13T12:00:00Z".into()),
        provenance: None,
        clip_count: None,
        extra: serde_json::Map::new(),
    });
    let path = dir.join("keyed.sigmf-meta");
    meta.write(&path).unwrap();
    path
}

/// One published extension, as a reader sees it.
#[derive(Debug)]
struct Seen {
    emitter: String,
    t_start_s: f64,
    t_end_s: f64,
    t_ns: i64,
}

#[test]
fn a_live_signals_box_tracks_the_live_edge_and_stops_when_the_emission_does() {
    let dir = TempDir::new("t388");
    let meta = keyed_tone(&dir.0.join("src"));
    let (mut cfg, replay) = replay_config(&dir.0, &meta, json!({}), hk_core::Pacing::Unpaced);
    let t0_ns = replay.info.start_time.as_unix_nanos();

    let consumer = Buf::default();
    let sink_buf = consumer.clone();
    let header_seen = Arc::new(Mutex::new(None));
    let hdr = Arc::clone(&header_seen);
    cfg.stream_sink = Some(Arc::new(move |h, handle| {
        if h.stream_id == PRESENCE_STREAM_ID {
            *hdr.lock().unwrap() = Some((h.kind, h.message_schema.clone()));
            handle
                .subscribe(
                    "t388-consumer",
                    Declared::local(sink_buf.clone()),
                    Box::new(|_| {}),
                )
                .unwrap();
        }
    }));

    let s = start(cfg, replay).wait().unwrap();
    assert!(s.errors.is_empty(), "{:?}", s.errors);

    // The stream is offered, and it is what the contract says it is (§15).
    let (kind, schema) = header_seen
        .lock()
        .unwrap()
        .clone()
        .expect("the presence stream was never offered");
    assert_eq!(kind, StreamKind::Messages);
    assert_eq!(schema.as_deref(), Some(PRESENCE_MESSAGE_SCHEMA));

    // ---- read what was actually published ----
    let bytes = consumer.0.lock().unwrap().clone();
    let mut reader = StreamReader::new(std::io::Cursor::new(bytes));
    let mut seen: Vec<Seen> = Vec::new();
    while let Some(r) = reader.next_record().unwrap() {
        let Record::Message(m) = r else {
            continue; // a drop marker is not an extension
        };
        let v = m.value;
        if v["type"] != "message" {
            continue;
        }
        assert_eq!(v["metadata"]["kind"], json!(PRESENCE_EXTENSION_KIND));
        assert_eq!(v["frame_model"], json!(PRESENCE_EXTENSION_KIND));
        assert_eq!(v["gated"], json!(false));
        assert!(v.get("content").is_none() || v["content"].is_null());
        let iv = &v["metadata"]["last_interval"];
        seen.push(Seen {
            emitter: v["emitter_id"].as_str().unwrap().to_owned(),
            t_start_s: iv["t_start_s"].as_f64().unwrap(),
            t_end_s: iv["t_end_s"].as_f64().unwrap(),
            t_ns: v["t_ns"].as_i64().unwrap(),
        });
        assert_eq!(iv["open"], json!(true), "an extension is an open interval");
    }
    assert!(
        !seen.is_empty(),
        "no presence extension was published at all — the box would still be poll-gated"
    );
    eprintln!("[T-388] {} extensions published", seen.len());

    // T-354: the envelope's `t_ns` is the same instant as `t_end_s`, in integer Unix nanoseconds.
    for e in &seen {
        assert!(
            (e.t_ns as f64 * 1e-9 - e.t_end_s).abs() < 1e-6,
            "t_ns and t_end_s disagree: {} vs {}",
            e.t_ns,
            e.t_end_s
        );
        assert!(e.t_end_s >= e.t_start_s);
    }

    let rel = |t_s: f64| t_s - t0_ns as f64 * 1e-9;

    // ---- (2) the control that matters: nothing is claimed after the tone stopped ----
    let worst = seen
        .iter()
        .map(|e| rel(e.t_end_s))
        .fold(f64::NEG_INFINITY, f64::max);
    assert!(
        worst <= STOPS_AT_S + STOP_SLACK_S,
        "a box extended to {worst:.3} s, past the {STOPS_AT_S} s the emission stopped: the push \
         drew ahead of what was observed"
    );
    // And the run really did keep going afterwards, so the assertion above had something to catch:
    // the stream had a third of the recording left to publish a later time in and did not.
    const {
        assert!(RUN_S - STOPS_AT_S > 3.0);
    }

    // ---- (1) latency: the busiest emitter's box tracked the live edge ----
    let busiest = {
        let mut counts = std::collections::HashMap::<&str, usize>::new();
        for e in &seen {
            *counts.entry(e.emitter.as_str()).or_default() += 1;
        }
        counts
            .into_iter()
            .max_by_key(|&(_, n)| n)
            .map(|(id, _)| id.to_owned())
            .unwrap()
    };
    let ends: Vec<f64> = seen
        .iter()
        .filter(|e| e.emitter == busiest)
        .map(|e| rel(e.t_end_s))
        .collect();
    assert!(
        ends.len() >= 4,
        "one emitter should be extended repeatedly while it keys, got {}",
        ends.len()
    );
    let mut step = 0.0f64;
    for w in ends.windows(2) {
        assert!(w[1] >= w[0], "an observed end went backwards: {w:?}");
        step = step.max(w[1] - w[0]);
    }
    let last = *ends.last().unwrap();
    eprintln!(
        "[T-388] busiest emitter: {} extensions, worst step {step:.3} s, last end {last:.3} s \
         (tone stops at {STOPS_AT_S} s)",
        ends.len()
    );
    assert!(
        step <= MAX_STEP_S,
        "the box top advanced in steps of {step:.3} s — the poll it replaces was 5 s, the target ~1 s"
    );
    assert!(
        STOPS_AT_S - last <= MAX_LAG_S,
        "the box top ended {:.3} s behind the last air, not the ~1 s target",
        STOPS_AT_S - last
    );
}
