//! T-489: an unwatched spectrum stream skips its FFT, and nothing recorded changes because of it.
//!
//! Reader 3's STFT was T-348's measured ~13% of pipeline CPU and ran whether or not anything was
//! subscribed. It is now skipped while no consumer is open — the headless scheduled-survey case
//! (product workflow #2: hours on battery with no browser attached). The three things that must
//! stay true, and what asserts them here:
//!
//! 1. **The ring is still drained.** Reader 3 holds a gate cursor, so a lossless run stalls capture
//!    behind it if it stops reading. `an_unwatched_run_records_exactly_what_a_watched_one_does`
//!    runs the *same recording* lossless with and without a consumer and requires the spectrum
//!    reader to have read the same number of samples either way, with no lost samples.
//! 2. **The data does not change because nobody was looking.** The same test compares the stored
//!    detections and the history/detection counters of the two runs: a skipped FFT that left a
//!    hole in the pyramid or the coverage map would be a lie about what was observed, not a
//!    saving. (It cannot: history, detection, the IQ ring and the receiver-line survey each hold
//!    their own ring reader and their own STFT.) The only difference allowed is `/spectrum/rows`,
//!    which honestly counts the display rows nobody asked for at zero.
//! 3. **Attaching starts the rows again.** `rows_resume_when_a_subscriber_attaches` replays in
//!    real time, subscribes part-way through, and requires the first row within one read timeout
//!    plus a row period — and requires *every* row the run published to have reached that
//!    subscriber, which can only hold if none were published before it arrived.

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use hk_core::Pacing;
use hk_model::{Detection, FreqRange, Region, TimeRange, Timestamp};
use hk_stream::{Declared, FrameDecoder, MAX_FRAME_LEN, PublisherHandle, StreamKind};
use serde_json::{Value, json};

