//! T-022a WebSocket bridge (docs/stream-contract.md §10): token auth, consumer cap, a slow
//! browser dropped without stalling the producer, and 1:1 framing.
//!
//! Use cases served: AWARE-042 and SPACE-050 are the history views this bridge feeds; the live
//! waterfall is their live half (C39).

use std::net::{SocketAddr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use hk_api::stream::client::parse_record;
use hk_api::stream::{
    BinaryRecord, CloseReason, ConsumerState, MessageRecord, Publisher, PublisherConfig, Record,
    RecordFlags, StreamHeader, StreamKind,
};
use hk_api::{ApiState, Server, ServerConfig, StreamRegistry, Token};
use hk_model::{ContentClass, Timestamp};
use serde_json::json;
use tungstenite::client::IntoClientRequest;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

const TOKEN: &str = "t022a-test-token-0123456789abcdef";
type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

fn serve(registry: &StreamRegistry) -> Server {
    let config = ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        Token::from_config(TOKEN).unwrap(),
    );
    let state = ApiState {
        streams: registry.clone(),
        ..ApiState::default()
    };
    Server::start(config, state).unwrap()
}

fn connect(addr: SocketAddr, path: &str) -> Result<Ws, tungstenite::Error> {
    let (mut ws, _) = tungstenite::connect(format!("ws://{addr}{path}"))?;
    if let MaybeTlsStream::Plain(s) = ws.get_mut() {
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    }
    Ok(ws)
}

fn authed(addr: SocketAddr, stream_id: &str) -> Result<Ws, tungstenite::Error> {
    connect(addr, &format!("/ws/{stream_id}?token={TOKEN}"))
}

fn status_of(r: Result<Ws, tungstenite::Error>) -> u16 {
    match r {
        Err(tungstenite::Error::Http(resp)) => resp.status().as_u16(),
        Err(e) => panic!("expected an HTTP refusal, got {e}"),
        Ok(_) => panic!("expected an HTTP refusal, got an upgrade"),
    }
}

fn wait_for(what: &str, f: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(2));
    }
}

fn spectrum_header(id: &str, class: ContentClass, rows_per_s: f64, bins: u32) -> StreamHeader {
    let mut h = StreamHeader::new(id, StreamKind::Spectrum, class, "hk-api-test");
    h.datatype = Some("rf32_le".into());
    h.fft_size = Some(bins);
    h.sample_rate_hz = Some(rows_per_s);
    h.center_hz = Some(100e6);
    h.bandwidth_hz = Some(2.4e6);
    h.max_frame_len = 32 + 4 * bins;
    h
}

fn row(i: u64, bins: usize) -> Vec<u8> {
    (0..bins)
        .flat_map(|b| (-100.0 + i as f32 + b as f32 * 0.01).to_le_bytes())
        .collect()
}

/// Data messages until the server closes the connection.
fn drain(ws: &mut Ws) -> Vec<Message> {
    let mut out = Vec::new();
    loop {
        match ws.read() {
            Ok(m @ (Message::Text(_) | Message::Binary(_))) => out.push(m),
            Ok(_) => {}
            Err(_) => return out,
        }
    }
}

fn header_of(m: &Message) -> StreamHeader {
    match m {
        Message::Text(t) => StreamHeader::from_json_bytes(t.as_str().as_bytes()).unwrap(),
        other => panic!("first message must be the text header, got {other:?}"),
    }
}

#[test]
fn bridge_rejects_missing_and_bad_tokens() {
    let registry = StreamRegistry::new();
    let publisher = Publisher::new(
        spectrum_header("spectrum/a", ContentClass::Unrestricted, 30.0, 16),
        PublisherConfig::default(),
    )
    .unwrap();
    let handle = publisher.handle();
    registry.register(publisher.header(), handle.clone());
    let server = serve(&registry);
    let addr = server.local_addr();

    assert_eq!(status_of(connect(addr, "/ws/spectrum/a")), 401, "no token");
    assert_eq!(
        status_of(connect(
            addr,
            "/ws/spectrum/a?token=wrong-token-0123456789abcdef"
        )),
        401,
        "bad token"
    );
    let prefix = &TOKEN[..TOKEN.len() - 1];
    assert_eq!(
        status_of(connect(addr, &format!("/ws/spectrum/a?token={prefix}"))),
        401,
        "token prefix"
    );
    assert_eq!(
        status_of(connect(addr, "/ws/nope")),
        401,
        "auth before existence"
    );
    assert_eq!(
        handle.open_consumers(),
        0,
        "refused attempts subscribe nothing"
    );
    assert_eq!(status_of(authed(addr, "nope")), 404);

    // Query-parameter token (the browser path) and bearer header (scripts) both admit.
    let mut ws = authed(addr, "spectrum/a").unwrap();
    assert_eq!(header_of(&ws.read().unwrap()), *publisher.header());
    let mut req = format!("ws://{addr}/ws/spectrum/a")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("Authorization", format!("Bearer {TOKEN}").parse().unwrap());
    let (mut ws2, _) = tungstenite::connect(req).unwrap();
    assert_eq!(header_of(&ws2.read().unwrap()), *publisher.header());
    assert_eq!(handle.open_consumers(), 2);
}

