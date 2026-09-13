//! Regressions for the T-016/T-014 independent re-probe (egress enforcement gaps):
//! - N4b: own-key locality enforced per consumer, not only in `bind_tcp`;
//! - N4d: metadata policy applied at egress to in-process producers;
//! - N3: gated spectrum row rate and payload size enforced per row, not only as declared.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hk_model::{ContentClass, DecodedIdentity, IdentityScheme, Timestamp};
use hk_stream::{
    BinaryRecord, Declared, GATED_SPECTRUM_BURST_ROWS, ListenAddr, Listener, MessageRecord,
    MetadataPolicy, MetadataType, Publisher, PublisherConfig, Record, RecordFlags,
    SpectrumGateReason, StreamError, StreamHeader, StreamKind, StreamReader,
};
use serde_json::json;

const SENTINEL: &str = "SENTINEL-EGRESS-51d2";

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

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

fn config() -> PublisherConfig {
    PublisherConfig {
        queue_bytes: 1024 * 1024,
        ..PublisherConfig::default()
    }
}

fn messages(class: ContentClass) -> StreamHeader {
    let mut h = StreamHeader::new("m", StreamKind::Messages, class, "egress-tests");
    h.max_frame_len = 64 * 1024;
    h
}

fn record(class: ContentClass) -> MessageRecord {
    MessageRecord {
        t: Timestamp::now(),
        emitter_id: None,
        provenance_ref: None,
        content_class: class,
        decode_id: None,
        annotation_id: None,
        decoder: Some("in process decoder INPROC-DEC".into()),
        frame_model: Some("INPROC FM".into()),
        crc_status: None,
        identity: Some(DecodedIdentity {
            scheme: IdentityScheme::AdsbIcao,
            value: "INPROC-ID".into(),
        }),
        metadata: json!({"free_text": "INPROC-PAGER-TEXT", "function": 2}),
        content: Some(json!({"text": SENTINEL})),
    }
}

/// Reads a whole TCP connection in a thread.
fn tcp_reader(addr: std::net::SocketAddr) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = TcpStream::connect(addr).unwrap().read_to_end(&mut bytes);
        bytes
    })
}

/// N4b: subscribing a `TcpStream` directly (the path a bridge or future `bind_*` takes) on an
/// own-key stream is refused, as is a TCP stream declared local or any writer declared remote;
/// a Unix socket still receives own-key content. Unrestricted streams accept TCP (control).
#[test]
fn n4b_direct_remote_subscribe_on_own_key_stream_is_refused() {
    let mut p = Publisher::new(messages(ContentClass::OwnKeyDecrypted), config()).unwrap();
    let h = p.handle();

    let tl = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = tcp_reader(tl.local_addr().unwrap());
    let (srv, _) = tl.accept().unwrap();
    let srv2 = srv.try_clone().unwrap();
    let closer = srv.try_clone().unwrap();
    let err = h
        .subscribe(
            "bridge",
            srv,
            Box::new(move |_| {
                let _ = closer.shutdown(Shutdown::Both);
            }),
        )
        .expect_err("TcpStream on own-key refused");
    assert!(
        matches!(
            err,
            StreamError::LocalOnly {
                class: ContentClass::OwnKeyDecrypted
            }
        ),
        "{err:?}"
    );
    assert!(matches!(
        h.subscribe("lying", Declared::local(srv2), Box::new(|_| {})),
        Err(StreamError::LocalOnly { .. })
    ));
    assert!(matches!(
        h.subscribe(
            "ws-bridge",
            Declared::remote(SharedBuf::default()),
            Box::new(|_| {})
        ),
        Err(StreamError::LocalOnly { .. })
    ));
    assert_eq!(h.gate_stats().remote_consumers_refused, 3);
    assert_eq!(h.open_consumers(), 0);

    let (ours, theirs) = UnixStream::pair().unwrap();
    let closer = ours.try_clone().unwrap();
    h.subscribe(
        "uds",
        ours,
        Box::new(move |_| {
            let _ = closer.shutdown(Shutdown::Both);
        }),
    )
    .expect("a Unix socket is local");
    let local = thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut theirs = theirs;
        theirs.read_to_end(&mut bytes).unwrap();
        bytes
    });
    p.publish_message(&record(ContentClass::OwnKeyDecrypted))
        .unwrap();
    drop(p);
    assert!(contains(&local.join().unwrap(), SENTINEL), "UDS works");
    assert!(
        !contains(&client.join().unwrap(), SENTINEL),
        "nothing on TCP"
    );

    // Positive control: TCP consumers of an unrestricted stream are fine.
    let p = Publisher::new(messages(ContentClass::Unrestricted), config()).unwrap();
    let tl = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = tcp_reader(tl.local_addr().unwrap());
    let (srv, _) = tl.accept().unwrap();
    p.handle()
        .subscribe("tcp", srv, Box::new(|_| {}))
        .expect("unrestricted over TCP");
    drop(p);
    client.join().unwrap();
}

