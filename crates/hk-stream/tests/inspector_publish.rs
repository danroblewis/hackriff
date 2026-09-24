//! T-089: inspector frame records through the publisher (stream-contract §14.2, §14.5): content
//! (bytes and layers) flows when the class permits it and is withheld otherwise, with metadata
//! reduced to the recipe's allowlist; status/edit records are metadata-only; and the published
//! byte stream is itself a recorded decoded stream that `RecordedFrames` reads back (§14.7).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use hk_model::{ContentClass, CrcStatus, Timestamp};
use hk_stream::inspector::{
    FitStatus, FrameContent, FrameMetadata, FrameRecord, INSPECTOR_MESSAGE_SCHEMA,
    InspectorProfile, InspectorRecordType, InspectorSource, LayerNode, LayerTree, NodeType,
    RecordedFrames, byte_span,
};
use hk_stream::{Declared, Publisher, PublisherConfig, StreamError, StreamHeader, StreamKind};
use serde_json::json;

const SECRET_HEX: &str = "c0ffee5ec2e7";

#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn header(class: ContentClass) -> StreamHeader {
    let mut h = StreamHeader::new(
        "inspector/p1/frames",
        StreamKind::Messages,
        class,
        "hk-pipeline:recipe:trial@1",
    );
    h.message_schema = Some(INSPECTOR_MESSAGE_SCHEMA.into());
    h.inspector = Some(InspectorProfile {
        pipeline_id: "p1".into(),
        recipe_id: "trial".into(),
        recipe_version: 1,
        output_id: "frames".into(),
        source: InspectorSource::Live,
        channels: Vec::new(),
    });
    h
}

fn record(class: ContentClass) -> FrameRecord {
    let mut layers = LayerTree {
        nodes: vec![LayerNode {
            id: 0,
            parent: None,
            name: "addr".into(),
            path: "addr".into(),
            ty: NodeType::Uint,
            bits: [0, 16],
            bytes: byte_span(0, 16),
            value: Some(json!(0xc0ff)),
            text: Some("0xC0FF".into()),
            label: None,
            error: false,
        }],
        fit: FitStatus::Ok,
        ..Default::default()
    };
    layers.index_bytes(6);
    FrameRecord {
        record_type: "frame".into(),
        seq: 999,
        t: 1_789_300_800_000_000_000,
        content_class: class,
        gated: false,
        crc_status: Some(CrcStatus::Valid),
        decoder: Some("recipe:trial@1".into()),
        frame_model: Some("trial".into()),
        emitter_id: None,
        metadata: FrameMetadata {
            frame: Some(4),
            sample_index: Some(123),
            bit_len: Some(48),
            recipe_version: Some(1),
            edit_rev: Some(0),
            fit: Some(FitStatus::Ok),
            ..Default::default()
        },
        content: Some(FrameContent {
            hex: SECRET_HEX.into(),
            layers: Some(layers),
        }),
    }
}

/// Publishes `records` (plus a status record) and returns the bytes a consumer received.
fn publish(p: Publisher, records: &[FrameRecord]) -> Vec<u8> {
    let mut p = p;
    let h = p.handle();
    let buf = SharedBuf::default();
    h.subscribe("mem", Declared::local(buf.clone()), Box::new(|_| {}))
        .unwrap();
    for r in records {
        p.publish_frame(r).unwrap();
    }
    p.publish_record(
        InspectorRecordType::Status,
        Timestamp::from_unix_nanos(1),
        ContentClass::Unrestricted,
        &json!({"crc.error_rate": 0.01, "sync.lock": "locked"}),
    )
    .unwrap();
    drop(p);
    assert!(h.wait_closed(Duration::from_secs(5)));
    buf.0.lock().unwrap().clone()
}

#[test]
fn unrestricted_frames_carry_bytes_and_layers_and_read_back_as_a_recording() {
    let p = Publisher::new(
        header(ContentClass::Unrestricted),
        PublisherConfig::default(),
    )
    .unwrap();
    let rec = record(ContentClass::Unrestricted);
    let bytes = publish(p, &[rec.clone(), rec.clone()]);

    let mut r = RecordedFrames::open(&bytes[..]).unwrap();
    assert_eq!(
        r.header().version,
        format!("1.{}", hk_stream::STREAM_VERSION_MINOR)
    );
    assert_eq!(r.header().inspector.as_ref().unwrap().recipe_id, "trial");
    let first = r.next_frame().unwrap().unwrap();
    let second = r.next_frame().unwrap().unwrap();
    assert!(r.next_frame().unwrap().is_none());
    assert_eq!(r.skipped(), 1, "the status record");
    assert_eq!((first.seq, second.seq), (0, 1), "the publisher assigns seq");
    assert!(!first.gated);
    assert_eq!(first.content, rec.content);
    assert_eq!(first.metadata, rec.metadata);
    assert_eq!(first.decoder.as_deref(), Some("recipe:trial@1"));
}

#[test]
fn status_records_must_be_metadata_and_frames_need_a_messages_stream() {
    let mut p = Publisher::new(
        header(ContentClass::Unrestricted),
        PublisherConfig::default(),
    )
    .unwrap();
    let err = p
        .publish_record(
            InspectorRecordType::Edit,
            Timestamp::from_unix_nanos(0),
            ContentClass::Unrestricted,
            &json!({"note": {"nested": "text"}}),
        )
        .unwrap_err();
    assert!(matches!(err, StreamError::NotMetadata), "{err:?}");

    let mut bits = StreamHeader::new("b", StreamKind::Bits, ContentClass::Unrestricted, "t");
    bits.datatype = Some("ru8".into());
    let mut p = Publisher::new(bits, PublisherConfig::default()).unwrap();
    assert!(matches!(
        p.publish_frame(&record(ContentClass::Unrestricted)),
        Err(StreamError::WrongKind { .. })
    ));
}