#[test]
fn bridge_enforces_the_publisher_consumer_cap() {
    let registry = StreamRegistry::new();
    let config = PublisherConfig {
        max_consumers: 2,
        ..PublisherConfig::default()
    };
    let publisher = Publisher::new(
        spectrum_header("spectrum/cap", ContentClass::Unrestricted, 30.0, 16),
        config,
    )
    .unwrap();
    let handle = publisher.handle();
    registry.register(publisher.header(), handle.clone());
    let server = serve(&registry);
    let addr = server.local_addr();

    let a = authed(addr, "spectrum/cap").unwrap();
    let _b = authed(addr, "spectrum/cap").unwrap();
    assert_eq!(handle.open_consumers(), 2);
    assert_eq!(
        status_of(authed(addr, "spectrum/cap")),
        503,
        "third consumer refused"
    );
    assert_eq!(handle.open_consumers(), 2);

    // A browser that goes away frees its slot (the bridge closes consumers on hang-up).
    drop(a);
    wait_for("hang-up reaped", || handle.open_consumers() == 1);
    let _c = authed(addr, "spectrum/cap").expect("slot reusable");
    assert_eq!(handle.open_consumers(), 2);
}

#[test]
fn bridge_maps_frames_one_to_one() {
    let registry = StreamRegistry::new();
    let bins = 32usize;
    let mut spectrum = Publisher::new(
        spectrum_header(
            "spectrum/f",
            ContentClass::Unrestricted,
            1000.0,
            bins as u32,
        ),
        PublisherConfig::default(),
    )
    .unwrap();
    let mut messages = Publisher::new(
        StreamHeader::new(
            "decodes/f",
            StreamKind::Messages,
            ContentClass::Unrestricted,
            "hk-api-test",
        ),
        PublisherConfig::default(),
    )
    .unwrap();
    registry.register(spectrum.header(), spectrum.handle());
    registry.register(messages.header(), messages.handle());
    let server = serve(&registry);
    let mut ws_spec = authed(server.local_addr(), "spectrum/f").unwrap();
    let mut ws_msg = authed(server.local_addr(), "decodes/f").unwrap();

    for i in 0..40u64 {
        let flags = if i == 7 {
            RecordFlags::OVERLOAD
        } else {
            RecordFlags::empty()
        };
        spectrum
            .publish_binary(BinaryRecord {
                t: Timestamp::from_unix_nanos(1_789_300_800_000_000_000 + i as i64 * 40_000_000),
                sample_index: i * 4096,
                flags,
                payload: &row(i, bins),
            })
            .unwrap();
    }
    for i in 0..5u64 {
        messages
            .publish_message(&MessageRecord {
                t: Timestamp::from_unix_nanos(1_789_300_800_000_000_000 + i as i64),
                emitter_id: None,
                provenance_ref: None,
                content_class: ContentClass::Unrestricted,
                decode_id: None,
                annotation_id: None,
                decoder: Some("test@1".into()),
                frame_model: Some("test-frame".into()),
                crc_status: None,
                identity: None,
                metadata: json!({ "n": i }),
                content: Some(json!({ "text": format!("hello {i}") })),
            })
            .unwrap();
    }
    let (spec_header, msg_header) = (spectrum.header().clone(), messages.header().clone());
    spectrum.finish();
    messages.finish();

    let msgs = drain(&mut ws_spec);
    assert_eq!(msgs.len(), 41, "header + one message per record");
    assert_eq!(header_of(&msgs[0]), spec_header);
    for (i, m) in msgs[1..].iter().enumerate() {
        let Message::Binary(b) = m else {
            panic!("record {i} of a binary stream must be a binary message")
        };
        let Record::Binary(d) = parse_record(StreamKind::Spectrum, b).unwrap() else {
            panic!("record {i}: data record expected")
        };
        let i = i as u64;
        assert_eq!(d.header.seq, i);
        assert_eq!(d.header.sample_index, i * 4096);
        assert_eq!(
            d.header.t.as_unix_nanos(),
            1_789_300_800_000_000_000 + i as i64 * 40_000_000
        );
        assert_eq!(d.header.flags.contains(RecordFlags::OVERLOAD), i == 7);
        assert_eq!(d.payload, row(i, bins));
    }

    let msgs = drain(&mut ws_msg);
    assert_eq!(msgs.len(), 6);
    assert_eq!(header_of(&msgs[0]), msg_header);
    for (i, m) in msgs[1..].iter().enumerate() {
        let Message::Text(t) = m else {
            panic!("messages records are text messages")
        };
        assert!(t.as_str().ends_with('\n'), "NDJSON record verbatim");
        let Record::Message(env) =
            parse_record(StreamKind::Messages, t.as_str().as_bytes()).unwrap()
        else {
            panic!("message record expected")
        };
        assert_eq!(env.seq, i as u64);
        assert_eq!(env.value["content"]["text"], json!(format!("hello {i}")));
    }
}

