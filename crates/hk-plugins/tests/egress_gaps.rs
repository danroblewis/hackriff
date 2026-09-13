//! Regressions for the T-016/T-014 independent re-probe, plugin side:
//! - N1: content encoded into allowlisted typed fields (hex text, packed integer, numeric page in
//!   capcode, `sample_index` offset, confidence digits) through the real host, database and wire;
//! - manifest defaults (hex/digits `max_len` 8, `review_note`, warnings, the example paging
//!   policy);
//! - Ingest strips restricted rows from an in-process producer that skipped the policy.

use std::io::Read;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hk_model::sigmf::Datatype;
use hk_model::{
    AnnotationTarget, ContentClass, CrcStatus, Decode, DecodeId, FreqRange, Region, Repository,
    SampleTime, TimeRange, Timestamp,
};
use hk_plugins::{
    EXAMPLE_RESTRICTED_PAGING_OUTPUT, Ingest, InputStreamDesc, ManifestError, PluginContext,
    PluginInstance, PluginManifest, PluginState,
};
use hk_stream::{
    BinaryRecord, ListenAddr, Listener, Publisher, PublisherConfig, Record, RecordFlags,
    StreamHeader, StreamKind, StreamReader,
};
use serde_json::{Value, json};

const RATE: f64 = 250_000.0;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hke{tag}{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

fn manifest_value(output: Value, script: &Path) -> Value {
    json!({
        "manifest_version": 1, "id": "covert", "version": "1", "licence": "MIT",
        // The script is an argument, so it needs no exec bit (no ETXTBSY window).
        "executable": "/bin/sh", "args": [script.display().to_string()],
        "input": {"kind": "channel", "datatype": "cf32_le", "framing": "raw"},
        "output": output,
        "restart": {"backoff_initial_ms": 10, "backoff_max_ms": 100, "max_restarts": 3, "window_s": 60}
    })
}

/// The probe's N1 output plus a hex key and hex identity at the (new) default length.
fn covert_output() -> Value {
    let mut output: Value = serde_json::from_str(EXAMPLE_RESTRICTED_PAGING_OUTPUT).unwrap();
    output["metadata_keys"]["addr"] = json!({"type": "hex"});
    output["identity"] = json!({"scheme": "other:pocsag-capcode", "charset": "hex"});
    output
}

/// The probe's original N1 manifest (64-character hex key and identity, no review note) is now
/// refused at load; reviewed long strings and integer keys load with warnings; the shipped paging
/// example has none.
#[test]
fn n1_manifest_defaults_refuse_long_unreviewed_strings() {
    let script = Path::new("/nonexistent.sh");
    let probe = json!({"schema_id":"hackriff.pocsag/1","content_class":"restricted-paging",
        "metadata_keys":{"addr":{"type":"hex","max_len":64},"function":{"type":"integer"},
                         "capcode":{"type":"digits","max_len":7},"encoding":{"type":"enum","values":["alpha","numeric"]}},
        "frame_models":["pocsag"],"labels":["pocsag"],
        "identity":{"scheme":"other:pocsag-capcode","charset":"hex","max_len":64}});
    let err = PluginManifest::from_json_str(&manifest_value(probe.clone(), script).to_string())
        .expect_err("64-char hex without review_note");
    assert!(
        matches!(
            err,
            ManifestError::Invalid {
                field: "output.metadata_keys",
                ref reason
            } if reason.contains("review_note")
        ),
        "{err:?}"
    );
    let mut reviewed = probe;
    reviewed["metadata_keys"]["addr"]["review_note"] = json!("64-bit address, reviewed");
    let err = PluginManifest::from_json_str(&manifest_value(reviewed.clone(), script).to_string())
        .expect_err("identity still unreviewed");
    assert!(matches!(
        err,
        ManifestError::Invalid {
            field: "output.identity",
            ..
        }
    ));
    reviewed["identity"]["review_note"] = json!("reviewed");
    let m = PluginManifest::from_json_str(&manifest_value(reviewed, script).to_string()).unwrap();
    assert_eq!(m.warnings.len(), 3, "{:?}", m.warnings);

    let m = PluginManifest::from_json_str(&manifest_value(covert_output(), script).to_string())
        .unwrap();
    assert!(m.warnings.is_empty(), "{:?}", m.warnings);
}

/// N1 through the real host: a restricted-paging plugin tries each covert vector. Hex text
/// longer than 8 is dropped (metadata and identity); a packed integer in the `function` enum is
/// dropped; a 10-digit numeric page in `capcode` is dropped (a page of at most 8 digits fits the
/// capcode type: that residual is bounded by the type, documented); an out-of-range
/// `sample_index` drops the line (counted); confidence is rounded to 0.01. Nothing reaches the
/// database file or the TCP republish.
#[test]
fn n1_covert_vectors_are_stripped_dropped_or_bounded_through_the_host() {
    let dir = temp_dir("n1");
    let hex_text: String = "PAGE TEXT HELLO"
        .bytes()
        .map(|b| format!("{b:02x}"))
        .collect();
    let packed = u64::from_be_bytes(*b"PAGETEXT");
    let lines = [
        // Hex text in a hex key and identity; packed integer in the function enum.
        json!({"type":"decode","frame_model":"pocsag",
               "identity":{"scheme":"other:pocsag-capcode","value":hex_text},
               "metadata":{"addr":hex_text,"function":packed,"capcode":"5551234","encoding":"alpha","baud":"1200"}}),
        // A 10-digit numeric page in the capcode.
        json!({"type":"decode","metadata":{"capcode":"5551234567","function":"2"}}),
        // "PAGE" as a sample index: far outside the input offered.
        json!({"type":"decode","sample_index":u32::from_be_bytes(*b"PAGE"),"metadata":{"function":"1"}}),
        // Control: in range (inside the first record, which is all the script waits for),
        // allowlisted values and identity survive.
        json!({"type":"decode","sample_index":8,"identity":{"scheme":"other:pocsag-capcode","value":"a1b2c3d4"},
               "metadata":{"capcode":"1234567","function":"3"}}),
        // Confidence digits and a packed integer in an annotation.
        json!({"type":"annotation","value":"pocsag","confidence":0.80657169,"metadata":{"function":packed}}),
    ];
    let mut body = String::from("head -c 64 >/dev/null\n");
    for line in &lines {
        body.push_str(&format!("echo '{line}'\n"));
    }
    body.push_str("exec cat >/dev/null\n");
    let script = dir.join("covert.sh");
    std::fs::write(&script, body).unwrap();
    let m = PluginManifest::from_json_str(&manifest_value(covert_output(), &script).to_string())
        .unwrap();

    // Republisher: unrestricted TCP stream carrying the manifest's policy.
    let mut header = StreamHeader::new(
        "decodes/pager",
        StreamKind::Messages,
        ContentClass::Unrestricted,
        "n1",
    );
    header.max_frame_len = 64 * 1024;
    header.message_schema = Some(m.output.schema_id.clone());
    let publisher = Publisher::with_metadata_policy(
        header,
        PublisherConfig {
            queue_bytes: 1024 * 1024,
            ..PublisherConfig::default()
        },
        m.output.metadata_policy.clone().unwrap(),
    )
    .unwrap();
    let handle = publisher.handle();
    let listener = Listener::bind_tcp("127.0.0.1:0", handle.clone()).unwrap();
    let ListenAddr::Tcp(addr) = listener.addr().clone() else {
        unreachable!()
    };
    let consumer = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = TcpStream::connect(addr).unwrap().read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while handle.open_consumers() == 0 {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }

    let region = Region {
        freq: FreqRange {
            lo_hz: 151.9e6,
            hi_hz: 152.1e6,
        },
        time: TimeRange {
            start: Timestamp::from_unix_nanos(0),
            end: Timestamp::from_unix_nanos(i64::MAX / 2),
        },
    };
    let sink = Arc::new(Mutex::new(Ingest::with_republish(
        Repository::open(dir.join("hk.sqlite")).unwrap(),
        publisher,
    )));
    let input = InputStreamDesc {
        datatype: Datatype::Cf32Le,
        sample_rate_hz: RATE,
        center_hz: Some(152e6),
        bandwidth_hz: Some(25e3),
        content_class: ContentClass::RestrictedPaging,
        anchor: SampleTime {
            sample_index: 0,
            host_time: Timestamp::now(),
        },
        emitter_id: None,
        provenance_ref: None,
    };
    let context = PluginContext {
        region: Some(region),
        ..PluginContext::default()
    };
    let mut inst = PluginInstance::spawn(m, input, context, Arc::clone(&sink)).unwrap();
    let mon = inst.monitor();
    assert!(mon.wait_for(Duration::from_secs(10), |s| s.state == PluginState::Running));
    // 4 records of 8 cf32 samples at sample indices 0, 8, 16, 24: input range 0..=32.
    for i in 0..4u64 {
        inst.push(BinaryRecord {
            t: Timestamp::now(),
            sample_index: i * 8,
            flags: RecordFlags::empty(),
            payload: &[0u8; 64],
        })
        .unwrap();
    }
    assert!(
        mon.wait_for(Duration::from_secs(10), |s| s.decodes == 3
            && s.annotations == 1
            && s.sample_index_out_of_range == 1),
        "{:?} {:?}",
        mon.stats(),
        mon.log_tail()
    );
    let stats = mon.stats();
    drop(mon);
    inst.shutdown();
    let mut ingest = Arc::try_unwrap(sink)
        .ok()
        .expect("host released the ingest")
        .into_inner()
        .unwrap();
    drop(ingest.take_publisher());
    let wire = consumer.join().unwrap();
    drop(listener);

    assert_eq!(stats.malformed, 0);
    assert_eq!(
        ingest.stats().rows_stripped,
        0,
        "host output is policy-shaped"
    );
    let mut reader = StreamReader::new(&wire[..]);
    let mut decodes = Vec::new();
    while let Some(r) = reader.next_record().unwrap() {
        if let Record::Message(msg) = r
            && let Some(id) = msg.value["decode_id"].as_str()
        {
            decodes.push(
                ingest
                    .repo()
                    .decode(id.parse::<DecodeId>().unwrap())
                    .unwrap(),
            );
        }
    }
    assert_eq!(decodes.len(), 3);
    let metas: Vec<&Value> = decodes.iter().map(|d| &d.metadata).collect();
    assert!(
        metas.contains(&&json!({"capcode": "5551234", "encoding": "alpha", "baud": "1200"})),
        "hex text and packed integer dropped; bounded capcode kept: {metas:?}"
    );
    assert!(metas.contains(&&json!({"function": "2"})), "{metas:?}");
    assert!(
        metas.contains(&&json!({"capcode": "1234567", "function": "3"})),
        "{metas:?}"
    );
    let identities: Vec<_> = decodes.iter().filter_map(|d| d.identity.as_ref()).collect();
    assert_eq!(identities.len(), 1);
    assert_eq!(identities[0].value, "a1b2c3d4");
    let annotations = ingest
        .repo()
        .annotations_for(&AnnotationTarget::Region(region))
        .unwrap();
    assert_eq!(annotations.len(), 1);
    assert_eq!(annotations[0].confidence, 0.81);
    assert_eq!(annotations[0].metadata, json!({}));

    ingest.repo_mut().checkpoint().unwrap();
    drop(ingest);
    let mut db = std::fs::read(dir.join("hk.sqlite")).unwrap();
    if let Ok(wal) = std::fs::read(dir.join("hk.sqlite-wal")) {
        db.extend(wal);
    }
    for (what, token) in [
        ("hex text", hex_text.as_str()),
        ("packed integer", &packed.to_string()),
        ("numeric page", "5551234567"),
        ("confidence digits", "0.80657169"),
        ("sample index", &u32::from_be_bytes(*b"PAGE").to_string()),
    ] {
        assert!(!contains(&db, token), "{what} persisted");
        assert!(!contains(&wire, token), "{what} streamed");
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// An in-process producer stores a restricted Decode with free-text metadata without calling the
/// policy: Ingest reduces it to the empty allowlist and counts it; a policy-shaped row is stored
/// as given.
#[test]
fn ingest_strips_restricted_rows_that_skipped_the_policy() {
    let mut ingest = Ingest::new(Repository::open_in_memory().unwrap());
    let row = |metadata: Value, frame_model: &str| Decode {
        id: DecodeId::new(),
        demodulation_ref: None,
        recording_ref: None,
        decoder_id: "in-process".into(),
        decoder_version: "1".into(),
        frame_model: frame_model.into(),
        metadata,
        content: None,
        crc_status: CrcStatus::Unknown,
        identity: None,
        content_class: ContentClass::RestrictedPaging,
        t: Timestamp::now(),
    };
    let raw = row(
        json!({"text": "hello pager text", "function": 2}),
        "free text model",
    );
    let raw_id = raw.id;
    ingest.store_decode(raw, None, None).unwrap();
    let stored = ingest.repo().decode(raw_id).unwrap();
    assert_eq!(stored.metadata, json!({}));
    assert_eq!(stored.frame_model, "hackriff.unsanitized/1");
    assert_eq!(ingest.stats().rows_stripped, 1);

    let clean = row(json!({"function": 2, "capcode": "1234567"}), "pocsag");
    let clean_id = clean.id;
    ingest.store_decode(clean, None, None).unwrap();
    let stored = ingest.repo().decode(clean_id).unwrap();
    assert_eq!(
        stored.metadata,
        json!({"function": 2, "capcode": "1234567"})
    );
    assert_eq!(ingest.stats().rows_stripped, 1);

    // Unrestricted rows are never touched.
    let mut open = row(json!({"text": "hello"}), "free text model");
    open.content_class = ContentClass::Unrestricted;
    let open_id = open.id;
    ingest.store_decode(open, None, None).unwrap();
    assert_eq!(
        ingest.repo().decode(open_id).unwrap().metadata,
        json!({"text": "hello"})
    );
    assert_eq!(ingest.stats().rows_stripped, 1);
}
