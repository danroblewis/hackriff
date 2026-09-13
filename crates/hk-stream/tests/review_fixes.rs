//! Regressions for the T-016 review probes (P7, P8, P10 and the robustness items): own-key
//! streams local-only, gated spectrum row-rate cap, UDS 0600 and live-socket refusal, consumer
//! cap with idle hang-up reaping, tail drop markers, drain timeout, marker-sized message frames.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hk_model::{ContentClass, Timestamp};
use hk_stream::{
    BinaryRecord, CloseReason, ConsumerState, ListenAddr, Listener, MessageRecord, Publisher,
    PublisherConfig, Record, RecordFlags, StreamError, StreamHeader, StreamKind, StreamReader,
};
use serde_json::json;

const SENTINEL: &str = "SENTINEL-OWNKEY-9e2b";

#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Blocks every write until opened.
struct Valve(Arc<(Mutex<bool>, Condvar)>, SharedBuf);

impl Write for Valve {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let (m, cv) = &*self.0;
        let mut open = m.lock().unwrap();
        while !*open {
            open = cv.wait(open).unwrap();
        }
        self.1.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn open_valve(v: &Arc<(Mutex<bool>, Condvar)>) {
    *v.0.lock().unwrap() = true;
    v.1.notify_all();
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hkr{tag}{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(2));
    }
}

fn messages(class: ContentClass) -> StreamHeader {
    let mut h = StreamHeader::new("m", StreamKind::Messages, class, "review-tests");
    h.max_frame_len = 16 * 1024;
    h
}

fn small() -> PublisherConfig {
    PublisherConfig {
        queue_bytes: 256 * 1024,
        ..PublisherConfig::default()
    }
}

fn record(class: ContentClass) -> MessageRecord {
    MessageRecord {
        t: Timestamp::now(),
        emitter_id: None,
        provenance_ref: None,
        content_class: class,
        decode_id: None,
        annotation_id: None,
        decoder: None,
        frame_model: None,
        crc_status: None,
        identity: None,
        metadata: json!({"len": 12}),
        content: Some(json!({"text": SENTINEL})),
    }
}

/// P8: an own-key-decrypted record never carries content on a stream that is not itself
/// own-key-decrypted, even though own-key permits content.
#[test]
fn p8_own_key_content_needs_an_own_key_stream() {
    for (stream_class, expect_content) in [
        (ContentClass::Unrestricted, false),
        (ContentClass::OwnKeyDecrypted, true),
    ] {
        let mut p = Publisher::new(messages(stream_class), small()).unwrap();
        let h = p.handle();
        let buf = SharedBuf::default();
        h.subscribe("mem", Box::new(buf.clone()), Box::new(|_| {}))
            .unwrap();
        p.publish_message(&record(ContentClass::OwnKeyDecrypted))
            .unwrap();
        drop(p);
        assert!(h.wait_closed(Duration::from_secs(5)));
        let bytes = buf.0.lock().unwrap().clone();
        let mut r = StreamReader::new(&bytes[..]);
        let Some(Record::Message(m)) = r.next_record().unwrap() else {
            panic!()
        };
        assert_eq!(m.content_class, ContentClass::OwnKeyDecrypted);
        assert_eq!(m.gated, !expect_content, "{stream_class:?}");
        assert_eq!(
            contains(&bytes, SENTINEL),
            expect_content,
            "{stream_class:?}"
        );
    }
}

