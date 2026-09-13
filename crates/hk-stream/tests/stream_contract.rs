//! Stream-output contract tests (T-016, ADR-0004, docs/stream-contract.md).
//!
//! - Gating: every ContentClass x every StreamKind, sentinel scan of the raw wire bytes.
//! - Backpressure: a consumer that never reads is dropped and counted while a fast consumer gets
//!   every record and the producer runs in bounded time.
//! - SIGNAL-001: ADS-B-like Decode messages reach an external consumer as header + NDJSON.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hk_model::{
    ContentClass, CrcStatus, Decode, DecodeId, DecodedIdentity, EmitterId, IdentityScheme,
    Timestamp,
};
use hk_stream::{
    BinaryRecord, CloseReason, ConsumerState, ListenAddr, Listener, MessageRecord, Publisher,
    PublisherConfig, Record, RecordFlags, StreamError, StreamHeader, StreamKind, StreamReader,
};
use serde_json::json;

const SENTINEL: &str = "SENTINEL-CONTENT-7f3a91";

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

/// A fresh short directory: Unix socket paths are limited to ~104 bytes on macOS.
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hk{tag}{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn header_for(kind: StreamKind, class: ContentClass) -> StreamHeader {
    let mut h = StreamHeader::new(
        format!("test/{}", kind.as_str()),
        kind,
        class,
        "hk-api-tests",
    );
    match kind {
        StreamKind::Iq => {
            h.datatype = Some("ci8".into());
            h.sample_rate_hz = Some(2e6);
            h.center_hz = Some(1090e6);
        }
        StreamKind::Audio => {
            h.datatype = Some("ri16_le".into());
            h.sample_rate_hz = Some(48e3);
        }
        StreamKind::Bits => h.datatype = Some("ru8".into()),
        StreamKind::Symbols => h.datatype = Some("rf32_le".into()),
        StreamKind::Spectrum => {
            h.datatype = Some("rf32_le".into());
            // A survey waterfall: within the gated row-rate cap, so it exists under every class.
            h.sample_rate_hz = Some(30.0);
            h.fft_size = Some(4);
        }
        StreamKind::Messages => h.message_schema = Some("hackriff.decode/1".into()),
    }
    h.max_frame_len = 64 * 1024;
    h
}

fn message(record_class: ContentClass, icao: &str, emitter: Option<EmitterId>) -> MessageRecord {
    MessageRecord {
        t: Timestamp::now(),
        emitter_id: emitter,
        provenance_ref: None,
        content_class: record_class,
        decode_id: Some(DecodeId::new()),
        annotation_id: None,
        decoder: Some("test@1".into()),
        frame_model: Some("adsb-df17".into()),
        crc_status: Some(CrcStatus::Valid),
        identity: Some(DecodedIdentity {
            scheme: IdentityScheme::AdsbIcao,
            value: icao.into(),
        }),
        metadata: json!({"icao": icao, "df": 17, "crc": "ok"}),
        content: Some(json!({"text": SENTINEL})),
    }
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

fn small_config() -> PublisherConfig {
    PublisherConfig {
        queue_bytes: 1024 * 1024,
        ..PublisherConfig::default()
    }
}

/// Unit-tests the ADR-0004 gate over every ContentClass x StreamKind, reading back through the
/// reference reader and scanning the raw bytes for the content sentinel.
#[test]
fn gating_matrix_every_class_by_every_kind() {
    let mut matrix = Vec::new();
    for &class in ContentClass::ALL {
        for &kind in StreamKind::ALL {
            let mut publisher = Publisher::new(header_for(kind, class), small_config()).unwrap();
            let buf = SharedBuf::default();
            let handle = publisher.handle();
            handle
                .subscribe("mem", Box::new(buf.clone()), Box::new(|_| {}))
                .unwrap();

            let payload_expected;
            if kind == StreamKind::Messages {
                // Record 0 claims unrestricted: only the header class can gate it.
                publisher
                    .publish_message(&message(ContentClass::Unrestricted, "a1b2c3", None))
                    .unwrap();
                // Record 1 claims restricted-paging: always gated, whatever the header says.
                publisher
                    .publish_message(&message(ContentClass::RestrictedPaging, "a1b2c4", None))
                    .unwrap();
                payload_expected = class.permits_content();
            } else {
                let payload = SENTINEL.repeat(8).into_bytes(); // 184 bytes: whole ci8/ri16/rf32 elements
                let result = publisher.publish_binary(BinaryRecord {
                    t: Timestamp::from_unix_nanos(42),
                    sample_index: 7,
                    // A producer cannot pre-set GATED to fake a metadata record.
                    flags: RecordFlags::GATED.with(RecordFlags::BURST_START),
                    payload: &payload,
                });
                payload_expected = !kind.payload_is_content() || class.permits_content();
                match (&result, payload_expected) {
                    (Ok(o), true) => assert_eq!(o.enqueued, 1),
                    (
                        Err(StreamError::ContentGated {
                            outcome, class: c, ..
                        }),
                        false,
                    ) => {
                        assert_eq!(*c, class);
                        assert_eq!(outcome.enqueued, 1, "metadata-only record still flows");
                    }
                    other => panic!("{class:?} x {kind:?}: unexpected {other:?}"),
                }
            }
            drop(publisher);
            assert!(handle.wait_closed(Duration::from_secs(5)));

            let bytes = buf.0.lock().unwrap().clone();
            let mut reader = StreamReader::new(&bytes[..]);
            let header = reader.read_header().unwrap().clone();
            assert_eq!(header.content_class, class);
            assert_eq!(header.kind, kind);
            let mut records = Vec::new();
            while let Some(r) = reader.next_record().unwrap() {
                records.push(r);
            }
            if kind == StreamKind::Messages {
                assert_eq!(records.len(), 2);
                for (i, r) in records.iter().enumerate() {
                    let Record::Message(m) = r else {
                        panic!("{r:?}")
                    };
                    let content_allowed = i == 0 && class.permits_content();
                    assert_eq!(m.gated, !content_allowed, "{class:?} record {i}");
                    assert_eq!(m.value.get("content").is_some(), content_allowed);
                    assert!(!m.content_class.permits_content() || content_allowed);
                    assert_eq!(m.value["metadata"]["df"], 17, "metadata always flows");
                    assert_eq!(m.seq, i as u64);
                }
            } else {
                assert_eq!(records.len(), 1);
                let Record::Binary(b) = &records[0] else {
                    panic!("{:?}", records[0])
                };
                assert_eq!(b.header.payload_len, 184, "length is metadata");
                assert_eq!(b.header.sample_index, 7);
                assert!(b.header.flags.contains(RecordFlags::BURST_START));
                assert_eq!(
                    b.header.flags.contains(RecordFlags::GATED),
                    !payload_expected
                );
                assert_eq!(b.payload.is_empty(), !payload_expected);
            }
            assert_eq!(
                contains(&bytes, SENTINEL),
                payload_expected,
                "{class:?} x {kind:?}: sentinel on the wire"
            );
            matrix.push(format!(
                "{class:?} x {}: {}",
                kind.as_str(),
                if payload_expected {
                    "content"
                } else {
                    "metadata-only"
                }
            ));
        }
    }
    println!("gating matrix:\n{}", matrix.join("\n"));
    assert_eq!(matrix.len(), 30);
}

/// Gated classes never put content bytes on a real socket; unrestricted and own-key do
/// (positive controls prove the scan can see the sentinel). Own-key streams are local-only, so
/// they are served on a Unix socket; everything else on TCP.
#[test]
fn gated_classes_never_emit_content_bytes_on_the_socket() {
    let dir = temp_dir("gs");
    for &class in ContentClass::ALL {
        let mut raw = Vec::new();
        for kind in [StreamKind::Messages, StreamKind::Iq, StreamKind::Audio] {
            let mut publisher = Publisher::new(header_for(kind, class), small_config()).unwrap();
            let handle = publisher.handle();
            let own_key = class == ContentClass::OwnKeyDecrypted;
            let listener = if own_key {
                Listener::bind_uds(dir.join(format!("{}.sock", kind.as_str())), handle.clone())
                    .unwrap()
            } else {
                Listener::bind_tcp("127.0.0.1:0", handle.clone()).unwrap()
            };
            let addr = listener.addr().clone();
            let client = thread::spawn(move || {
                let mut bytes = Vec::new();
                match addr {
                    ListenAddr::Tcp(a) => TcpStream::connect(a).unwrap().read_to_end(&mut bytes),
                    ListenAddr::Unix(p) => std::os::unix::net::UnixStream::connect(p)
                        .unwrap()
                        .read_to_end(&mut bytes),
                }
                .unwrap();
                bytes
            });
            wait_for(|| handle.open_consumers() == 1);
            for i in 0..50 {
                if kind == StreamKind::Messages {
                    publisher
                        .publish_message(&message(class, &format!("{:06x}", 0xa00000 + i), None))
                        .unwrap();
                } else {
                    let payload = SENTINEL.repeat(4).into_bytes();
                    let _ = publisher.publish_binary(BinaryRecord {
                        t: Timestamp::now(),
                        sample_index: i as u64 * 92,
                        flags: RecordFlags::empty(),
                        payload: &payload,
                    });
                }
            }
            drop(publisher);
            raw.extend(client.join().unwrap());
        }
        assert!(contains(&raw, "a00031"), "metadata flows for {class:?}");
        assert_eq!(
            contains(&raw, SENTINEL),
            class.permits_content(),
            "{class:?}"
        );
    }
}

fn wait_for(mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out");
        thread::sleep(Duration::from_millis(1));
    }
}

/// A consumer that never reads is dropped (counted, then disconnected) while a fast consumer
/// receives every record in order and the producer finishes in bounded time.
#[test]
fn slow_consumer_is_dropped_not_blocking() {
    const N: u64 = 100_000;
    const PAYLOAD: usize = 256;
    const DISCONNECT_AFTER_DROPS: u64 = 5_000;

    let dir = temp_dir("bp");
    let path = dir.join("iq.sock");
    let mut header = header_for(StreamKind::Iq, ContentClass::Unrestricted);
    header.max_frame_len = 4096;
    let config = PublisherConfig {
        queue_bytes: 64 * 1024,
        disconnect_after_drops: DISCONNECT_AFTER_DROPS,
        disconnect_after: Duration::from_secs(3600),
        ..PublisherConfig::default()
    };
    let mut publisher = Publisher::new(header, config).unwrap();
    let handle = publisher.handle();
    let listener = Listener::bind_uds(&path, handle.clone()).unwrap();

    // Slow consumer: connects through the listener and never reads.
    let stuck = UnixStream::connect(&path).unwrap();
    wait_for(|| handle.open_consumers() == 1);
    let slow_id = handle.consumer_stats()[0].id;

    // Fast consumer: a socket pair with enough queue for the whole run, read in a tight loop.
    let (ours, theirs) = UnixStream::pair().unwrap();
    let closer = ours.try_clone().unwrap();
    handle
        .subscribe_with_queue(
            "fast",
            Box::new(ours),
            Box::new(move |_| {
                let _ = closer.shutdown(std::net::Shutdown::Both);
            }),
            64 * 1024 * 1024,
        )
        .unwrap();
    let fast = thread::spawn(move || {
        let mut reader = StreamReader::new(theirs);
        let mut next = 0u64;
        while let Some(r) = reader.next_record().unwrap() {
            match r {
                Record::Binary(b) => {
                    assert_eq!(b.header.seq, next, "fast consumer saw a gap");
                    assert_eq!(b.payload.len(), PAYLOAD);
                    assert_eq!(b.payload[0], next as u8);
                    next += 1;
                }
                other => panic!("fast consumer got {other:?}"),
            }
        }
        next
    });

    let mut payload = [0u8; PAYLOAD];
    let mut disconnected_at = None;
    let mut worst = Duration::ZERO;
    let start = Instant::now();
    for seq in 0..N {
        payload[0] = seq as u8;
        let t0 = Instant::now();
        let outcome = publisher
            .publish_binary(BinaryRecord {
                t: Timestamp::from_unix_nanos(seq as i64),
                sample_index: seq * PAYLOAD as u64 / 2,
                flags: RecordFlags::empty(),
                payload: &payload,
            })
            .unwrap();
        worst = worst.max(t0.elapsed());
        if outcome.disconnected > 0 {
            assert!(disconnected_at.is_none());
            disconnected_at = Some(seq);
        }
    }
    let elapsed = start.elapsed();
    drop(publisher);
    let received = fast.join().unwrap();
    drop(stuck);
    drop(listener);

    let rate = N as f64 / elapsed.as_secs_f64();
    println!(
        "published {N} records in {elapsed:?} ({rate:.0} records/s) with a stuck consumer; worst publish {worst:?}"
    );
    assert_eq!(received, N, "fast consumer receives every record");
    assert!(
        elapsed < Duration::from_secs(20),
        "producer kept full rate: {elapsed:?}"
    );
    assert!(
        worst < Duration::from_millis(500),
        "no publish blocked: {worst:?}"
    );

    let slow = handle.stats(slow_id).unwrap();
    let at = disconnected_at.expect("slow consumer disconnected");
    assert_eq!(slow.state, ConsumerState::Closed(CloseReason::SlowConsumer));
    assert!(slow.records_dropped >= DISCONNECT_AFTER_DROPS);
    assert_eq!(
        slow.records_enqueued + slow.records_dropped,
        at + 1,
        "every record offered while attached is either queued or counted as dropped"
    );
    let fast_stats = handle
        .consumer_stats()
        .into_iter()
        .find(|s| s.label == "fast")
        .unwrap();
    assert_eq!(fast_stats.records_dropped, 0);
    assert_eq!(fast_stats.records_enqueued, N);
    let _ = std::fs::remove_dir_all(dir);
}

/// A consumer that falls behind and recovers sees an explicit "dropped N" marker naming exactly
/// the missing seqs.
#[test]
fn recovering_consumer_sees_exact_drop_markers() {
    struct Gate(Arc<(Mutex<bool>, std::sync::Condvar)>, SharedBuf);
    impl Write for Gate {
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
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let buf = SharedBuf::default();
    let mut header = header_for(StreamKind::Messages, ContentClass::Unrestricted);
    header.max_frame_len = 4096;
    let config = PublisherConfig {
        queue_bytes: 32 * 1024,
        disconnect_after_drops: u64::MAX,
        disconnect_after: Duration::from_secs(3600),
        ..PublisherConfig::default()
    };
    let mut publisher = Publisher::new(header, config).unwrap();
    let handle = publisher.handle();
    let id = handle
        .subscribe(
            "gated-writer",
            Box::new(Gate(Arc::clone(&gate), buf.clone())),
            Box::new(|_| {}),
        )
        .unwrap();
    let mut published = 0u64;
    while handle.stats(id).unwrap().records_dropped < 100 {
        publisher
            .publish_message(&message(ContentClass::Unrestricted, "a1b2c3", None))
            .unwrap();
        published += 1;
    }
    let dropped = handle.stats(id).unwrap().records_dropped;
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    wait_for(|| handle.stats(id).unwrap().queued_bytes == 0);
    publisher
        .publish_message(&message(ContentClass::Unrestricted, "a1b2c3", None))
        .unwrap();
    published += 1;
    drop(publisher);
    assert!(handle.wait_closed(Duration::from_secs(5)));

    let bytes = buf.0.lock().unwrap().clone();
    let mut reader = StreamReader::new(&bytes[..]);
    let mut expected = 0u64;
    let mut markers = 0;
    while let Some(r) = reader.next_record().unwrap() {
        match r {
            Record::Message(m) => {
                assert_eq!(m.seq, expected);
                expected += 1;
            }
            Record::Dropped(d) => {
                assert_eq!(d.first_seq, expected);
                assert_eq!(d.count, dropped);
                expected += d.count;
                markers += 1;
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(markers, 1);
    assert_eq!(expected, published, "markers account for every missing seq");
}

/// SIGNAL-001 (ADS-B / Mode S baseline): ADS-B-like Decode messages published on a messages
/// stream reach an external consumer as a header plus framed NDJSON records carrying the emitter
/// id and content_class, in order.
#[test]
fn signal_001_adsb_decodes_reach_an_external_consumer() {
    let dir = temp_dir("signal001");
    let path = dir.join("decodes.sock");
    let mut header = StreamHeader::new(
        "decodes/adsb",
        StreamKind::Messages,
        ContentClass::Unrestricted,
        "hk-plugins:readsb-like@test",
    );
    header.message_schema = Some("hackriff.decode/1".into());
    header.max_frame_len = 64 * 1024;
    let mut publisher = Publisher::new(header, small_config()).unwrap();
    let handle = publisher.handle();
    let _listener = Listener::bind_uds(&path, handle.clone()).unwrap();

    let consumer_path = path.clone();
    let consumer = thread::spawn(move || {
        let mut reader = StreamReader::connect_uds(&consumer_path).unwrap();
        let header = reader.read_header().unwrap().clone();
        let mut records = Vec::new();
        while let Some(r) = reader.next_record().unwrap() {
            records.push(r);
        }
        (header, records)
    });
    wait_for(|| handle.open_consumers() == 1);

    let aircraft: Vec<(String, EmitterId)> = (0..4)
        .map(|i| (format!("{:06x}", 0xa1b2c0 + i), EmitterId::new()))
        .collect();
    let mut sent = Vec::new();
    for n in 0..20 {
        let (icao, emitter) = &aircraft[n % aircraft.len()];
        let decode = Decode {
            id: DecodeId::new(),
            demodulation_ref: None,
            recording_ref: None,
            decoder_id: "readsb-like".into(),
            decoder_version: "test".into(),
            frame_model: "adsb-df17".into(),
            metadata: json!({"icao": icao, "df": 17, "tc": 11, "crc": "ok"}),
            content: None,
            crc_status: CrcStatus::Valid,
            identity: Some(DecodedIdentity {
                scheme: IdentityScheme::AdsbIcao,
                value: icao.clone(),
            }),
            content_class: ContentClass::Unrestricted,
            t: Timestamp::from_unix_nanos(1_757_000_000_000_000_000 + n as i64),
        };
        publisher
            .publish_message(&MessageRecord::from_decode(&decode, Some(*emitter), None))
            .unwrap();
        sent.push((decode, *emitter));
    }
    drop(publisher);
    let (header, records) = consumer.join().unwrap();

    assert_eq!(header.kind, StreamKind::Messages);
    assert_eq!(header.content_class, ContentClass::Unrestricted);
    assert_eq!(header.message_schema.as_deref(), Some("hackriff.decode/1"));
    assert_eq!(records.len(), sent.len());
    for (i, (r, (decode, emitter))) in records.iter().zip(&sent).enumerate() {
        let Record::Message(m) = r else {
            panic!("{r:?}")
        };
        assert_eq!(m.seq, i as u64, "in order");
        assert!(!m.gated);
        assert_eq!(m.content_class, ContentClass::Unrestricted);
        assert_eq!(m.value["type"], "message");
        assert_eq!(m.value["emitter_id"], emitter.to_string());
        assert_eq!(m.value["content_class"], "unrestricted");
        assert_eq!(m.value["decode_id"], decode.id.to_string());
        assert_eq!(m.value["crc_status"], "valid");
        assert_eq!(m.value["identity"]["scheme"], "adsb-icao");
        assert_eq!(m.value["metadata"]["icao"], decode.metadata["icao"]);
        assert_eq!(m.value["t"], decode.t.as_unix_nanos());
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// Readers fail closed on a message record whose class is missing or unknown.
#[test]
fn reader_fails_closed_on_missing_or_unknown_record_class() {
    for line in [
        r#"{"type":"message","seq":1,"metadata":{}}"#,
        r#"{"type":"message","seq":2,"content_class":"public","metadata":{}}"#,
    ] {
        let Record::Message(m) =
            hk_stream::client::parse_record(StreamKind::Messages, line.as_bytes()).unwrap()
        else {
            panic!()
        };
        assert_eq!(m.content_class, ContentClass::MetadataOnly);
    }
}

/// Wrong-kind publishes and oversize records are refused without consuming a seq.
#[test]
fn wrong_kind_and_oversize_records_are_refused() {
    let mut p = Publisher::new(
        header_for(StreamKind::Messages, ContentClass::Unrestricted),
        small_config(),
    )
    .unwrap();
    let err = p
        .publish_binary(BinaryRecord {
            t: Timestamp::now(),
            sample_index: 0,
            flags: RecordFlags::empty(),
            payload: &[0; 4],
        })
        .unwrap_err();
    assert!(matches!(err, StreamError::WrongKind { .. }));
    let mut big = message(ContentClass::Unrestricted, "a1b2c3", None);
    big.metadata = json!({"blob": "x".repeat(70_000)});
    assert!(matches!(
        p.publish_message(&big),
        Err(StreamError::Frame(_))
    ));
    assert_eq!(p.next_seq(), 0);

    let mut iq = Publisher::new(
        header_for(StreamKind::Iq, ContentClass::Unrestricted),
        small_config(),
    )
    .unwrap();
    assert!(matches!(
        iq.publish_binary(BinaryRecord {
            t: Timestamp::now(),
            sample_index: 0,
            flags: RecordFlags::empty(),
            payload: &vec![0; 64 * 1024],
        }),
        Err(StreamError::Frame(_))
    ));
    assert_eq!(iq.next_seq(), 0);
}
