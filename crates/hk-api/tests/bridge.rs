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
    // T-417: finishing a publisher no longer ends the connection by itself — a retune finishes one
    // and offers the next under the same id. A stream that is really over withdraws its id, which
    // is what the pipeline's `stream_unsink` does, and that is what closes these sockets.
    spectrum.finish();
    messages.finish();
    registry.unregister("spectrum/f");
    registry.unregister("decodes/f");

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

/// T-417 (the user, 2026-09-17): *"a settle gap is fine … so connected consumers keep receiving
/// after the retune"*. A retune finishes the spectrum publisher and offers a new one under the same
/// id (a header must describe every row after it, T-057); a re-plumb rebuilds every reader and does
/// the same. The **connection** is not what moved, so the bridge carries it across: the socket
/// stays open and the next publisher's header arrives on it as another text message.
///
/// The counterpart — a consumer that cannot keep up is still dropped deliberately (§7, T-388) — is
/// `slow_browser_is_dropped_while_the_producer_keeps_rate` below, unchanged by this.
#[test]
fn a_new_publisher_under_the_same_id_keeps_the_browser_connected() {
    let registry = StreamRegistry::new();
    let bins = 16usize;
    let mut first = Publisher::new(
        spectrum_header(
            "spectrum/live",
            ContentClass::Unrestricted,
            30.0,
            bins as u32,
        ),
        PublisherConfig::default(),
    )
    .unwrap();
    registry.register(first.header(), first.handle());
    let server = serve(&registry);
    let mut ws = authed(server.local_addr(), "spectrum/live").unwrap();
    assert_eq!(header_of(&ws.read().unwrap()).center_hz, Some(100e6));
    for i in 0..3u64 {
        first
            .publish_binary(BinaryRecord {
                t: Timestamp::from_unix_nanos(1_789_300_800_000_000_000 + i as i64 * 40_000_000),
                sample_index: i * 4096,
                flags: RecordFlags::empty(),
                payload: &row(i, bins),
            })
            .unwrap();
    }
    for _ in 0..3 {
        assert!(matches!(ws.read().unwrap(), Message::Binary(_)));
    }

    // The retune: this publisher is finished and the next window is offered under the same id.
    // Nothing is published in between — the settle gap is real and is not covered over.
    first.finish();
    let mut header = spectrum_header(
        "spectrum/live",
        ContentClass::Unrestricted,
        30.0,
        bins as u32,
    );
    header.center_hz = Some(101.8e6);
    let mut second = Publisher::new(header, PublisherConfig::default()).unwrap();
    let handle2 = second.handle();
    registry.register(second.header(), handle2.clone());

    // The same socket, never reconnected, receives the new header and then the new window's rows.
    let hd = loop {
        match ws.read().unwrap() {
            Message::Text(t) => {
                break StreamHeader::from_json_bytes(t.as_str().as_bytes()).unwrap();
            }
            Message::Binary(_) => panic!("a row of the old window after it finished"),
            _ => {}
        }
    };
    assert_eq!(hd.center_hz, Some(101.8e6), "the new window's header");
    assert_eq!(hd.stream_id, "spectrum/live", "the same stream id");
    wait_for("the carried-over consumer", || {
        handle2.open_consumers() == 1
    });
    second
        .publish_binary(BinaryRecord {
            t: Timestamp::from_unix_nanos(1_789_300_801_000_000_000),
            sample_index: 0,
            flags: RecordFlags::empty(),
            payload: &row(9, bins),
        })
        .unwrap();
    let Message::Binary(b) = ws.read().unwrap() else {
        panic!("a row of the new window")
    };
    let Record::Binary(d) = parse_record(StreamKind::Spectrum, &b).unwrap() else {
        panic!("data record expected")
    };
    assert_eq!(d.payload, row(9, bins));

    // And a stream that is really over — the id withdrawn — still ends the connection.
    second.finish();
    registry.unregister("spectrum/live");
    assert!(
        drain(&mut ws)
            .iter()
            .all(|m| !matches!(m, Message::Text(_))),
        "no third header: there is no third window"
    );
}

