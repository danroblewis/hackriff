//! T-089 inspector routes at the HTTP boundary over an in-memory capture store: re-parse a
//! recorded decoded stream (the §3 byte stream, stream-contract §14.7) with a draft field map
//! (paged frames with layer trees and byte ranges, a fit summary over the whole recording,
//! nothing saved), gated records never parsed or served, and the documented refusals.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use hk_api::stream::frame::encode_frame;
use hk_api::stream::inspector::{
    CaptureSource, FitStatus, FrameContent, FrameMetadata, FrameRecord, INSPECTOR_MESSAGE_SCHEMA,
    InspectorProfile, InspectorSource, to_hex,
};
use hk_api::stream::{StreamHeader, StreamKind};
use hk_api::{ApiState, Server, ServerConfig, Token};
use hk_model::{ContentClass, CrcStatus};
use serde_json::{Value, json};

const TOKEN: &str = "t089-inspector-token-0123456789abcdef";

struct MemCaptures(BTreeMap<String, Vec<u8>>);

impl CaptureSource for MemCaptures {
    fn open(&self, id: &str) -> std::io::Result<Option<Box<dyn Read + Send>>> {
        Ok(self
            .0
            .get(id)
            .map(|b| Box::new(std::io::Cursor::new(b.clone())) as Box<dyn Read + Send>))
    }
}

/// A 16-bit record: `kind` (4 bits), `len` (4 bits), then `len` payload bytes.
fn frame(i: u64, class: ContentClass, bytes: &[u8]) -> FrameRecord {
    let gated = !class.permits_content();
    FrameRecord {
        record_type: "frame".into(),
        seq: i,
        t: 1_789_300_800_000_000_000 + i as i64,
        content_class: class,
        gated,
        crc_status: Some(CrcStatus::Valid),
        decoder: Some("recipe:trial@3".into()),
        frame_model: Some("trial".into()),
        emitter_id: None,
        metadata: FrameMetadata {
            frame: Some(i),
            bit_len: Some(bytes.len() as u32 * 8),
            recipe_version: Some(3),
            edit_rev: Some(2),
            ..Default::default()
        },
        content: (!gated).then(|| FrameContent {
            hex: to_hex(bytes),
            layers: None,
        }),
    }
}

fn recording(stream_class: ContentClass, records: &[FrameRecord]) -> Vec<u8> {
    let mut h = StreamHeader::new(
        "inspector/p7/frames",
        StreamKind::Messages,
        stream_class,
        "hk-pipeline:recipe:trial@3",
    );
    h.message_schema = Some(INSPECTOR_MESSAGE_SCHEMA.into());
    h.inspector = Some(InspectorProfile {
        pipeline_id: "p7".into(),
        recipe_id: "trial".into(),
        recipe_version: 3,
        output_id: "frames".into(),
        source: InspectorSource::Live,
        channels: Vec::new(),
    });
    let mut out = Vec::new();
    encode_frame(&mut out, &h.to_json_bytes().unwrap(), h.max_frame_len).unwrap();
    for r in records {
        encode_frame(&mut out, &serde_json::to_vec(r).unwrap(), h.max_frame_len).unwrap();
    }
    out
}

fn serve(captures: Option<MemCaptures>) -> Server {
    let config = ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        Token::from_config(TOKEN).unwrap(),
    );
    let state = ApiState {
        captures: captures.map(|c| Arc::new(c) as Arc<dyn CaptureSource>),
        ..ApiState::default()
    };
    Server::start(config, state).unwrap()
}

fn call(addr: SocketAddr, method: &str, path: &str, auth: bool, body: &str) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let auth = if auth {
        format!("Authorization: Bearer {TOKEN}\r\n")
    } else {
        String::new()
    };
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: t\r\n{auth}Content-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let status = raw[9..12].parse().unwrap();
    let body = raw.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

fn post(addr: SocketAddr, path: &str, body: Value) -> (u16, Value) {
    call(addr, "POST", path, true, &body.to_string())
}