const FS: f64 = 250e3;
const CENTER: f64 = 433.5e6;

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Buf {
    /// Bytes received so far.
    fn bytes(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Data records (type 1) in a local consumer's framed bytes.
fn records(bytes: &[u8]) -> usize {
    let mut dec = FrameDecoder::new(MAX_FRAME_LEN);
    dec.push(bytes);
    let mut n = 0;
    let mut first = true;
    while let Ok(Some(frame)) = dec.next_frame() {
        if first {
            first = false; // the header frame
            continue;
        }
        if frame.len() >= 32 && frame[0] == 1 {
            n += 1;
        }
    }
    n
}

/// Every stored detection, without its per-run identities: what the run recorded.
fn recorded(dir: &std::path::Path) -> Vec<Value> {
    let ever = TimeRange::new(
        Timestamp::from_unix_nanos(0),
        Timestamp::from_unix_nanos(i64::MAX / 2),
    );
    let mut d: Vec<Detection> = repo(dir)
        .detections_in_region(&Region::new(FreqRange::new(0.0, 7e9), ever))
        .unwrap();
    d.sort_by_key(|x| (x.time.start.as_unix_nanos(), x.f_center_hz.to_bits()));
    d.iter()
        .map(|x| {
            let mut v = serde_json::to_value(x).unwrap();
            let o = v.as_object_mut().unwrap();
            for k in ["id", "survey_id", "provenance_ref"] {
                o.remove(k);
            }
            v
        })
        .collect()
}

#[test]
fn an_unwatched_run_records_exactly_what_a_watched_one_does() {
    let dir = TempDir::new("t489-parity");
    let src = tone_recording(&dir.0.join("src"), "tone", FS, 3.0, CENTER, None);

    // Watched: the sink opens a consumer the moment the publisher is offered.
    let watched_dir = dir.0.join("watched");
    let (mut cfg, replay) = replay_config(&watched_dir, &src, json!({}), Pacing::Unpaced);
    assert!(cfg.lossless, "the parity runs are lossless");
    let seen = Buf::default();
    let sink_buf = seen.clone();
    cfg.stream_sink = Some(Arc::new(move |h, handle| {
        if h.kind == StreamKind::Spectrum {
            handle
                .subscribe(
                    "t489-watcher",
                    Declared::local(sink_buf.clone()),
                    Box::new(|_| {}),
                )
                .unwrap();
        }
    }));
    let (watched, fired) = wait_guarded(start(cfg, replay), Duration::from_secs(120));
    assert!(!fired, "the watched run finished on its own");
    assert!(watched.errors.is_empty(), "{:?}", watched.errors);

    // Unwatched: the publisher is still offered (nothing could ever subscribe otherwise), but
    // nothing subscribes — the headless survey.
    let unwatched_dir = dir.0.join("unwatched");
    let (mut cfg, replay) = replay_config(&unwatched_dir, &src, json!({}), Pacing::Unpaced);
    let offers = Arc::new(Mutex::new(0usize));
    let counted = Arc::clone(&offers);
    cfg.stream_sink = Some(Arc::new(move |h, _handle| {
        if h.kind == StreamKind::Spectrum {
            *counted.lock().unwrap() += 1;
        }
    }));
    let (unwatched, fired) = wait_guarded(start(cfg, replay), Duration::from_secs(120));
    assert!(!fired, "the unwatched run finished on its own");
    assert!(unwatched.errors.is_empty(), "{:?}", unwatched.errors);

    // (b) The stream is still offered while idle: gating the offer would mean nothing could ever
    // subscribe, because the offer is what registers the handle with the hk-api bridge.
    assert!(
        *offers.lock().unwrap() >= 1,
        "the spectrum publisher is offered with no consumer"
    );

    // The saving: the rows nobody asked for are not produced.
    assert!(
        watched.counter("/spectrum/rows") > 10,
        "watched rows {}",
        watched.counter("/spectrum/rows")
    );
    assert_eq!(
        unwatched.counter("/spectrum/rows"),
        0,
        "no row is published with no consumer"
    );
    assert_eq!(
        unwatched.counter("/readers/spectrum/frames"),
        0,
        "and no FFT frame is computed for one"
    );
    assert!(seen.bytes() > 0, "the watched consumer received bytes");

    // (a) The ring is still drained: reader 3 read every sample either way, so its gate cursor
    // advanced and a lossless capture never waited on it.
    for s in [&watched, &unwatched] {
        assert_eq!(s.always_on_lost_samples, 0, "{}", s.to_text());
        assert_eq!(s.counter("/readers/spectrum/lost_samples"), 0);
    }
    let read = unwatched.counter("/readers/spectrum/samples");
    assert!(read > 0, "the unwatched reader read samples");
    assert_eq!(
        read,
        watched.counter("/readers/spectrum/samples"),
        "the same samples are read either way"
    );

    // The rule: the data must not change because nobody was looking.
    assert_eq!(
        recorded(&unwatched_dir),
        recorded(&watched_dir),
        "the stored detections differ between a watched and an unwatched run"
    );
    for p in [
        "/history/frames_ingested",
        "/history/tiles_written",
        "/detect/tracks_opened",
        "/readers/history/frames",
        "/readers/detect/frames",
    ] {
        assert_eq!(
            unwatched.counter(p),
            watched.counter(p),
            "{p} differs between a watched and an unwatched run"
        );
    }
    eprintln!(
        "T-489: watched {} rows / unwatched {} rows; both read {read} samples, \
         {} history frames, {} detections",
        watched.counter("/spectrum/rows"),
        unwatched.counter("/spectrum/rows"),
        watched.counter("/history/frames_ingested"),
        recorded(&watched_dir).len()
    );
}

#[test]
fn rows_resume_when_a_subscriber_attaches() {
    let dir = TempDir::new("t489-resume");
    let src = tone_recording(&dir.0.join("src"), "tone", FS, 4.0, CENTER, None);
    let (mut cfg, replay) = replay_config(&dir.0, &src, json!({}), Pacing::RealTime { speed: 1.0 });
    let offered: Arc<Mutex<Option<(PublisherHandle, u32)>>> = Arc::new(Mutex::new(None));
    let put = Arc::clone(&offered);
    cfg.stream_sink = Some(Arc::new(move |h, handle| {
        if h.kind == StreamKind::Spectrum {
            *put.lock().unwrap() = Some((handle, h.fft_size.unwrap_or(0)));
        }
    }));
    let handle = start(cfg, replay);

    // Let the run go unwatched for a while first.
    let deadline = Instant::now() + Duration::from_secs(20);
    let publisher = loop {
        if let Some(p) = offered.lock().unwrap().clone() {
            break p;
        }
        assert!(Instant::now() < deadline, "the publisher was never offered");
        std::thread::sleep(Duration::from_millis(20));
    };
    std::thread::sleep(Duration::from_millis(1200));

    let rows = Buf::default();
    let attached = Instant::now();
    publisher
        .0
        .subscribe(
            "t489-latecomer",
            Declared::local(rows.clone()),
            Box::new(|_| {}),
        )
        .unwrap();
    let bins = publisher.1 as usize;
    let first = loop {
        if records(&rows.0.lock().unwrap()) > 0 {
            break attached.elapsed();
        }
        assert!(
            attached.elapsed() < Duration::from_secs(5),
            "no row {:?} after attaching ({} bytes)",
            attached.elapsed(),
            rows.bytes()
        );
        std::thread::sleep(Duration::from_millis(5));
    };

    let (summary, fired) = wait_guarded(handle, Duration::from_secs(120));
    assert!(!fired, "the run finished on its own");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    let published = summary.counter("/spectrum/rows") - summary.counter("/spectrum/rows_gated");
    // The consumer's writer thread may still be draining when the run returns.
    let deadline = Instant::now() + Duration::from_secs(30);
    let received = loop {
        let n = records(&rows.0.lock().unwrap()) as u64;
        if n >= published || Instant::now() > deadline {
            break n;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    eprintln!(
        "T-489: first row {first:?} after attaching; {published} rows published, \
         {received} received ({bins} bins)"
    );

    // One read timeout (50 ms) plus a row period, with slack for a loaded box.
    assert!(
        first < Duration::from_secs(1),
        "the first row took {first:?} to arrive after a subscriber attached"
    );
    assert!(published > 0, "rows resumed");
    // Every row the run published reached this subscriber, which can only be true if none was
    // published before it attached.
    assert_eq!(
        received, published,
        "the run published {published} rows and the subscriber that attached part-way through \
         received {received}: rows were produced while nothing was subscribed"
    );
}
