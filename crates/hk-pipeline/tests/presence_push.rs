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
//!    has not watched long enough to call an absence. Eleven key-offs must not become eleven
//!    intervals — and none may be closed-then-revoked either, which is the same defect one tick
//!    short of being visible in the END count.
//!
//!    **The silence a tick actually reads is not the key-off** — it is the gap between consecutive
//!    burst *completions*, because a track's measured end does not advance while a burst is in
//!    flight. T-449 is the ticket for having got that wrong: T-410 set the keying period to exactly
//!    `MIN_IDLE_GAP_S`, so the fixture's entire margin was the detector's ~17 ms burst-landing lag
//!    and the test failed roughly one run in five. See `MAX_KEYING_SILENCE_S`.
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
use hk_model::MIN_IDLE_GAP_S;
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
/// Key period: on for `KEY_ON_S` of it, silent for the rest. Enough bursts before `STOPS_AT_S` for
/// the live offer's `LIVE_MIN_BURSTS`, and each many STFT frames long.
const KEY_PERIOD_S: f64 = 0.75;
/// On-time inside each period. The key-off is `KEY_PERIOD_S - KEY_ON_S` = 0.5 s — a real observed
/// silence, and comfortably inside the 1 s idle floor, which is what property (2) is about.
const KEY_ON_S: f64 = 0.25;

/// **The quantity the idle floor actually judges, and the reason T-449 existed.**
///
/// A [`LiveExtent`](hk_detect::LiveExtent)'s silence is `now - t_last_end`, and `t_last_end` is the
/// end of the last burst **whose detection has reached the tracker** — it does not advance while a
/// burst is in flight. So the silence a presence tick reads during steady keying is not the
/// key-off: it is the interval between consecutive burst *completions*, `KEY_PERIOD_S`, plus
/// however long the newest burst is still in flight when the tick lands.
///
/// T-410 built this fixture with `KEY_PERIOD_S = 1.0` believing the judged quantity was the 0.5 s
/// key-off. It was `1.0` — **exactly `MIN_IDLE_GAP_S`** — so the test's whole margin was the
/// detector's burst-landing lag, and a tick landing inside it read **1.016 s** of silence while the
/// tone was plainly still keying. That published an END, which the next tick correctly REVOKEd; the
/// test then counted the REVOKE as a second opening and failed. Whether any tick landed there is
/// wall-clock luck: the writer coalesces batches, so the tick grid's phase shifts run to run — the
/// traced tick count varies **14–18** over the same recording on the same machine. That is the
/// 1-in-5. The product was right at every step; the fixture had put the emission's keying period on
/// the threshold and then asserted the decision always fell one way.
///
/// Measured here with the periods below, over 8 traced runs: the largest silence any tick read
/// while the tone was still keying was **0.781 s** (identical in all 8 — it is `KEY_PERIOD_S` plus
/// a ~31 ms burst-landing lag, not a sample of the machine), against the 1.0 s floor, and the first
/// silence read as closed was 1.283 s. The assertion below is the guard that stops the coincidence
/// coming back.
const MAX_KEYING_SILENCE_S: f64 = 0.85;
const _: () = assert!(KEY_ON_S < KEY_PERIOD_S);
/// The guard itself, and the one that would not compile under T-410's constants: the worst silence
/// a tick can read while the tone is still keying is at least `KEY_PERIOD_S`, and it must land
/// **inside** the floor that closes an interval — not on it.
const _: () = assert!(KEY_PERIOD_S < MAX_KEYING_SILENCE_S && MAX_KEYING_SILENCE_S < MIN_IDLE_GAP_S);

/// The last instant the tone is actually on the air: the end of the last burst that starts before
/// `STOPS_AT_S`, truncated by it. Computed from the same expression [`keyed_tone`] keys on, so the
/// bound below is measured against the emission rather than against `STOPS_AT_S`, which the tone
/// has already been silent for part of.
fn last_burst_end_s() -> f64 {
    let k = ((STOPS_AT_S / KEY_PERIOD_S).ceil() as i64 - 1).max(0) as f64;
    (k * KEY_PERIOD_S + KEY_ON_S).min(STOPS_AT_S)
}