fn draft_map() -> Value {
    json!({
        "unit": "bits",
        "fields": [
            {"name": "kind", "type": "enum", "length": 4, "values": {"1": "hello", "2": "data"}},
            {"name": "len", "type": "uint", "length": 4},
            {"name": "payload", "type": "ascii", "length": {"field": "len", "scale": 8}}
        ]
    })
}

fn captures() -> MemCaptures {
    let mut records = Vec::new();
    for i in 0..25u64 {
        let text = format!("f{i:02}");
        let mut bytes = vec![0x10 | text.len() as u8];
        bytes.extend(text.bytes());
        if i == 7 {
            bytes[0] = 0x2f; // claims 15 payload bytes: out of bounds
        }
        records.push(frame(i, ContentClass::Unrestricted, &bytes));
    }
    // A restricted record stored gated (metadata only), and one wrongly stored with content.
    records.push(frame(25, ContentClass::RestrictedPaging, &[]));
    let mut leaked = frame(26, ContentClass::Unrestricted, &[0x13, b's', b'e', b'c']);
    leaked.content_class = ContentClass::RestrictedPaging;
    records.push(leaked);
    MemCaptures(BTreeMap::from([
        (
            "cap-1".to_owned(),
            recording(ContentClass::Unrestricted, &records),
        ),
        ("garbage".to_owned(), b"not a stream".to_vec()),
    ]))
}

#[test]
fn capture_reparse_pages_frames_with_layers_and_summarises_fit_over_the_whole_recording() {
    let server = serve(Some(captures()));
    let addr = server.local_addr();

    let (st, v) = post(
        addr,
        "/api/captures/cap-1/parse",
        json!({"field_map": draft_map(), "from_frame": 5, "limit": 3}),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["capture_id"], "cap-1");
    assert_eq!(v["total_frames"], 27);
    assert_eq!(
        (v["from_frame"].clone(), v["limit"].clone()),
        (json!(5), json!(3))
    );
    assert_eq!(v["next_from_frame"], 8);
    assert_eq!(v["stream"]["content_class"], "unrestricted");
    assert_eq!(
        v["stream"]["inspector"]["source"],
        json!({"kind": "capture", "capture_id": "cap-1", "reparse": true})
    );
    let frames = v["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 3);
    let f5 = &frames[0];
    assert_eq!(f5["metadata"]["frame"], 5);
    assert_eq!(
        f5["metadata"]["recipe_version"], 3,
        "the recording's revision"
    );
    assert_eq!(f5["metadata"]["edit_rev"], 2);
    assert_eq!(f5["metadata"]["fit"], "ok");
    let layers = &f5["content"]["layers"];
    let nodes = layers["nodes"].as_array().unwrap();
    let payload = nodes.iter().find(|n| n["path"] == "payload").unwrap();
    assert_eq!(payload["value"], "f05");
    assert_eq!(payload["bits"], json!([8, 24]));
    assert_eq!(payload["bytes"], json!([1, 4]));
    let kind = nodes.iter().find(|n| n["path"] == "kind").unwrap();
    assert_eq!(kind["text"], "hello");
    assert_eq!(layers["byte_index"][0], json!([0, 1]));
    assert_eq!(layers["byte_index"][2], json!([payload["id"]]));
    // Frame 7 does not fit; later fields are still tried and the frame is partial.
    assert_eq!(frames[2]["metadata"]["fit"], "partial");
    assert_eq!(
        frames[2]["content"]["layers"]["errors"][0]["kind"],
        "out-of-bounds"
    );

    // The summary covers all 27 frames, not the page.
    assert_eq!(
        v["fit"],
        json!({"frames": 27, "ok": 24, "partial": 1, "failed": 0, "unparsed": 2,
               "errors": {"payload": {"out-of-bounds": 1}}, "truncated": false})
    );

    // Last page: gated records served metadata-only, never parsed.
    let (st, v) = post(
        addr,
        "/api/captures/cap-1/parse",
        json!({"field_map": draft_map(), "from_frame": 25}),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["next_from_frame"], Value::Null);
    for f in v["frames"].as_array().unwrap() {
        assert_eq!(f["gated"], true, "{f}");
        assert!(f.get("content").is_none(), "{f}");
    }
    assert!(
        !v.to_string().contains("736563"),
        "restricted bytes never served"
    );

    // Nothing was saved: the plain frame list has stored records without layers.
    let (st, v) = post(addr, "/api/captures/cap-1/parse", json!({"limit": 2}));
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["fit"], Value::Null);
    assert_eq!(v["frames"][1]["content"]["hex"], "13663031");
    assert!(v["frames"][1]["content"].get("layers").is_none());
    assert_eq!(v["stream"]["inspector"]["source"]["reparse"], false);
}