/// T-425: the carry-over must be **driven by the offer**, not by `bridge::WATCH_TICK`.
///
/// The test above waits for the carried-over consumer before it publishes, so it never noticed
/// that the re-attach used to be quantised to the 50 ms tick. The pipeline does not wait: the
/// spectrum reader offers the new publisher when the new segment's first samples arrive and
/// publishes that window's first row one row period later (~40 ms at the live rate). A
/// tick-quantised re-attach therefore swallowed the first one or two rows of every new window,
/// silently — the consumer was not subscribed, so there is no drop marker to say so — and made
/// each retune look like a longer break in the air than it was. That is the coverage lie told in
/// the other direction, and it also made `hk-cli::api_contract`'s seam assertion meaningless: a
/// mutation that papered the seam over completely still passed, because the swallowed rows forged
/// a gap that was not there.
///
/// Measured: with `StreamRegistry::wait_for_offer_after` the consumer is re-subscribed in tens of
/// microseconds. The bound below is half of `WATCH_TICK`, which is three orders of magnitude above
/// that and still fails outright if the wait goes back to polling on the tick.
#[test]
fn the_carry_over_re_subscribes_on_the_offer_not_on_the_watch_tick() {
    let registry = StreamRegistry::new();
    let bins = 16usize;
    let mut first = Publisher::new(
        spectrum_header(
            "spectrum/live",
            ContentClass::Unrestricted,
            30.0,
            bins as u32,
        ),
        PublisherConfig::default(),
    )
    .unwrap();
    registry.register(first.header(), first.handle());
    let server = serve(&registry);
    let mut ws = authed(server.local_addr(), "spectrum/live").unwrap();
    assert_eq!(header_of(&ws.read().unwrap()).center_hz, Some(100e6));
    first
        .publish_binary(BinaryRecord {
            t: Timestamp::from_unix_nanos(1_789_300_800_000_000_000),
            sample_index: 0,
            flags: RecordFlags::empty(),
            payload: &row(0, bins),
        })
        .unwrap();
    assert!(matches!(ws.read().unwrap(), Message::Binary(_)));

    // The seam, in the shape a re-plumb has it: the old publisher finishes when the old segment
    // tears down, and the new one is offered only after the whole re-plumb, so the watcher is
    // already parked in the settle gap when the offer lands. Noticing the finish is still
    // tick-paced and is not what this measures; the sleep puts the watcher past that point, so the
    // clock below times the offer → re-subscribe step alone.
    let handle1 = first.handle();
    first.finish();
    wait_for("the publisher to finish", || handle1.open_consumers() == 0);
    thread::sleep(Duration::from_millis(150)); // > 2 × bridge::WATCH_TICK
    let mut header = spectrum_header(
        "spectrum/live",
        ContentClass::Unrestricted,
        30.0,
        bins as u32,
    );
    header.center_hz = Some(101.8e6);
    let second = Publisher::new(header, PublisherConfig::default()).unwrap();
    let handle2 = second.handle();
    let offered = Instant::now();
    registry.register(second.header(), handle2.clone());
    while handle2.open_consumers() == 0 {
        assert!(
            offered.elapsed() < Duration::from_millis(25),
            "the carried-over consumer took {:?} to re-subscribe: the bridge is waiting on a \
             timer again, and the producer's first rows of the new window are being lost",
            offered.elapsed()
        );
        std::hint::spin_loop();
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
    registry.unregister("iq/fast"); // T-417: what ends a connection is the id going away
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

// ---------------------------------------------------------------------------
// T-531: a registration has a lifetime
// ---------------------------------------------------------------------------

fn bits_header(id: &str) -> StreamHeader {
    let mut h = StreamHeader::new(
        id,
        StreamKind::Bits,
        ContentClass::Unrestricted,
        "hk-api-test",
    );
    h.datatype = Some("ru8".into());
    h.center_hz = Some(915e6);
    h.bandwidth_hz = Some(50e3);
    h
}

/// The `publish_bits` shape: one publisher per emitter, created, registered, written and finished
/// inside one call, so nothing is ever attached to it. Before T-531 each one stayed in the map for
/// the life of the run — 1299 entries / 659 KB after 40 minutes of sweeping (T-525). The registry
/// must instead hold only what it is offering now.
#[test]
fn a_long_sweep_of_per_emitter_streams_does_not_accumulate() {
    const EMITTERS: u32 = 4000;
    let linger = Duration::from_millis(30);
    let registry = StreamRegistry::with_linger(linger);
    let mut peak = 0;
    for i in 0..EMITTERS {
        let header = bits_header(&format!("bits/fsk-bursts/{i}"));
        let publisher = Publisher::new(header.clone(), PublisherConfig::default()).unwrap();
        registry.register(&header, publisher.handle());
        publisher.finish(); // exactly what the FSK chain does: the stream is over on return
        peak = peak.max(registry.len());
        if i % 200 == 0 {
            thread::sleep(linger + Duration::from_millis(5));
        }
    }
    thread::sleep(linger + Duration::from_millis(5));
    let left = registry.len();
    eprintln!("[T-531] {EMITTERS} emitters: peak {peak} entries, {left} left");
    assert!(
        peak <= 250,
        "the registry grew with the sweep: {peak} entries for {EMITTERS} emitters"
    );
    assert!(left <= 1, "spent streams outlived the linger: {left}");
    let listed = registry.listing()["streams"].as_array().unwrap().len();
    assert_eq!(
        listed, left,
        "the listing is the registry, not a capped view"
    );
}

/// The cap is the fail-closed backstop under the linger: even with the linger far in the future,
/// the map does not grow past [`MAX_STREAMS`]. It is not the fix (the linger is) — it is what
/// keeps a pathological offer rate from making the linger's bound large.
#[test]
fn the_registry_is_capped_even_inside_the_linger() {
    let registry = StreamRegistry::with_linger(Duration::from_secs(3600));
    for i in 0..(hk_api::MAX_STREAMS * 4) {
        let header = bits_header(&format!("bits/fsk-bursts/{i}"));
        let publisher = Publisher::new(header.clone(), PublisherConfig::default()).unwrap();
        registry.register(&header, publisher.handle());
        publisher.finish();
    }
    assert!(
        registry.len() <= hk_api::MAX_STREAMS,
        "over the cap: {}",
        registry.len()
    );
}

/// Only *spent* entries are reaped. A publisher that is still live with no browser attached — the
/// spectrum stream on a server nobody has open — is idle, not over, and must stay offered.
#[test]
fn a_live_stream_with_no_consumers_is_never_withdrawn() {
    let registry = StreamRegistry::with_linger(Duration::from_millis(10));
    let live = Publisher::new(
        spectrum_header("spectrum/live", ContentClass::Unrestricted, 30.0, 16),
        PublisherConfig::default(),
    )
    .unwrap();
    registry.register(live.header(), live.handle());
    let spent_header = bits_header("bits/fsk-bursts/7");
    let spent = Publisher::new(spent_header.clone(), PublisherConfig::default()).unwrap();
    registry.register(&spent_header, spent.handle());
    spent.finish();
    assert_eq!(registry.len(), 2, "both are offered while the linger holds");

    thread::sleep(Duration::from_millis(20));
    let ids: Vec<String> = registry.listing()["streams"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["stream_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, vec!["spectrum/live".to_owned()]);
    assert_eq!(live.handle().open_consumers(), 0, "and nobody was attached");
}

/// The linger is [`CARRY_OVER_GRACE`], so the retune carry-over (T-417/T-425) is untouched: a
/// finished publisher whose next offer arrives inside the grace is still there to carry over to,
/// and the browser sees the new window's header on the same socket.
#[test]
fn a_retune_carry_over_is_inside_the_linger() {
    assert!(
        hk_api::FINISHED_LINGER >= hk_api::bridge::CARRY_OVER_GRACE,
        "withdrawing a stream inside the carry-over grace would end a waiting connection"
    );
    let registry = StreamRegistry::new();
    let first = Publisher::new(
        spectrum_header("spectrum/r", ContentClass::Unrestricted, 60.0, 16),
        PublisherConfig::default(),
    )
    .unwrap();
    registry.register(first.header(), first.handle());
    let server = serve(&registry);
    let mut ws = authed(server.local_addr(), "spectrum/r").unwrap();
    wait_for("the browser to attach", || {
        first.handle().open_consumers() == 1
    });
    assert_eq!(header_of(&ws.read().unwrap()).stream_id, "spectrum/r");
    first.finish();

    let mut next = Publisher::new(
        spectrum_header("spectrum/r", ContentClass::Unrestricted, 30.0, 16),
        PublisherConfig::default(),
    )
    .unwrap();
    registry.register(next.header(), next.handle());
    next.publish_binary(BinaryRecord {
        t: Timestamp::from_unix_nanos(1),
        sample_index: 0,
        flags: RecordFlags::empty(),
        payload: &row(0, 16),
    })
    .unwrap();
    // The second header on the same socket: the id survived its publisher finishing.
    let mut saw_next_header = false;
    for _ in 0..8 {
        if let Ok(m) = ws.read() {
            if let Message::Text(_) = &m {
                let h = header_of(&m);
                if h.sample_rate_hz == Some(30.0) {
                    saw_next_header = true;
                    break;
                }
            }
        }
    }
    assert!(saw_next_header, "the connection did not carry over");
}

/// T-530: **a re-plumb is a gap in the stream, not the end of it — for the client that arrives
/// during it, too.**
///
/// The test above carries a consumer that was *already attached* across the gap. One that knocks
/// while the gap is open used to be told `410 Gone, "stream finished"` — a run that was still
/// capturing, described as permanently over. `ops/stage.sh`'s health check is `/` plus exactly
/// this handshake, so a long enough gap read as "hk serve crashed" and restarted the demo under
/// the user (T-525 measured 21.43 s of it before its fix, 0.17 s after).
///
/// The producer now says which end it is ([`hk_api::stream::Publisher::finish_between_windows`] vs
/// `finish`), and the handshake honours it: wait for the successor, else `503 replumbing` — *not
/// now*, never *never again*. A publisher that really finished still answers `410` at once, so a
/// run that has ended is not made to look like a gap.
#[test]
fn a_handshake_between_windows_waits_for_the_successor_and_never_says_gone() {
    let registry = StreamRegistry::new();
    let bins = 16u32;
    let first = Publisher::new(
        spectrum_header("spectrum/live", ContentClass::Unrestricted, 30.0, bins),
        PublisherConfig::default(),
    )
    .unwrap();
    registry.register(first.header(), first.handle());
    let server = serve(&registry);
    let addr = server.local_addr();

    // (1) The gap opens: the segment's publisher is finished, saying a successor is coming.
    first.finish_between_windows();
    let knock = thread::spawn(move || authed(addr, "spectrum/live"));
    thread::sleep(Duration::from_millis(250));
    let mut header = spectrum_header("spectrum/live", ContentClass::Unrestricted, 30.0, bins);
    header.center_hz = Some(101.8e6);
    let second = Publisher::new(header, PublisherConfig::default()).unwrap();
    registry.register(second.header(), second.handle());

    // The client that knocked during the gap gets the upgrade and the *new* window's header. It
    // never learns there was a gap at all, which is the point: nothing it can do about one.
    let mut ws = knock
        .join()
        .unwrap()
        .expect("the handshake waited and upgraded");
    assert_eq!(
        header_of(&ws.read().unwrap()).center_hz,
        Some(101.8e6),
        "the successor's header"
    );
    drop(ws);

    // (2) A gap whose successor never arrives is still not an end: `503`, after a bounded wait.
    second.finish_between_windows();
    let started = Instant::now();
    let refused = authed(addr, "spectrum/live");
    let (status, body) = match refused {
        Err(tungstenite::Error::Http(r)) => (
            r.status().as_u16(),
            String::from_utf8_lossy(r.body().as_deref().unwrap_or(&[])).into_owned(),
        ),
        Err(e) => panic!("expected an HTTP refusal, got {e}"),
        Ok(_) => panic!("expected a refusal: no successor was ever offered"),
    };
    assert_eq!(status, 503, "not 410: the run has not ended, {body}");
    assert!(body.contains("replumbing"), "{body}");
    assert!(
        started.elapsed() >= Duration::from_millis(1500),
        "it refused at once instead of waiting for the successor first: {:?}",
        started.elapsed()
    );

    // (3) …and a publisher that really finished says so at once. A gap and an end are different
    // things, and the fix must not have blurred them.
    let third = Publisher::new(
        spectrum_header("spectrum/live", ContentClass::Unrestricted, 30.0, bins),
        PublisherConfig::default(),
    )
    .unwrap();
    registry.register(third.header(), third.handle());
    third.finish();
    let started = Instant::now();
    assert_eq!(
        status_of(authed(addr, "spectrum/live")),
        410,
        "a finished stream still says it is gone"
    );
    assert!(
        started.elapsed() < Duration::from_millis(1000),
        "the end was treated as a gap and waited out: {:?}",
        started.elapsed()
    );
}

// ---------------------------------------------------------------------------
// T-542: what a sweep does to the spectrum stream's id
// ---------------------------------------------------------------------------

/// **The registry must give a stream that has just gone quiet its whole linger, however long it
/// had been offered before that.**
///
/// The reap aged an entry by `offered` — when it was *registered*. `spectrum/live` is registered
/// once per segment and then offered for as long as that segment lasts, so by the time a re-plumb
/// finishes its publisher the entry is already far older than [`hk_api::FINISHED_LINGER`] and the
/// very next `register` anywhere in the registry withdraws it. Under a sweep that is constant:
/// the FSK-burst chains publish one stream per emitter, so `register` — and with it the reap —
/// runs many times a second.
///
/// Measured on the live HackRF, `POST /api/control/scan` over 1 MHz–6000 MHz at dwell 1 s:
/// `/ws/spectrum/live` answered **404 no such stream** during a re-plumb, which `ops/stage.sh`'s
/// health check reads as a dead server — so the demo was restarted under the user, which is the
/// "the backend goes down" they reported.
#[test]
fn a_stream_offered_longer_than_the_linger_is_not_reaped_the_moment_it_finishes() {
    let linger = Duration::from_millis(300);
    let registry = StreamRegistry::with_linger(linger);
    let live = Publisher::new(
        spectrum_header("spectrum/live", ContentClass::Unrestricted, 30.0, 16),
        PublisherConfig::default(),
    )
    .unwrap();
    registry.register(live.header(), live.handle());

    // The segment runs. This is the whole point: by the time it ends, the *offer* is old.
    thread::sleep(linger * 3);
    assert!(
        registry.offer("spectrum/live").is_some(),
        "a live publisher was withdrawn while it was still publishing"
    );

    // The re-plumb starts: the publisher finishes, saying a successor is coming, and nobody is
    // attached (no browser open — exactly the demo's steady state).
    live.finish_between_windows();

    // A burst chain registers its emitter's stream, which runs the reap.
    for i in 0..3 {
        let h = bits_header(&format!("bits/fsk-bursts/{i}"));
        let p = Publisher::new(h.clone(), PublisherConfig::default()).unwrap();
        registry.register(&h, p.handle());
        p.finish();
    }

    assert!(
        registry.offer("spectrum/live").is_some(),
        "the spectrum stream was withdrawn the instant it went quiet: its linger started when it \
         was registered, not when it fell silent, so a long-lived stream had no grace at all"
    );

    // And the grace still ends: a stream that stays quiet past its own linger is withdrawn.
    thread::sleep(linger * 2);
    let h = bits_header("bits/fsk-bursts/9");
    let p = Publisher::new(h.clone(), PublisherConfig::default()).unwrap();
    registry.register(&h, p.handle());
    p.finish();
    assert!(
        registry.offer("spectrum/live").is_none(),
        "the linger no longer expires, which would make the registry the accumulator T-531 removed"
    );
}

/// **A re-plumb longer than five seconds is still a gap, not an end** — end to end, on the wire.
///
/// This is the user's second symptom, and it needs both halves of T-542 to pass:
///
///  * the registry must still be offering the id (above), or the handshake is `404`;
///  * [`hk_api::stream::BETWEEN_WINDOWS_GRACE`] must still be running, or it is `410 Gone`.
///
/// Measured on the live HackRF under a 1 MHz–6 GHz sweep at dwell 1 s: re-plumbs of 8–19 s are
/// routine (the supervisor joins the old segment's workers, then starts a new one), and
/// `/ws/spectrum/live` answered `410` for every second past the fifth — for 19 s in one episode
/// and 41 s in another. `ops/stage.sh` accepts `101` and `503`; `404` and `410` are what it
/// restarts the server for.
///
/// It sleeps for real, because the thing under test is a *duration*: the old grace was 5 s and the
/// bound that matters is `hk_pipeline::run::REPLUMB_TIMEOUT`, 30 s. A test that did not outlast
/// five seconds could not tell the two apart.
#[test]
fn a_re_plumb_past_the_old_five_second_grace_still_answers_503() {
    let registry = StreamRegistry::new();
    let first = Publisher::new(
        spectrum_header("spectrum/live", ContentClass::Unrestricted, 30.0, 16),
        PublisherConfig::default(),
    )
    .unwrap();
    registry.register(first.header(), first.handle());
    let server = serve(&registry);
    let addr = server.local_addr();

    first.finish_between_windows();
    // Longer than the grace was, shorter than a measured re-plumb. Meanwhile the sweep's burst
    // chains keep registering, as they do on the live radio.
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(6500) {
        let h = bits_header(&format!(
            "bits/fsk-bursts/{}",
            started.elapsed().as_millis()
        ));
        let p = Publisher::new(h.clone(), PublisherConfig::default()).unwrap();
        registry.register(&h, p.handle());
        p.finish();
        thread::sleep(Duration::from_millis(100));
    }

    let status = status_of(authed(addr, "spectrum/live"));
    assert_eq!(
        status, 503,
        "a run that is still capturing, mid-re-plumb, answered {status}: 404 means the registry \
         withdrew the id and 410 means the stream was called finished. Both are read as a dead \
         server by ops/stage.sh, which restarts the demo under the user."
    );

    // The successor arrives and the stream is simply there again — no client ever had to be told
    // anything had happened.
    let mut header = spectrum_header("spectrum/live", ContentClass::Unrestricted, 30.0, 16);
    header.center_hz = Some(34.3e6);
    let second = Publisher::new(header, PublisherConfig::default()).unwrap();
    registry.register(second.header(), second.handle());
    let mut ws = authed(addr, "spectrum/live").expect("the successor is offered");
    assert_eq!(header_of(&ws.read().unwrap()).center_hz, Some(34.3e6));
}