#[test]
fn slow_browser_is_dropped_while_the_producer_keeps_rate() {
    const PAYLOAD: usize = 64 * 1024;
    const N: u64 = 625;
    let period = Duration::from_millis(4); // 250 records/s ≈ 16 MB/s for 2.5 s
    let registry = StreamRegistry::new();
    let mut header = StreamHeader::new(
        "iq/fast",
        StreamKind::Iq,
        ContentClass::Unrestricted,
        "hk-api-test",
    );
    header.datatype = Some("ci8".into());
    header.sample_rate_hz = Some(20e6);
    header.max_frame_len = (32 + PAYLOAD) as u32;
    let config = PublisherConfig {
        queue_bytes: 2 << 20,
        disconnect_after: Duration::from_millis(300),
        ..PublisherConfig::default()
    };
    let mut publisher = Publisher::new(header, config).unwrap();
    let handle = publisher.handle();
    registry.register(publisher.header(), handle.clone());
    let server = serve(&registry);

    let mut slow = authed(server.local_addr(), "iq/fast").unwrap();
    header_of(&slow.read().unwrap()); // then never reads again
    let slow_label = match slow.get_ref() {
        MaybeTlsStream::Plain(s) => format!("ws:{}", s.local_addr().unwrap()),
        _ => unreachable!(),
    };
    let mut fast = authed(server.local_addr(), "iq/fast").unwrap();
    let reader = thread::spawn(move || {
        let (mut records, mut dropped) = (0u64, 0u64);
        loop {
            match fast.read() {
                Ok(Message::Binary(b)) => match parse_record(StreamKind::Iq, &b).unwrap() {
                    Record::Binary(_) => records += 1,
                    Record::Dropped(m) => dropped += m.count,
                    _ => {}
                },
                Ok(_) => {}
                Err(_) => return (records, dropped),
            }
        }
    });
    wait_for("both consumers", || handle.open_consumers() == 2);

    let payload = vec![0x5au8; PAYLOAD];
    let start = Instant::now();
    let mut worst = Duration::ZERO;
    for i in 0..N {
        let due = start + period * i as u32;
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            thread::sleep(wait);
        }
        let t0 = Instant::now();
        publisher
            .publish_binary(BinaryRecord {
                t: Timestamp::from_unix_nanos(i as i64),
                sample_index: i * PAYLOAD as u64 / 2,
                flags: RecordFlags::empty(),
                payload: &payload,
            })
            .unwrap();
        worst = worst.max(t0.elapsed());
    }
    let elapsed = start.elapsed();
    let scheduled = period * N as u32;
    let slow_stats = handle
        .consumer_stats()
        .into_iter()
        .find(|s| s.label == slow_label)
        .expect("slow consumer stats");
    publisher.finish();
    let (records, dropped) = reader.join().unwrap();

    eprintln!(
        "slow browser: {slow_stats:?}; producer {elapsed:?} for {scheduled:?} scheduled, worst publish {worst:?}; fast browser {records} records + {dropped} dropped"
    );
    assert_eq!(
        slow_stats.state,
        ConsumerState::Closed(CloseReason::SlowConsumer),
        "the stalled browser is disconnected"
    );
    assert!(slow_stats.records_dropped > 0);
    assert!(
        worst < Duration::from_millis(100),
        "a publish call waited on a browser: {worst:?}"
    );
    assert!(
        elapsed < scheduled + Duration::from_millis(1500),
        "producer fell behind its schedule: {elapsed:?} vs {scheduled:?}"
    );
    assert_eq!(
        records + dropped,
        N,
        "the reading browser accounts for every seq (records + drop markers)"
    );
    assert!(records >= N / 2, "the reading browser kept receiving");
}
