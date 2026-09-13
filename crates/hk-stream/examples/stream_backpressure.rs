//! Producer throughput with one stuck consumer (never reads) and one fast consumer, over Unix
//! domain sockets. `cargo run --release -p hk-stream --example stream_backpressure`.

use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use hk_model::{ContentClass, CrcStatus, Timestamp};
use hk_stream::{
    BinaryRecord, Listener, MessageRecord, Publisher, PublisherConfig, Record, RecordFlags,
    StreamHeader, StreamKind, StreamReader,
};

fn run(kind: StreamKind, records: u64, payload_len: usize) {
    let dir = std::env::temp_dir().join(format!("hk-bp-example-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{}.sock", kind.as_str()));
    let mut header = StreamHeader::new("bench", kind, ContentClass::Unrestricted, "example");
    header.datatype = Some("ci8".into());
    header.sample_rate_hz = Some(2e6);
    header.max_frame_len = 65_536;
    let config = PublisherConfig {
        queue_bytes: 4 * 1024 * 1024,
        disconnect_after_drops: u64::MAX,
        disconnect_after: Duration::from_secs(3600),
        ..PublisherConfig::default()
    };
    let mut publisher = Publisher::new(header, config).unwrap();
    let handle = publisher.handle();
    let listener = Listener::bind_uds(&path, handle.clone()).unwrap();
    let stuck = UnixStream::connect(&path).unwrap();
    let fast_path = path.clone();
    let fast = thread::spawn(move || {
        let mut r = StreamReader::connect_uds(&fast_path).unwrap();
        let (mut n, mut dropped) = (0u64, 0u64);
        while let Some(rec) = r.next_record().unwrap() {
            match rec {
                Record::Dropped(d) => dropped += d.count,
                _ => n += 1,
            }
        }
        (n, dropped)
    });
    while handle.open_consumers() < 2 {
        thread::sleep(Duration::from_millis(1));
    }
    let payload = vec![0u8; payload_len];
    let message = MessageRecord {
        t: Timestamp::now(),
        emitter_id: None,
        provenance_ref: None,
        content_class: ContentClass::Unrestricted,
        decode_id: None,
        annotation_id: None,
        decoder: Some("bench@0".into()),
        frame_model: Some("adsb-df17".into()),
        crc_status: Some(CrcStatus::Valid),
        identity: None,
        metadata: serde_json::json!({"icao": "a1b2c3", "df": 17}),
        content: None,
    };
    let start = Instant::now();
    for i in 0..records {
        if kind == StreamKind::Messages {
            publisher.publish_message(&message).unwrap();
        } else {
            publisher
                .publish_binary(BinaryRecord {
                    t: Timestamp::from_unix_nanos(i as i64),
                    sample_index: i,
                    flags: RecordFlags::empty(),
                    payload: &payload,
                })
                .unwrap();
        }
    }
    let elapsed = start.elapsed();
    let stats = handle.consumer_stats();
    drop(publisher);
    let (received, fast_dropped) = fast.join().unwrap();
    drop(stuck);
    drop(listener);
    let stuck_stats = stats.iter().find(|s| s.records_dropped > 0);
    println!(
        "{:>8} payload {:>5} B: {records} records in {:.3} s = {:.0} records/s ({:.1} MB/s); fast consumer got {received}, dropped {fast_dropped}; stuck consumer dropped {}",
        kind.as_str(),
        payload_len,
        elapsed.as_secs_f64(),
        records as f64 / elapsed.as_secs_f64(),
        records as f64 * payload_len as f64 / 1e6 / elapsed.as_secs_f64(),
        stuck_stats.map_or(0, |s| s.records_dropped),
    );
    let _ = std::fs::remove_dir_all(dir);
}

fn main() {
    run(StreamKind::Iq, 1_000_000, 256);
    run(StreamKind::Iq, 200_000, 16_384);
    run(StreamKind::Messages, 500_000, 0);
}