/// N4d: an in-process producer publishes a restricted-paging record with free-text metadata,
/// frame model, identity and decoder on an unrestricted TCP stream. Without a policy everything
/// outside the host fields is stripped; with a policy only allowlisted typed keys survive; a
/// restricted messages stream cannot be created without a policy.
#[test]
fn n4d_in_process_restricted_metadata_is_reduced_at_egress() {
    for class in [
        ContentClass::MetadataOnly,
        ContentClass::RestrictedCellular,
        ContentClass::RestrictedPaging,
    ] {
        assert!(
            matches!(
                Publisher::new(messages(class), config()),
                Err(StreamError::MetadataPolicyRequired { .. })
            ),
            "{class:?}"
        );
    }
    let policy = MetadataPolicy {
        keys: [("function".to_owned(), MetadataType::Integer)].into(),
        ..MetadataPolicy::default()
    };
    assert!(
        Publisher::with_metadata_policy(
            messages(ContentClass::RestrictedPaging),
            config(),
            policy.clone()
        )
        .is_ok()
    );

    for with_policy in [false, true] {
        let mut header = messages(ContentClass::Unrestricted);
        header.message_schema = Some("hackriff.pager/1".into());
        let mut p = if with_policy {
            Publisher::with_metadata_policy(header, config(), policy.clone()).unwrap()
        } else {
            Publisher::new(header, config()).unwrap()
        };
        let h = p.handle();
        let listener = Listener::bind_tcp("127.0.0.1:0", h.clone()).unwrap();
        let ListenAddr::Tcp(addr) = listener.addr().clone() else {
            unreachable!()
        };
        let client = tcp_reader(addr);
        let deadline = Instant::now() + Duration::from_secs(10);
        while h.open_consumers() == 0 {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        p.publish_message(&record(ContentClass::RestrictedPaging))
            .unwrap();
        // Positive control: the same fields on an unrestricted record flow.
        p.publish_message(&record(ContentClass::Unrestricted))
            .unwrap();
        let removed = h.gate_stats().metadata_fields_sanitized;
        drop(p);
        let wire = client.join().unwrap();
        drop(listener);

        let mut r = StreamReader::new(&wire[..]);
        let Some(Record::Message(gated)) = r.next_record().unwrap() else {
            panic!()
        };
        let Some(Record::Message(open)) = r.next_record().unwrap() else {
            panic!()
        };
        assert!(gated.gated);
        let v = &gated.value;
        let expected_meta = if with_policy {
            json!({"function": 2})
        } else {
            json!({})
        };
        assert_eq!(v["metadata"], expected_meta, "with_policy={with_policy}");
        assert_eq!(v["frame_model"], "hackriff.pager/1", "replaced by schema");
        assert!(v.get("identity").is_none());
        assert!(v.get("decoder").is_none());
        assert!(v.get("content").is_none());
        let gated_text = v.to_string();
        for token in ["INPROC", SENTINEL] {
            assert!(!gated_text.contains(token), "{token} in {gated_text}");
        }
        assert_eq!(removed, if with_policy { 4 } else { 5 });
        assert_eq!(open.value["metadata"]["free_text"], "INPROC-PAGER-TEXT");
        assert!(contains(&wire, SENTINEL), "positive control: wire scan");
    }
}

fn spectrum(class: ContentClass, rate: f64) -> Publisher {
    let mut h = StreamHeader::new("spec", StreamKind::Spectrum, class, "egress-tests");
    h.datatype = Some("rf32_le".into());
    h.fft_size = Some(64);
    h.max_frame_len = 64 * 1024;
    h.sample_rate_hz = Some(rate);
    Publisher::new(
        h,
        PublisherConfig {
            queue_bytes: 64 * 1024 * 1024,
            ..PublisherConfig::default()
        },
    )
    .unwrap()
}

fn row(t: Timestamp, i: u64, payload: &[u8]) -> BinaryRecord<'_> {
    BinaryRecord {
        t,
        sample_index: i,
        flags: RecordFlags::empty(),
        payload,
    }
}

