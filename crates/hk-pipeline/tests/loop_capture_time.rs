//! T-474: a `--loop` replay presents ONE monotone capture-time axis, to a real consumer.
//!
//! The decision this pins is stated in `hk_pipeline::capture`'s `Axis`: a loop is a new *pass*
//! spliced onto the same axis, not a new epoch and never a jump backwards. The reason it is a
//! correctness claim rather than a cosmetic one is downstream: `hk-store`'s spectrum-history
//! pyramid keeps one forward-only watermark and answers a frame for an already-sealed tile with
//! `IngestOutcome::Late` — counted, never errored. A clock that wrapped would therefore drop rows
//! *silently*, with every suite still green, and the client's live edge (which clamps monotone,
//! `ui/src/surface/preview.ts`) would sit ahead of the newest row that exists.
//!
//! So this test reads the same two things a user's session depends on:
//! 1. the **published** spectrum stream — what the browser's live edge is made of — record by
//!    record, across several wraps;
//! 2. the **history counters** — whether the pyramid kept every row it was given.

mod common;

use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use hk_core::{Pacing, Source};
use hk_pipeline::{Pipeline, SourceFactory, TrackInventory, open_replay};
use hk_stream::{Declared, Record, StreamReader};
use serde_json::json;

const FS: f64 = 250e3;
/// A pass of the recording, seconds.
const PASS_S: f64 = 0.6;
const CENTER: f64 = 433.5e6;
/// Wraps to see before the run is stopped.
const LOOPS: u64 = 3;

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

#[test]
fn a_looping_replay_never_serves_a_backwards_capture_time() {
    let dir = TempDir::new("t474-loop");
    let meta = tone_recording(&dir.0.join("src"), "loop", FS, PASS_S, CENTER, None);
    let (mut cfg, replay) = replay_config(&dir.0, &meta, json!({}), Pacing::Unpaced);

    // The consumer: subscribed to the offered spectrum stream, exactly as the web client is.
    let consumer = Buf::default();
    let sink_buf = consumer.clone();
    let stream_id = cfg.spectrum_stream_id.clone();
    cfg.stream_sink = Some(Arc::new(move |h, handle| {
        if h.stream_id == stream_id {
            handle
                .subscribe(
                    "t474-consumer",
                    Declared::local(sink_buf.clone()),
                    Box::new(|_| {}),
                )
                .unwrap();
        }
    }));

    // `--loop`: the same recording, opened again at its end.
    let again = meta.clone();
    let reopen: SourceFactory = Box::new(move || {
        Ok(Box::new(open_replay(&again, Pacing::Unpaced, false)?.source) as Box<dyn Source>)
    });
    let handle = Pipeline::start(
        cfg,
        Box::new(replay.source),
        replay.info,
        Some(reopen),
        Box::new(TrackInventory::default()),
    )
    .unwrap();

    // A looping replay never ends on its own: stop it once it has wrapped enough times.
    let counters = handle.counters();
    let stopper = handle.stopper();
    let waiter = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(120);
        while counters.source.loops.load(Ordering::Relaxed) < LOOPS {
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stopper.stop();
    });
    let (s, fired) = wait_guarded(handle, Duration::from_secs(180));
    waiter.join().unwrap();
    eprintln!("{}", s.to_text());
    assert!(!fired, "the watchdog had to stop the run");
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    let loops = s.counter("/source/loops");
    assert!(loops >= LOOPS, "the replay wrapped {loops} times");
    assert_eq!(s.counter("/source/ring_errors"), 0);

    // ---- 1. what the live edge is made of ----
    let bytes = consumer.0.lock().unwrap().clone();
    let mut reader = StreamReader::new(std::io::Cursor::new(bytes));
    let mut times: Vec<i64> = Vec::new();
    while let Some(r) = reader.next_record().unwrap() {
        if let Record::Binary(b) = r {
            times.push(b.header.t.as_unix_nanos());
        }
    }
    assert!(
        times.len() > 10,
        "only {} spectrum rows were published; the test saw no wrap",
        times.len()
    );
    for (k, w) in times.windows(2).enumerate() {
        assert!(
            w[1] > w[0],
            "spectrum row {} moved capture time backwards: {} -> {} ({} ms). A consumer that \
             clamps its edge monotone (every one of ours does) would then sit ahead of the \
             newest row that exists.",
            k + 1,
            w[0],
            w[1],
            (w[1] - w[0]) as f64 / 1e6
        );
    }
    // The passes were spliced onto the axis, not laid on top of each other: the published span
    // covers more than one pass of the recording.
    let span_s = (times[times.len() - 1] - times[0]) as f64 / 1e9;
    assert!(
        span_s > PASS_S * 1.5,
        "the published rows span only {span_s:.3} s of capture time over {loops} passes of a \
         {PASS_S} s recording: the axis restarted instead of continuing"
    );

    // ---- 2. whether the pyramid kept every row ----
    // A backwards wrap is not a cosmetic glitch: it is dropped rows. `frames_late` is how that
    // loss would show, and it is the only place it would show.
    assert_eq!(
        s.counter("/history/frames_late"),
        0,
        "the spectrum-history pyramid dropped rows as late — capture time went backwards under it"
    );
    assert_eq!(
        s.counter("/history/view_late"),
        0,
        "the view lattice dropped rows as late — capture time went backwards under it"
    );
    // A wrap is an expected splice, so it needs no rescue; a rescue here would mean a source's
    // own clock was behind the run's (see `Axis::place`).
    assert_eq!(s.counter("/source/time_splices"), 0);
}
