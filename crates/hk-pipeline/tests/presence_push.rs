//! T-410 (was T-388), end to end through the real pipeline: the `presence` stream publishes an
//! interval's **endpoints**, nothing while it continues, and it **caps at the measured end** when
//! the emission stops.
//!
//! T-388 built this stream under contract A — a record per open emitter per tick pushing the box's
//! measured top forward. ADR-0019 replaced it with contract B: the box runs to the live edge and
//! caps only on a detected END, so the stream carries START / END / REOPEN and **never a per-poll
//! presence bump**. This test drives the whole thing over a keyed tone and asserts on the records
//! actually published, not on a helper.
//!
//! Three properties, over one 12 s recording whose tone keys off for good at 8 s:
//!
//! 1. **Endpoints only.** A continuing interval says nothing. The keyed emitter produces **one**
//!    opening record and **one** closing record, not one per tick — which is what makes the stream
//!    silent over a band of steady carriers.
//! 2. **A silence shorter than the idle gap is not an end.** The tone is off for 0.5 s between
//!    bursts, inside the 1 s floor, so those gaps close nothing: the receiver was watching, but it
//!    has not watched long enough to call an absence. Eight key-offs must not become eight
//!    intervals.
//! 3. **The END caps at the MEASURED end.** The closing record names the last instant the tone was
//!    actually on the air — not the instant the end was decided, and not `now`. This is what makes
//!    the box *retract* to the truth rather than stop wherever the assumption had reached, and it
//!    is the assertion that keeps contract B honest: with a third of the recording still to run,
//!    **no record may name a time after the emission stopped**.

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use common::*;
use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
use hk_pipeline::{
    PRESENCE_END_KIND, PRESENCE_MESSAGE_SCHEMA, PRESENCE_REOPEN_KIND, PRESENCE_REVOKE_KIND,
    PRESENCE_START_KIND, PRESENCE_STREAM_ID,
};
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

/// Longest lag of the closing record's measured end behind the last instant the tone was on the
/// air, s. The END names the measured end, so this bounds how much of a real emission a capped box
/// can lose — and, equally, that the 0.5 s key-offs did not close the interval early.
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

/// One published endpoint, as a reader sees it.
#[derive(Debug)]
struct Seen {
    kind: String,
    emitter: String,
    t_start_s: f64,
    t_end_s: f64,
    t_ns: i64,
    open: bool,
}

#[test]
fn a_live_signals_box_opens_once_and_caps_at_the_measured_end() {
    let dir = TempDir::new("t410");
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
                    "t410-consumer",
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
            continue; // a drop marker is not an endpoint
        };
        let v = m.value;
        if v["type"] != "message" {
            continue;
        }
        let kind = v["metadata"]["kind"].as_str().unwrap().to_owned();
        assert!(
            [
                PRESENCE_START_KIND,
                PRESENCE_REOPEN_KIND,
                PRESENCE_END_KIND,
                PRESENCE_REVOKE_KIND,
            ]
            .contains(&kind.as_str()),
            "unknown record kind on the presence stream: {kind}"
        );
        assert_eq!(v["frame_model"], json!(kind));
        assert_eq!(v["gated"], json!(false));
        assert!(v.get("content").is_none() || v["content"].is_null());
        let iv = &v["metadata"]["last_interval"];
        seen.push(Seen {
            open: iv["open"].as_bool().unwrap(),
            emitter: v["emitter_id"].as_str().unwrap().to_owned(),
            t_start_s: iv["t_start_s"].as_f64().unwrap(),
            t_end_s: iv["t_end_s"].as_f64().unwrap(),
            t_ns: v["t_ns"].as_i64().unwrap(),
            kind,
        });
    }
    assert!(
        !seen.is_empty(),
        "no presence endpoint was published at all — the box would still be poll-gated"
    );

    // T-354: the envelope's `t_ns` is the instant the record is about, in integer Unix nanoseconds
    // — the interval's start for an opening record, its measured end for a closing one.
    for e in &seen {
        // An opening record is about its start; an END and a REVOKE are both about the measured
        // edge the box lands on (T-413).
        let opening = e.kind == PRESENCE_START_KIND || e.kind == PRESENCE_REOPEN_KIND;
        let about = if opening { e.t_start_s } else { e.t_end_s };
        assert!(
            (e.t_ns as f64 * 1e-9 - about).abs() < 1e-6,
            "t_ns disagrees with the endpoint it names: {} vs {about}",
            e.t_ns
        );
        assert_eq!(e.open, e.kind != PRESENCE_END_KIND);
        assert!(e.t_end_s >= e.t_start_s);
    }

    let rel = |t_s: f64| t_s - t0_ns as f64 * 1e-9;

    // ---- (3) the control that matters: nothing is claimed after the tone stopped ----
    let worst = seen
        .iter()
        .map(|e| rel(e.t_end_s))
        .fold(f64::NEG_INFINITY, f64::max);
    assert!(
        worst <= STOPS_AT_S + STOP_SLACK_S,
        "a record named {worst:.3} s, past the {STOPS_AT_S} s the emission stopped: the stream \
         published a presumption instead of the measured end"
    );
    // And the run really did keep going afterwards, so the assertion above had something to catch.
    const {
        assert!(RUN_S - STOPS_AT_S > 3.0);
    }

    // ---- (1) + (2): the keyed emitter opens once, closes once, and caps at the measured end ----
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
    let mine: Vec<&Seen> = seen.iter().filter(|e| e.emitter == busiest).collect();
    let opens = mine.iter().filter(|e| e.open).count();
    let ends: Vec<&&Seen> = mine.iter().filter(|e| !e.open).collect();
    eprintln!(
        "[T-410] {} endpoints published over {} emitters; the keyed emitter: {opens} opening, {} \
         closing (tone keys 0.5 s off {KEY_PERIOD_S} s, stops at {STOPS_AT_S} s)",
        seen.len(),
        seen.iter()
            .map(|e| e.emitter.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        ends.len(),
    );

    // (1) Endpoints only. Under contract A this emitter alone produced one record per tick for the
    // eight seconds it was keying; under contract B a continuing interval is not news.
    assert_eq!(opens, 1, "the interval opened more than once: {mine:?}");

    // (2) The 0.5 s key-offs are inside the 1 s idle floor, so they close nothing. Eight of them
    // must not become eight intervals — a silence shorter than the gap is not evidence of absence.
    assert!(
        ends.len() <= 1,
        "the keying's own off-halves closed the interval {} times: {mine:?}",
        ends.len()
    );

    // (3) And when it did end, it ended where the tone actually stopped.
    let end = ends.first().expect(
        "the emission stopped with a third of the recording left and the interval never closed — \
         under ADR-0019 that box runs to the live edge over four seconds of silent air",
    );
    let capped = rel(end.t_end_s);
    eprintln!("[T-410] closed at {capped:.3} s against a {STOPS_AT_S} s stop");
    assert!(
        (STOPS_AT_S - capped).abs() <= MAX_LAG_S,
        "the box capped at {capped:.3} s, not within {MAX_LAG_S} s of the {STOPS_AT_S} s stop"
    );
}