#[test]
fn inspector_routes_refuse_as_documented() {
    let server = serve(Some(captures()));
    let addr = server.local_addr();
    let body = json!({"field_map": draft_map()});

    let (st, v) = post(addr, "/api/captures/none/parse", body.clone());
    assert_eq!((st, v["code"].as_str()), (404, Some("not_found")), "{v}");
    let (st, v) = post(addr, "/api/captures/garbage/parse", body.clone());
    assert_eq!((st, v["code"].as_str()), (422, Some("unreadable")), "{v}");
    let (st, v) = post(addr, "/api/captures/cap-1/parse", json!({"limit": 501}));
    assert_eq!((st, v["code"].as_str()), (400, Some("invalid")), "{v}");
    let (st, v) = post(addr, "/api/captures/cap-1/parse", json!({"extra": 1}));
    assert_eq!(st, 400, "{v}");

    // An invalid draft map: errors by dotted path, the value never echoed.
    let bad = json!({"field_map": {"unit": "bits", "fields": [
        {"name": "hdr", "type": "layer", "length": 4, "fields": [
            {"name": "wide", "type": "uint", "length": 9}]}]}});
    let (st, v) = post(addr, "/api/captures/cap-1/parse", bad);
    assert_eq!(st, 400, "{v}");
    assert_eq!(v["errors"][0]["path"], "hdr.wide");
    let (st, v) = post(
        addr,
        "/api/inspector/parse",
        json!({"field_map": {"fields": [], "secret_key": "VALUE-7781"}, "frames": [{"hex": "00"}]}),
    );
    assert_eq!(st, 400, "{v}");
    assert!(!v.to_string().contains("VALUE-7781"));

    let (st, _) = call(addr, "POST", "/api/captures/cap-1/parse", false, "{}");
    assert_eq!(st, 401);
    let (st, v) = call(addr, "GET", "/api/inspector/parse", true, "");
    assert_eq!(st, 405, "{v}");

    let bare = serve(None);
    let (st, v) = post(bare.local_addr(), "/api/captures/cap-1/parse", body);
    assert_eq!((st, v["code"].as_str()), (503, Some("unavailable")), "{v}");
}

#[test]
fn inspector_parse_evaluates_submitted_frames() {
    let server = serve(None);
    let (st, v) = post(
        server.local_addr(),
        "/api/inspector/parse",
        json!({"field_map": draft_map(), "frames": [
            {"hex": "1348494A"}, {"hex": "2F", "bit_len": 8}, {"hex": "10", "bit_len": 4}
        ]}),
    );
    assert_eq!(st, 200, "{v}");
    let frames = v["frames"].as_array().unwrap();
    assert_eq!(frames[0]["hex"], "1348494a");
    assert_eq!(frames[0]["layers"]["fit"], "ok");
    assert_eq!(frames[1]["layers"]["fit"], "partial");
    assert_eq!(frames[2]["bit_len"], 4);
    assert_eq!(
        frames[2]["layers"]["fit"],
        serde_json::to_value(FitStatus::Partial).unwrap()
    );
    assert_eq!(v["fit"]["frames"], 3);
    let (st, _) = post(
        server.local_addr(),
        "/api/inspector/parse",
        json!({"field_map": draft_map(), "frames": [{"hex": "10", "bit_len": 9}]}),
    );
    assert_eq!(st, 400);
}