/// Longest lag of the closing record's measured end from [`last_burst_end_s`], s — **in either
/// direction**. The END names the *measured* end, so this bounds both how much of a real emission a
/// capped box can lose and how far past the evidence it may reach.
///
/// It is small on purpose, and measured: over 40 runs (20 isolated, 20 under load) the END landed
/// **2 ms** past the last burst's true end, every run. The defects it is here to catch all miss by
/// far more — an END that names the decision instant lands ~1.3 s late, one published on a key-off
/// lands at least `KEY_PERIOD_S` early, and the plausible-looking "now, less one idle gap" lands
/// 0.28 s late, which is inside `STOP_SLACK_S` and so caught by nothing else.
const MAX_LAG_S: f64 = 0.1;
/// Slack past [`last_burst_end_s`] a record may legitimately claim: trailing energy the detector
/// measured past the burst's ideal edge. Anything beyond this is the box drawing ahead of the
/// evidence. Measured over 40 runs: the furthest any record reached was **2 ms** past it. The
/// number is T-410's, kept; what changed is its subject — it was measured from `STOPS_AT_S`, which
/// the tone has already been silent for a quarter of a second by.
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
        let on = t < STOPS_AT_S && (t % KEY_PERIOD_S) < KEY_ON_S;
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
        worst <= last_burst_end_s() + STOP_SLACK_S,
        "a record named {worst:.3} s, past the {:.3} s the emission was last on the air: the \
         stream published a presumption instead of the measured end",
        last_burst_end_s()
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
    // **An opening record is one that draws a NEW box**, which is START or REOPEN and never REVOKE:
    // a revocation re-opens the interval an END capped, on the same row and with the same
    // `t_start_s`, so one box grows rather than a second appearing (`presence.rs`, T-413). Counting
    // it by the wire's `open` flag — which a REVOKE also sets, because the interval *is* open again
    // — read a correct revocation as a second opening. That is half of why T-449 failed: the other
    // half (the fixture keying on the threshold) is why there was a revocation to miscount.
    let opens = mine
        .iter()
        .filter(|e| e.kind == PRESENCE_START_KIND || e.kind == PRESENCE_REOPEN_KIND)
        .count();
    let revokes = mine
        .iter()
        .filter(|e| e.kind == PRESENCE_REVOKE_KIND)
        .count();
    let ends: Vec<&&Seen> = mine
        .iter()
        .filter(|e| e.kind == PRESENCE_END_KIND)
        .collect();
    eprintln!(
        "[T-410] {} endpoints published over {} emitters; the keyed emitter: {opens} opening, {} \
         closing, {revokes} revoked (tone keys {KEY_ON_S} s on / {KEY_PERIOD_S} s, stops at \
         {STOPS_AT_S} s)",
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

    // (2) The key-offs are shorter than the idle floor, so they close nothing — and the quantity
    // that has to be shorter is `KEY_PERIOD_S` plus the burst in flight, not the key-off (see
    // `MAX_KEYING_SILENCE_S`). Eleven of them must not become eleven intervals.
    assert!(
        ends.len() <= 1,
        "the keying's own off-halves closed the interval {} times: {mine:?}",
        ends.len()
    );
    // …and not one END was published-and-withdrawn either. A REVOKE here is the *near miss* of the
    // same defect: the interval was closed mid-keying and the resumption correctly re-opened it, so
    // the counted END total stays 1 and property (2) alone cannot see it. Under contract B that
    // costs a box that visibly caps and un-caps while the emitter is plainly on the air.
    assert_eq!(
        revokes, 0,
        "an END was published while the tone was still keying and then withdrawn: {mine:?}"
    );

    // (3) And when it did end, it ended where the tone actually stopped — measured against the last
    // burst's true end, not against `STOPS_AT_S`, which the tone has already been silent for part
    // of.
    let end = ends.first().expect(
        "the emission stopped with a third of the recording left and the interval never closed — \
         under ADR-0019 that box runs to the live edge over four seconds of silent air",
    );
    let capped = rel(end.t_end_s);
    let truth = last_burst_end_s();
    eprintln!(
        "[T-410] closed at {capped:.3} s against a last burst ending at {truth:.3} s (lag {:.3} s)",
        capped - truth
    );
    assert!(
        (truth - capped).abs() <= MAX_LAG_S,
        "the box capped at {capped:.3} s, not within {MAX_LAG_S} s of the {truth:.3} s the tone \
         was last on the air"
    );
}
