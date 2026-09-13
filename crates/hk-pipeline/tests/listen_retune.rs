//! T-043 × T-050 legal sentinel: listening under broadcast FM, then a control-plane retune into
//! the 930.5 MHz paging band (47 CFR 24.129, restricted-paging). The scripted radio plays the same
//! carrier in both windows, so only the window's class differs.
//!
//! - **Positive control:** under FM the listen request is admitted and audio records flow.
//! - **After the retune:** the run re-plumbed; the listen chain ended with its segment
//!   (`/listen/retune_ends`), no further audio record is produced or delivered, the stream's
//!   consumer is closed, and a new request at the same offset in the paging window is refused
//!   (403, restricted-paging) before any probe reads the ring.

mod common;
#[path = "support/radio.rs"]
mod radio;

use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use hk_model::{ContentClass, Timestamp};
use hk_pipeline::class::window_class;
use hk_pipeline::{Pipeline, PipelineConfig, SourceInfo, TrackInventory, replay_plan};
use hk_stream::{
    BinaryRecordHeader, Declared, FrameDecoder, HEADER_MAX_LEN, OpenRequest, StreamOpener,
};

const FM: f64 = 100.8e6;
const PAGING: f64 = 930.5e6;
const FS: f64 = 1.0e6;
const OFFSET_HZ: f64 = 150e3;

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

fn wait(what: &str, limit: Duration, f: impl Fn() -> bool) {
    let deadline = Instant::now() + limit;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A 10 kHz selection around `f`.
fn request(f: f64) -> OpenRequest {
    OpenRequest {
        params: vec![
            ("f_lo".into(), format!("{}", f - 5e3)),
            ("f_hi".into(), format!("{}", f + 5e3)),
        ],
        peer: "test".into(),
    }
}

/// Audio data records (type 1) in a consumer's wire bytes (header frame first).
fn data_records(buf: &Buf) -> usize {
    let bytes = buf.0.lock().unwrap().clone();
    let mut dec = FrameDecoder::new(HEADER_MAX_LEN);
    dec.push(&bytes);
    let (mut n, mut first) = (0, true);
    while let Ok(Some(f)) = dec.next_frame() {
        if std::mem::replace(&mut first, false) {
            continue;
        }
        if BinaryRecordHeader::decode(f).is_some_and(|h| h.record_type == 1) {
            n += 1;
        }
    }
    n
}

#[test]
fn retuning_into_paging_while_listening_stops_the_audio_and_refuses_new_listeners() {
    assert_eq!(window_class(FM, FS), ContentClass::Unrestricted);
    assert_eq!(window_class(PAGING, FS), ContentClass::RestrictedPaging);
    let dir = TempDir::new("t043-listen-retune");
    let (radio, ctl) = radio::Radio::new(FM, FS, 16_384, radio::tone(|_| OFFSET_HZ));
    let t0 = Timestamp::from_unix_nanos(radio::T0_NS);
    let mut cfg = PipelineConfig::new(&dir.0, replay_plan(FM, FS, t0)).unwrap();
    cfg.source_class = window_class(FM, FS);
    cfg.live_window_class = true;
    cfg.lossless = true;
    cfg.settings.chains = Some(Vec::new());
    let handle = Pipeline::start(
        cfg,
        Box::new(radio),
        SourceInfo {
            sample_rate_hz: FS,
            center_hz: FM,
            start_time: t0,
        },
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let counters = handle.counters();
    let listen = handle.listen_service();
    let plane = handle.controller();
    let lc = &counters.listen;
    wait(
        "the first second of samples",
        Duration::from_secs(120),
        || counters.source.samples.load(Ordering::Relaxed) >= FS as u64,
    );

    // Positive control: listening under FM.
    let opened = listen
        .open(&request(FM + OFFSET_HZ))
        .unwrap_or_else(|e| panic!("listening under broadcast FM: {e}"));
    assert_eq!(opened.header.content_class, ContentClass::Unrestricted);
    eprintln!(
        "listen header: {}",
        serde_json::to_string(&opened.header).unwrap()
    );
    let buf = Buf::default();
    opened
        .handle
        .subscribe(
            "t043-retune",
            Declared::local(buf.clone()),
            Box::new(|_| {}),
        )
        .unwrap();
    wait("audio under FM", Duration::from_secs(120), || {
        data_records(&buf) >= 10
    });

    // Retune into paging: the chain ends with its segment.
    let outcome = plane.retune(PAGING, FS).expect("retune into paging");
    assert!(outcome.replumbed, "another class: re-plumbed");
    assert_eq!(outcome.content_class, ContentClass::RestrictedPaging);
    wait(
        "the listen chain to end with its segment",
        Duration::from_secs(30),
        || lc.active.load(Ordering::SeqCst) == 0,
    );
    assert!(lc.retune_ends.load(Ordering::Relaxed) >= 1);
    wait("the consumer to close", Duration::from_secs(10), || {
        opened.handle.open_consumers() == 0
    });
    let (frames, delivered) = (lc.frames.load(Ordering::Relaxed), data_records(&buf));
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        lc.frames.load(Ordering::Relaxed),
        frames,
        "no audio after the retune"
    );
    assert_eq!(data_records(&buf), delivered, "no audio delivered after it");

    // A new request at the same offset in the paging window is refused before any probe.
    let probes = lc.probes.load(Ordering::Relaxed);
    match listen.open(&request(PAGING + OFFSET_HZ)) {
        Err(e) => {
            assert_eq!(e.status, 403, "{e}");
            assert_eq!(e.content_class, Some(ContentClass::RestrictedPaging));
        }
        Ok(_) => panic!("paging-band audio must be refused"),
    }
    assert_eq!(lc.probes.load(Ordering::Relaxed), probes);
    assert_eq!(lc.attached.load(Ordering::Relaxed), 1);
    eprintln!("listen counters: {}", lc.to_json());

    drop(opened);
    drop(listen);
    ctl.finish();
    let (s, fired) = wait_guarded(handle, Duration::from_secs(120));
    assert!(!fired, "the run finished on its own");
    assert!(s.errors.is_empty(), "{:?}", s.errors);
}