/// N3: a stream declared at 10 rows/s is offered 10 000 16 kB rows and then 10 000 in-cap rows
/// as fast as possible. Oversize rows never leave; in-cap rows leave at most at the declared rate
/// (plus the burst); every seq is delivered or covered by a counted GATED marker.
#[test]
fn n3_gated_spectrum_rate_and_payload_are_enforced_per_row() {
    const RATE: f64 = 10.0;
    const N: u64 = 10_000;
    let mut p = spectrum(ContentClass::RestrictedPaging, RATE);
    let h = p.handle();
    let buf = SharedBuf::default();
    h.subscribe("mem", Declared::local(buf.clone()), Box::new(|_| {}))
        .unwrap();

    let oversize = vec![0x7fu8; 16_000];
    for i in 0..N {
        match p.publish_binary(row(Timestamp::now(), i, &oversize)) {
            Err(StreamError::SpectrumGated {
                reason: SpectrumGateReason::PayloadLen,
                ..
            }) => {}
            other => panic!("oversize row {i}: {other:?}"),
        }
    }
    let in_cap = vec![0x11u8; 64 * 4];
    let start = Instant::now();
    let mut accepted = 0u64;
    for i in 0..N {
        match p.publish_binary(row(Timestamp::now(), N + i, &in_cap)) {
            Ok(_) => accepted += 1,
            Err(StreamError::SpectrumGated {
                reason: SpectrumGateReason::RowRate,
                ..
            }) => {}
            Err(e) => panic!("{e:?}"),
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    let bound = RATE * elapsed + GATED_SPECTRUM_BURST_ROWS;
    println!("N3: {N} rows in {elapsed:.3}s, {accepted} delivered (bound {bound:.2})");
    assert!(accepted >= 1);
    assert!(accepted as f64 <= bound.floor(), "{accepted} > {bound}");
    assert_eq!(h.gate_stats().spectrum_rows_gated, 2 * N - accepted);
    drop(p);
    assert!(h.wait_closed(Duration::from_secs(30)));

    let bytes = buf.0.lock().unwrap().clone();
    assert!(!contains(&bytes, "\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f"));
    let mut r = StreamReader::new(&bytes[..]);
    let (mut next, mut data, mut gated_markers) = (0u64, 0u64, 0u64);
    while let Some(rec) = r.next_record().unwrap() {
        match rec {
            Record::Binary(b) => {
                assert_eq!(b.header.seq, next);
                assert!(b.payload.len() <= 64 * 4);
                next += 1;
                data += 1;
            }
            Record::Dropped(d) => {
                assert_eq!(d.first_seq, next);
                assert!(d.gated, "only gate markers: the queue never filled");
                next += d.count;
                gated_markers += 1;
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(data, accepted);
    assert_eq!(
        next,
        2 * N,
        "every seq delivered or covered by a gated marker"
    );
    assert!(gated_markers >= 1 && gated_markers <= accepted + 1);
}

/// The row rate is also enforced on `t` spacing (rows arriving slowly but stamped 1 ms apart)
/// and `t` may not go backwards; a gated spectrum stream must declare its geometry; permitting
/// classes are not enforced (control).
#[test]
fn gated_spectrum_enforces_t_spacing_monotonic_time_and_geometry() {
    let mut p = spectrum(ContentClass::MetadataOnly, 10.0);
    let payload = [0u8; 16];
    let base = 1_757_000_000_000_000_000i64;
    let mut results = Vec::new();
    for i in 0..4u64 {
        let t = Timestamp::from_unix_nanos(base + i as i64 * 1_000_000);
        results.push(p.publish_binary(row(t, i, &payload)).is_ok());
        thread::sleep(Duration::from_millis(110));
    }
    assert_eq!(
        results,
        [true, true, false, false],
        "burst of 2, then t spacing"
    );

    let mut p = spectrum(ContentClass::RestrictedCellular, 10.0);
    assert!(
        p.publish_binary(row(Timestamp::from_unix_nanos(base), 0, &payload))
            .is_ok()
    );
    thread::sleep(Duration::from_millis(250));
    assert!(matches!(
        p.publish_binary(row(
            Timestamp::from_unix_nanos(base - 5_000_000_000),
            1,
            &payload
        )),
        Err(StreamError::SpectrumGated {
            reason: SpectrumGateReason::RowRate,
            ..
        })
    ));

    for missing in ["fft_size", "datatype"] {
        let mut h = StreamHeader::new("s", StreamKind::Spectrum, ContentClass::MetadataOnly, "t");
        h.datatype = Some("rf32_le".into());
        h.fft_size = Some(64);
        h.sample_rate_hz = Some(10.0);
        match missing {
            "fft_size" => h.fft_size = None,
            _ => h.datatype = Some("not-a-type".into()),
        }
        assert!(
            matches!(
                Publisher::new(h, config()),
                Err(StreamError::SpectrumGeometry { missing: m, .. }) if m == missing
            ),
            "{missing}"
        );
    }

    let mut open = spectrum(ContentClass::Unrestricted, 10.0);
    let big = vec![0u8; 16_000];
    for i in 0..1000 {
        open.publish_binary(row(Timestamp::now(), i, &big))
            .expect("permitting classes are not rate-enforced");
    }
}