/// Own-key streams are refused on TCP and served on a mode-0600 Unix socket.
#[test]
fn own_key_streams_are_uds_only_and_the_socket_is_0600() {
    let dir = temp_dir("ok");
    let path = dir.join("own.sock");
    let mut p = Publisher::new(messages(ContentClass::OwnKeyDecrypted), small()).unwrap();
    let h = p.handle();
    assert_eq!(h.content_class(), ContentClass::OwnKeyDecrypted);
    let err = Listener::bind_tcp("127.0.0.1:0", h.clone())
        .err()
        .expect("own-key over TCP refused");
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);

    let listener = Listener::bind_uds(&path, h.clone()).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "socket mode {mode:o}");
    assert!(
        std::fs::read_dir(&dir).unwrap().count() == 1,
        "no staging directory left behind"
    );
    let client_path = path.clone();
    let client = thread::spawn(move || {
        let mut bytes = Vec::new();
        UnixStream::connect(client_path)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        bytes
    });
    wait_for("uds consumer", || h.open_consumers() == 1);
    p.publish_message(&record(ContentClass::OwnKeyDecrypted))
        .unwrap();
    drop(p);
    assert!(
        contains(&client.join().unwrap(), SENTINEL),
        "local own-key works"
    );
    drop(listener);
    let _ = std::fs::remove_dir_all(dir);
}

/// A path served by a live listener is refused; a stale socket file is replaced.
#[test]
fn bind_uds_refuses_a_live_socket_and_replaces_a_stale_one() {
    let dir = temp_dir("lv");
    let path = dir.join("s.sock");
    let p = Publisher::new(messages(ContentClass::Unrestricted), small()).unwrap();
    let first = Listener::bind_uds(&path, p.handle()).unwrap();
    let err = Listener::bind_uds(&path, p.handle())
        .err()
        .expect("live socket refused");
    assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
    assert!(
        UnixStream::connect(&path).is_ok(),
        "first listener untouched"
    );
    drop(first);

    // Leave a stale socket file behind (bound, then closed without unlinking).
    drop(UnixListener::bind(&path).unwrap());
    assert!(path.exists());
    let second = Listener::bind_uds(&path, p.handle()).unwrap();
    assert!(UnixStream::connect(&path).is_ok());
    drop(second);
    let _ = std::fs::remove_dir_all(dir);
}

/// Under a class that forbids content, spectrum streams faster than the row-rate cap (or with
/// no declared rate) are refused; survey-speed waterfalls and permitting classes are allowed.
#[test]
fn gated_spectrum_row_rate_is_capped() {
    let spectrum = |class, rate: Option<f64>| {
        let mut h = StreamHeader::new("s", StreamKind::Spectrum, class, "t");
        h.datatype = Some("rf32_le".into());
        h.sample_rate_hz = rate;
        h.fft_size = Some(1024);
        h.max_frame_len = 16 * 1024;
        Publisher::new(h, small())
    };
    for class in [
        ContentClass::MetadataOnly,
        ContentClass::RestrictedCellular,
        ContentClass::RestrictedPaging,
    ] {
        assert!(matches!(
            spectrum(class, Some(1000.0)),
            Err(StreamError::SpectrumRowRate { .. })
        ));
        assert!(matches!(
            spectrum(class, None),
            Err(StreamError::SpectrumRowRate { .. })
        ));
        assert!(spectrum(class, Some(30.0)).is_ok(), "survey waterfall");
        assert!(spectrum(class, Some(hk_stream::GATED_SPECTRUM_MAX_ROW_RATE_HZ)).is_ok());
    }
    assert!(spectrum(ContentClass::Unrestricted, Some(10_000.0)).is_ok());
    assert!(spectrum(ContentClass::OwnKeyDecrypted, None).is_ok());
}

/// P7: 1000 connect/close cycles on an idle stream are reaped without any publish, and open
/// consumers are capped at `max_consumers`.
#[test]
fn p7_connect_close_flood_is_reaped_and_consumers_are_capped() {
    let p = Publisher::new(
        messages(ContentClass::Unrestricted),
        PublisherConfig {
            max_consumers: 8,
            ..small()
        },
    )
    .unwrap();
    let h = p.handle();
    let listener = Listener::bind_tcp("127.0.0.1:0", h.clone()).unwrap();
    let ListenAddr::Tcp(addr) = listener.addr().clone() else {
        unreachable!()
    };
    let start = Instant::now();
    for _ in 0..1000 {
        if let Ok(s) = TcpStream::connect(addr) {
            drop(s);
        }
    }
    wait_for("flood reaped", || h.open_consumers() == 0);
    println!("1000 connect/close cycles reaped in {:?}", start.elapsed());

    let held: Vec<TcpStream> = (0..12).map(|_| TcpStream::connect(addr).unwrap()).collect();
    wait_for("cap reached", || h.open_consumers() == 8);
    thread::sleep(Duration::from_millis(200));
    assert_eq!(h.open_consumers(), 8, "never more than max_consumers");
    drop(held);
    wait_for("held connections reaped", || h.open_consumers() == 0);
    drop(listener);
    drop(p);
}

/// P10: records dropped just before the stream finishes are still reported by a marker.
#[test]
fn p10_tail_drops_before_finish_get_a_marker() {
    let valve = Arc::new((Mutex::new(false), Condvar::new()));
    let buf = SharedBuf::default();
    let mut h = StreamHeader::new("b", StreamKind::Bits, ContentClass::Unrestricted, "t");
    h.datatype = Some("ru8".into());
    h.max_frame_len = 4096;
    let mut p = Publisher::new(
        h,
        PublisherConfig {
            queue_bytes: 16 * 1024,
            disconnect_after_drops: u64::MAX,
            disconnect_after: Duration::from_secs(3600),
            ..PublisherConfig::default()
        },
    )
    .unwrap();
    let handle = p.handle();
    handle
        .subscribe(
            "valve",
            Box::new(Valve(Arc::clone(&valve), buf.clone())),
            Box::new(|_| {}),
        )
        .unwrap();
    let mut n = 0u64;
    loop {
        let o = p
            .publish_binary(BinaryRecord {
                t: Timestamp::from_unix_nanos(n as i64),
                sample_index: n,
                flags: RecordFlags::empty(),
                payload: &[1u8; 1000],
            })
            .unwrap();
        n += 1;
        if o.dropped > 0 && n > 40 {
            break;
        }
    }
    drop(p);
    open_valve(&valve);
    assert!(handle.wait_closed(Duration::from_secs(5)));
    let bytes = buf.0.lock().unwrap().clone();
    let mut r = StreamReader::new(&bytes[..]);
    let (mut next, mut markers) = (0u64, 0);
    while let Some(rec) = r.next_record().unwrap() {
        match rec {
            Record::Binary(b) => {
                assert_eq!(b.header.seq, next);
                next += 1;
            }
            Record::Dropped(d) => {
                assert_eq!(d.first_seq, next);
                next += d.count;
                markers += 1;
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(markers, 1, "the tail drop run is reported");
    assert_eq!(
        next, n,
        "every published seq is delivered or covered by a marker"
    );
}

/// A consumer that cannot drain after the publisher finishes is closed after `drain_timeout`.
#[test]
fn draining_consumer_is_closed_after_drain_timeout() {
    let valve = Arc::new((Mutex::new(false), Condvar::new()));
    let mut p = Publisher::new(
        messages(ContentClass::Unrestricted),
        PublisherConfig {
            drain_timeout: Duration::from_millis(200),
            ..small()
        },
    )
    .unwrap();
    let h = p.handle();
    let id = h
        .subscribe(
            "stuck",
            Box::new(Valve(Arc::clone(&valve), SharedBuf::default())),
            Box::new(|_| {}),
        )
        .unwrap();
    p.publish_message(&record(ContentClass::Unrestricted))
        .unwrap();
    let t0 = Instant::now();
    drop(p);
    assert!(h.wait_closed(Duration::from_secs(3)));
    assert!(t0.elapsed() >= Duration::from_millis(150));
    assert_eq!(
        h.stats(id).unwrap().state,
        ConsumerState::Closed(CloseReason::DrainTimeout)
    );
    open_valve(&valve);
}

/// A messages stream's frame limit must hold a drop marker.
#[test]
fn message_stream_max_frame_len_must_hold_a_marker() {
    let mut h = messages(ContentClass::Unrestricted);
    h.max_frame_len = 100;
    assert!(h.validate().is_err());
    h.max_frame_len = 160;
    assert!(h.validate().is_ok());
}
